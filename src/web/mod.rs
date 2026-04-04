//! Web dashboard server: axum HTTP server with embedded SPA.

pub mod auth;
pub mod daemon;
mod routes;

use std::net::SocketAddr;

use anyhow::Result;
use axum::Router;
#[cfg(debug_assertions)]
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::db::Database;
use crate::embedding::{Embedder, Reranker};

/// Shared state for axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: std::sync::Arc<std::sync::Mutex<Database>>,
    pub embedder: std::sync::Arc<dyn Embedder>,
    pub reranker: Option<std::sync::Arc<dyn Reranker>>,
    pub reranker_threshold: f64,
    pub auth_token: String,
    pub mcp_config: crate::mcp::ServerConfig,
}

/// Options for the web server.
pub struct ServeOptions {
    pub open_browser: bool,
}

/// Build the full app Router with all routes, SPA fallback, and security headers.
pub(crate) fn app_router(state: AppState) -> Router {
    let auth_layer =
        axum::middleware::from_fn_with_state(state.clone(), auth::require_bearer_token);
    let mcp_service = build_mcp_service(&state);

    Router::new()
        .nest(
            "/api",
            routes::api_router().layer(auth_layer.clone()),
        )
        .route(
            "/mcp",
            axum::routing::any_service(mcp_service).layer(auth_layer),
        )
        .fallback_service(spa_service())
        .with_state(state)
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::X_FRAME_OPTIONS,
            axum::http::HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::CONTENT_SECURITY_POLICY,
            axum::http::HeaderValue::from_static(
                "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:",
            ),
        ))
}

/// Build the streamable HTTP MCP service for mounting at `/mcp`.
fn build_mcp_service(
    state: &AppState,
) -> rmcp::transport::streamable_http_server::tower::StreamableHttpService<
    crate::mcp::ErinraServer,
    rmcp::transport::streamable_http_server::session::never::NeverSessionManager,
> {
    use rmcp::transport::streamable_http_server::{
        session::never::NeverSessionManager,
        tower::{StreamableHttpServerConfig, StreamableHttpService},
    };
    // Pre-build the server once; the factory just clones it per request.
    // This avoids per-request DB mutex acquisition and taxonomy queries
    // that ErinraServer::new() performs to cache instructions.
    let server = crate::mcp::ErinraServer::new(
        state.db.clone(),
        state.embedder.clone(),
        state.reranker.clone(),
        state.mcp_config.clone(),
    );

    // Stateless mode: no sessions, plain JSON responses (no SSE framing).
    let config_http = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_json_response(true);

    StreamableHttpService::new(
        move || Ok(server.clone()),
        std::sync::Arc::new(NeverSessionManager::default()),
        config_http,
    )
}

/// Start the web server and block until shutdown.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    db: Database,
    embedder: std::sync::Arc<dyn Embedder>,
    reranker: Option<std::sync::Arc<dyn Reranker>>,
    reranker_threshold: f64,
    auth_token: String,
    mcp_config: crate::mcp::ServerConfig,
    addr: SocketAddr,
    opts: ServeOptions,
) -> Result<()> {
    // Save token ref before moving into AppState, for use in browser URL.
    let browser_token = if opts.open_browser {
        Some(auth_token.clone())
    } else {
        None
    };

    let state = AppState {
        db: std::sync::Arc::new(std::sync::Mutex::new(db)),
        embedder,
        reranker,
        reranker_threshold,
        auth_token,
        mcp_config,
    };

    let app = app_router(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    eprintln!("Erinra dashboard: http://{local_addr}");

    if let Some(token) = browser_token {
        let url = format!("http://{local_addr}?token={token}");
        if let Err(e) = open::that(url) {
            tracing::warn!("failed to open browser: {e}");
        }
    }

    axum::serve(listener, app).await?;
    Ok(())
}

/// Serve the embedded SPA. In release builds, the SPA is compiled into the binary.
/// In debug builds, we serve from the `web/build` directory on disk if it exists,
/// falling back to a simple HTML page if not built yet.
fn spa_service() -> axum::routing::MethodRouter {
    #[cfg(not(debug_assertions))]
    {
        use axum::http::StatusCode;

        use axum::response::IntoResponse;

        static SPA_DIR: include_dir::Dir =
            include_dir::include_dir!("$CARGO_MANIFEST_DIR/web/build");

        axum::routing::get(|uri: axum::http::Uri| async move {
            let path = uri.path().trim_start_matches('/');
            let is_exact = SPA_DIR.get_file(path).is_some();
            let file = if is_exact {
                SPA_DIR.get_file(path)
            } else {
                SPA_DIR.get_file("index.html")
            };
            match file {
                Some(file) => {
                    let content_type = if is_exact && !path.is_empty() {
                        mime_guess::from_path(path)
                            .first_or_text_plain()
                            .to_string()
                    } else {
                        "text/html; charset=utf-8".to_string()
                    };
                    (
                        [(axum::http::header::CONTENT_TYPE, content_type)],
                        file.contents(),
                    )
                        .into_response()
                }
                None => {
                    tracing::error!("SPA index.html missing from embedded assets");
                    (StatusCode::INTERNAL_SERVER_ERROR, "SPA assets missing").into_response()
                }
            }
        })
    }

    #[cfg(debug_assertions)]
    {
        use axum::response::Html;

        let build_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web/build");
        if build_dir.exists() {
            axum::routing::get_service(ServeDir::new(&build_dir).fallback(
                tower_http::services::ServeFile::new(build_dir.join("index.html")),
            ))
        } else {
            axum::routing::get(|| async {
                Html(
                    "<h1>Erinra Dashboard</h1>\
                     <p>SPA not built yet. Run <code>cd web && npm run build</code> first.</p>",
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::embedding::MockEmbedder;

    const TEST_TOKEN: &str = "test-secret-token-1234";

    fn test_app() -> Router {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let state = AppState {
            db: Arc::new(Mutex::new(db)),
            embedder: Arc::new(MockEmbedder::new(768)),
            reranker: None,
            reranker_threshold: 0.0,
            auth_token: TEST_TOKEN.to_string(),
            mcp_config: crate::mcp::ServerConfig::default(),
        };
        app_router(state)
    }

    #[tokio::test]
    async fn mcp_initialize_with_valid_auth_returns_server_info() {
        let app = test_app();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.1"}
            }
        });
        let response = app
            .oneshot(
                Request::post("/mcp")
                    .header("Authorization", format!("Bearer {TEST_TOKEN}"))
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200, "MCP initialize should return 200");
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("response should be valid JSON");
        assert!(
            json["result"]["serverInfo"].is_object(),
            "response should contain serverInfo, got: {json}"
        );
        assert_eq!(json["result"]["serverInfo"]["name"], "erinra");
    }

    /// Helper: send a JSON-RPC request to /mcp with auth.
    async fn mcp_request(app: Router, body: serde_json::Value) -> serde_json::Value {
        let response = app
            .oneshot(
                Request::post("/mcp")
                    .header("Authorization", format!("Bearer {TEST_TOKEN}"))
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "MCP request should return 200");
        let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).expect("response should be valid JSON")
    }

    #[tokio::test]
    async fn mcp_store_and_get_round_trip() {
        // Each stateless request needs its own app (router is consumed by oneshot).
        // The underlying DB/embedder are shared via Arc, so state persists.
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let state = AppState {
            db: Arc::new(Mutex::new(db)),
            embedder: Arc::new(MockEmbedder::new(768)),
            reranker: None,
            reranker_threshold: 0.0,
            auth_token: TEST_TOKEN.to_string(),
            mcp_config: crate::mcp::ServerConfig::default(),
        };

        // Step 1: Initialize (required even in stateless mode).
        let init_body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.1"}
            }
        });
        let init_resp = mcp_request(app_router(state.clone()), init_body).await;
        assert!(
            init_resp["result"]["serverInfo"].is_object(),
            "initialize should return serverInfo"
        );

        // Step 2: Store a memory.
        let store_body = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {
                "name": "store",
                "arguments": {
                    "content": "MCP over HTTP works!",
                    "projects": ["test-project"],
                    "type": "note"
                }
            }
        });
        let store_resp = mcp_request(app_router(state.clone()), store_body).await;
        let store_text = store_resp["result"]["content"][0]["text"]
            .as_str()
            .expect("store should return text content");
        let store_data: serde_json::Value =
            serde_json::from_str(store_text).expect("store text should be JSON");
        let memory_id = store_data["id"]
            .as_str()
            .expect("store should return an id");
        assert!(!memory_id.is_empty());

        // Step 3: Get the memory back by ID.
        let get_body = serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "get",
                "arguments": { "ids": [memory_id] }
            }
        });
        let get_resp = mcp_request(app_router(state.clone()), get_body).await;
        let get_text = get_resp["result"]["content"][0]["text"]
            .as_str()
            .expect("get should return text content");
        let get_data: serde_json::Value =
            serde_json::from_str(get_text).expect("get text should be JSON");
        assert_eq!(get_data[0]["id"].as_str(), Some(memory_id));
        assert_eq!(
            get_data[0]["content"].as_str(),
            Some("MCP over HTTP works!")
        );
        assert_eq!(get_data[0]["projects"][0].as_str(), Some("test-project"));
    }

    #[tokio::test]
    async fn mcp_wrong_content_type_returns_415() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::post("/mcp")
                    .header("Authorization", format!("Bearer {TEST_TOKEN}"))
                    .header("Content-Type", "text/plain")
                    .header("Accept", "application/json, text/event-stream")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            415,
            "wrong Content-Type should return 415 Unsupported Media Type"
        );
    }

    #[tokio::test]
    async fn mcp_coexists_with_existing_routes() {
        let app = test_app();

        // API route still works.
        let api_resp = app
            .oneshot(
                Request::get("/api/discover")
                    .header("Authorization", format!("Bearer {TEST_TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(api_resp.status(), 200, "API discover should still work");
        let content_type = api_resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("application/json"),
            "API should return JSON, got: {content_type}"
        );

        // SPA route still works.
        let app = test_app();
        let spa_resp = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(spa_resp.status(), 200, "SPA root should still work");
        let spa_ct = spa_resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            spa_ct.contains("text/html"),
            "SPA should return HTML, got: {spa_ct}"
        );
    }

    #[tokio::test]
    async fn mcp_without_auth_returns_401() {
        let app = test_app();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.1"}
            }
        });
        let response = app
            .oneshot(
                Request::post("/mcp")
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            401,
            "MCP request without auth should return 401"
        );
    }

    #[tokio::test]
    async fn get_root_returns_html() {
        let app = test_app();
        let response = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let content_type = response
            .headers()
            .get("content-type")
            .expect("should have content-type header")
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("text/html"),
            "expected text/html, got: {content_type}"
        );

        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body_str.contains("<!doctype html>") || body_str.contains("<!DOCTYPE html>"),
            "body should contain HTML doctype, got: {}",
            &body_str[..body_str.len().min(200)]
        );
    }

    #[tokio::test]
    async fn spa_fallback_returns_index_html() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::get("/nonexistent/spa/route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let content_type = response
            .headers()
            .get("content-type")
            .expect("should have content-type header")
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("text/html"),
            "SPA fallback should return HTML, got: {content_type}"
        );

        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body_str.contains("<!doctype html>") || body_str.contains("<!DOCTYPE html>"),
            "SPA fallback body should contain HTML doctype"
        );
    }

    #[tokio::test]
    async fn robots_txt_returns_correct_content() {
        let app = test_app();
        let response = app
            .oneshot(Request::get("/robots.txt").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let content_type = response
            .headers()
            .get("content-type")
            .expect("should have content-type header")
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("text/plain"),
            "robots.txt should be text/plain, got: {content_type}"
        );

        let body = axum::body::to_bytes(response.into_body(), 10_000)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body_str.contains("User-agent"),
            "robots.txt should contain User-agent directive, got: {body_str}"
        );
    }

    #[tokio::test]
    async fn static_assets_get_correct_mime_types() {
        let app = test_app();
        let response = app
            .oneshot(Request::get("/_app/env.js").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let content_type = response
            .headers()
            .get("content-type")
            .expect("should have content-type header")
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("javascript"),
            "env.js should have javascript content-type, got: {content_type}"
        );
    }

    #[tokio::test]
    async fn security_headers_present_on_spa_responses() {
        let app = test_app();
        let response = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let headers = response.headers();

        let xcto = headers
            .get("x-content-type-options")
            .expect("should have x-content-type-options header")
            .to_str()
            .unwrap();
        assert_eq!(xcto, "nosniff");

        let xfo = headers
            .get("x-frame-options")
            .expect("should have x-frame-options header")
            .to_str()
            .unwrap();
        assert_eq!(xfo, "DENY");

        let csp = headers
            .get("content-security-policy")
            .expect("should have content-security-policy header")
            .to_str()
            .unwrap();
        assert_eq!(
            csp,
            "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:"
        );
    }

    #[tokio::test]
    async fn api_routes_take_priority_over_spa_fallback() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::get("/api/discover")
                    .header("Authorization", format!("Bearer {TEST_TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let content_type = response
            .headers()
            .get("content-type")
            .expect("should have content-type header")
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("application/json"),
            "API route should return JSON, got: {content_type}"
        );
    }

    #[tokio::test]
    async fn api_with_valid_bearer_token_returns_200() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::get("/api/discover")
                    .header("Authorization", format!("Bearer {TEST_TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn api_without_bearer_token_returns_401() {
        let app = test_app();
        let response = app
            .oneshot(Request::get("/api/discover").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 401);
    }

    #[tokio::test]
    async fn api_with_wrong_bearer_token_returns_401() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::get("/api/discover")
                    .header("Authorization", "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 401);
    }

    #[tokio::test]
    async fn api_with_malformed_auth_headers_returns_401() {
        let malformed_headers = vec![
            "Basic abc", // wrong scheme
            "Bearer ",   // no value after space
            "",          // empty
            "Bearer",    // no space separator
        ];

        for header_value in malformed_headers {
            let app = test_app();
            let response = app
                .oneshot(
                    Request::get("/api/discover")
                        .header("Authorization", header_value)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(
                response.status(),
                401,
                "Expected 401 for Authorization: '{header_value}'"
            );
        }
    }
}
