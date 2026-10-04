//! Stdio-to-HTTP relay: bridges stdio JSON-RPC to the daemon's `/mcp` HTTP endpoint.

use anyhow::{Context, Result};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::web::daemon::ReadyDaemon;

/// Bridge stdio JSON-RPC to the daemon's `/mcp` HTTP endpoint.
///
/// Reads newline-delimited JSON from `reader`, POSTs each message to the daemon,
/// and writes responses to `writer`. Generic over reader/writer for testability
/// (production uses stdin/stdout, tests use DuplexStream).
///
/// Takes a [`ReadyDaemon`] rather than a loose `(url, token)` pair so the relay
/// path can only be entered with proven-valid, non-empty credentials.
pub async fn run_relay<R, W>(mut reader: R, mut writer: W, ready: &ReadyDaemon) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to create HTTP client")?;
    let mcp_url = format!("{}/mcp", ready.base_url());
    let auth_header = format!("Bearer {}", ready.auth_token.as_str());

    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .await
            .context("failed to read from stdin")?;
        if bytes_read == 0 {
            // EOF — clean shutdown
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse to check if this is a request (has "id") or notification (no "id")
        let msg: serde_json::Value =
            serde_json::from_str(trimmed).context("failed to parse JSON-RPC message")?;
        let is_request = msg.get("id").is_some();

        let mut request = client
            .post(&mcp_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Authorization", &auth_header);
        for (name, value) in protocol_headers(&msg) {
            request = request.header(name, value);
        }
        let response = request
            .body(trimmed.to_string())
            .send()
            .await
            .context("failed to send request to daemon")?;

        let status = response.status();

        if is_request && status.as_u16() == 200 {
            let body = response
                .bytes()
                .await
                .context("failed to read response body")?;
            writer
                .write_all(&body)
                .await
                .context("failed to write response")?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        } else if status.as_u16() == 202 {
            // Notification accepted — no response to write
        } else if is_request && !status.is_success() {
            // Return a JSON-RPC error to the client instead of terminating the relay.
            // Only connection-level failures (handled by `?` on send()) should kill the session.
            let error_response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": msg.get("id"),
                "error": {
                    "code": -32603,
                    "message": format!("daemon returned HTTP {status}")
                }
            });
            let mut err_bytes = serde_json::to_vec(&error_response)?;
            err_bytes.push(b'\n');
            writer.write_all(&err_bytes).await?;
            writer.flush().await?;
        } else if !status.is_success() {
            tracing::warn!(
                status = %status,
                method = ?msg.get("method"),
                "daemon returned error for notification, ignoring"
            );
        }
    }

    Ok(())
}

/// The HTTP headers an MCP Streamable HTTP client derives from a message body.
///
/// A 2026-07-28 client carries its protocol version in `params._meta`, and the
/// daemon rejects such requests unless the version, method and name also arrive
/// as headers.
fn protocol_headers(msg: &serde_json::Value) -> Vec<(&'static str, String)> {
    use rmcp::transport::common::http_header::{
        HEADER_MCP_METHOD, HEADER_MCP_NAME, HEADER_MCP_PROTOCOL_VERSION,
    };

    let mut headers = Vec::new();
    let Some(method) = msg.get("method").and_then(|m| m.as_str()) else {
        return headers;
    };
    let params = msg.get("params");
    if let Some(version) = params
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(|v| v.as_str())
    {
        headers.push((HEADER_MCP_PROTOCOL_VERSION, version.to_string()));
    }
    headers.push((HEADER_MCP_METHOD, method.to_string()));

    let name_key = match method {
        "tools/call" | "prompts/get" => Some("name"),
        "resources/read" | "resources/subscribe" | "resources/unsubscribe" => Some("uri"),
        "tasks/get" | "tasks/update" | "tasks/cancel" => Some("taskId"),
        _ => None,
    };
    // Values needing the spec's Base64 header encoding are left off; the daemon
    // then rejects the request, which the client sees as an error response.
    if let Some(name) = name_key
        .and_then(|key| params?.get(key)?.as_str())
        .filter(|name| is_plain_header_value(name))
    {
        headers.push((HEADER_MCP_NAME, name.to_string()));
    }
    headers
}

/// True if `value` can be sent verbatim under SEP-2243 (no Base64 wrapping).
fn is_plain_header_value(value: &str) -> bool {
    let padded = value.starts_with([' ', '\t']) || value.ends_with([' ', '\t']);
    let looks_encoded = value.starts_with("=?base64?") && value.ends_with("?=");
    let printable_ascii = value.chars().all(|c| (' '..='~').contains(&c));
    printable_ascii && !padded && !looks_encoded
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use crate::db::{Database, DbConfig};
    use crate::embedding::MockEmbedder;
    use crate::service::{MemoryService, ServiceConfig};
    use crate::web::AppState;
    use crate::web::daemon::{AuthToken, DaemonPid, ReadyDaemon};

    use super::*;

    /// Build a [`ReadyDaemon`] pointing at the given loopback port + token.
    fn ready_for(port: u16, token: &str) -> ReadyDaemon {
        ReadyDaemon {
            daemon_pid: DaemonPid::new(std::process::id()).unwrap(),
            port,
            auth_token: AuthToken::new(token).unwrap(),
        }
    }

    /// Start a real Axum server on an ephemeral port using in-memory DB,
    /// returning a [`ReadyDaemon`] describing it.
    async fn start_test_server() -> ReadyDaemon {
        let auth_token = "test-relay-token".to_string();
        let port = start_test_server_with_token(&auth_token).await;
        ready_for(port, &auth_token)
    }

    /// Start a real Axum server bound to `127.0.0.1:0`, returning the bound port.
    async fn start_test_server_with_token(auth_token: &str) -> u16 {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let service = MemoryService::new(
            Arc::new(Mutex::new(db)),
            Arc::new(MockEmbedder::new(768)),
            None,
            ServiceConfig::default(),
        );
        let state = AppState {
            service,
            auth_token: auth_token.to_string(),
        };
        let app = crate::web::app_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        port
    }

    /// A relay test harness: spawns the relay, provides send/receive helpers.
    struct RelayHarness {
        stdin_tx: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        stdout_reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    }

    impl RelayHarness {
        async fn start(ready: &ReadyDaemon) -> Self {
            let (stdin_tx, stdin_rx) = tokio::io::duplex(8192);
            let (stdout_tx, stdout_rx) = tokio::io::duplex(8192);

            let ready = ready.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stdin_rx);
                run_relay(reader, stdout_tx, &ready).await
            });

            let (stdin_read_half, stdin_write_half) = tokio::io::split(stdin_tx);
            let (stdout_read_half, _stdout_write_half) = tokio::io::split(stdout_rx);
            // We only write to stdin_tx and read from stdout_rx
            drop(stdin_read_half);
            drop(_stdout_write_half);

            Self {
                stdin_tx: stdin_write_half,
                stdout_reader: BufReader::new(stdout_read_half),
            }
        }

        /// Send a JSON-RPC message (appends newline).
        async fn send(&mut self, msg: &serde_json::Value) {
            let mut line = serde_json::to_string(msg).unwrap();
            line.push('\n');
            self.stdin_tx.write_all(line.as_bytes()).await.unwrap();
            self.stdin_tx.flush().await.unwrap();
        }

        /// Read one JSON-RPC response line.
        async fn recv(&mut self) -> serde_json::Value {
            let mut line = String::new();
            self.stdout_reader.read_line(&mut line).await.unwrap();
            serde_json::from_str(&line).expect("response should be valid JSON")
        }
    }

    #[tokio::test]
    async fn relay_forwards_initialize_request_and_returns_server_info() {
        let ready = start_test_server().await;
        let mut harness = RelayHarness::start(&ready).await;

        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "relay-test", "version": "0.1"}
                }
            }))
            .await;

        let response = harness.recv().await;
        assert!(
            response["result"]["serverInfo"].is_object(),
            "response should contain serverInfo, got: {response}"
        );
        assert_eq!(
            response["result"]["serverInfo"]["name"], "erinra",
            "server name should be erinra"
        );
    }

    #[tokio::test]
    async fn relay_forwards_tool_calls_store_and_get_round_trip() {
        let ready = start_test_server().await;
        let mut harness = RelayHarness::start(&ready).await;

        // Initialize
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "relay-test", "version": "0.1"}
                }
            }))
            .await;
        let init_resp = harness.recv().await;
        assert!(init_resp["result"]["serverInfo"].is_object());

        // Send initialized notification (no response expected)
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .await;

        // Store a memory
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "store",
                    "arguments": {
                        "content": "Relay round-trip test memory",
                        "projects": ["relay-test"],
                        "type": "note"
                    }
                }
            }))
            .await;
        let store_resp = harness.recv().await;
        let store_text = store_resp["result"]["content"][0]["text"]
            .as_str()
            .expect("store should return text content");
        let store_data: serde_json::Value = serde_json::from_str(store_text).unwrap();
        let memory_id = store_data["id"]
            .as_str()
            .expect("store should return an id");
        assert!(!memory_id.is_empty());

        // Get the memory back
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "get",
                    "arguments": { "ids": [memory_id] }
                }
            }))
            .await;
        let get_resp = harness.recv().await;
        let get_text = get_resp["result"]["content"][0]["text"]
            .as_str()
            .expect("get should return text content");
        let get_data: serde_json::Value = serde_json::from_str(get_text).unwrap();
        assert_eq!(get_data[0]["id"].as_str(), Some(memory_id));
        assert_eq!(
            get_data[0]["content"].as_str(),
            Some("Relay round-trip test memory")
        );
        assert_eq!(get_data[0]["projects"][0].as_str(), Some("relay-test"));
    }

    #[tokio::test]
    async fn relay_handles_notifications_without_writing_response() {
        let ready = start_test_server().await;
        let mut harness = RelayHarness::start(&ready).await;

        // Initialize first (required)
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "relay-test", "version": "0.1"}
                }
            }))
            .await;
        let _ = harness.recv().await;

        // Send a notification (no "id" field)
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .await;

        // Now send a request and verify we get the response to THAT request,
        // not some stale notification response
        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            }))
            .await;
        let tools_resp = harness.recv().await;

        // The response should be for id:2 (tools/list), not anything from the notification
        assert_eq!(
            tools_resp["id"], 2,
            "response should be for the tools/list request"
        );
        assert!(
            tools_resp["result"]["tools"].is_array(),
            "tools/list should return a tools array, got: {tools_resp}"
        );
    }

    #[tokio::test]
    async fn relay_stops_on_reader_eof() {
        let ready = start_test_server().await;

        let (stdin_tx, stdin_rx) = tokio::io::duplex(8192);
        let (stdout_tx, _stdout_rx) = tokio::io::duplex(8192);

        let relay_handle = tokio::spawn(async move {
            let reader = BufReader::new(stdin_rx);
            run_relay(reader, stdout_tx, &ready).await
        });

        // Drop the write side of stdin -> relay should see EOF and exit cleanly
        drop(stdin_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), relay_handle)
            .await
            .expect("relay should finish within timeout")
            .expect("relay task should not panic");

        assert!(
            result.is_ok(),
            "relay should return Ok on EOF, got: {result:?}"
        );
    }

    /// Per-request `_meta` a 2026-07-28 client sends instead of initializing.
    fn modern_meta() -> serde_json::Value {
        serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {}
        })
    }

    #[tokio::test]
    async fn relay_forwards_modern_server_discover() {
        let ready = start_test_server().await;
        let mut harness = RelayHarness::start(&ready).await;

        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "server/discover",
                "params": {"_meta": modern_meta()}
            }))
            .await;

        let response = harness.recv().await;
        assert!(response["error"].is_null(), "got error: {response}");
        assert!(response["result"].is_object(), "got: {response}");
    }

    #[tokio::test]
    async fn relay_forwards_modern_tool_call() {
        let ready = start_test_server().await;
        let mut harness = RelayHarness::start(&ready).await;

        harness
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "discover", "arguments": {}, "_meta": modern_meta()}
            }))
            .await;

        let response = harness.recv().await;
        assert!(response["error"].is_null(), "got error: {response}");
        assert_eq!(response["result"]["isError"], false, "got: {response}");
    }

    fn headers_for(msg: serde_json::Value) -> Vec<(&'static str, String)> {
        protocol_headers(&msg)
    }

    #[test]
    fn legacy_tool_call_gets_method_and_name_but_no_version() {
        let headers = headers_for(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "store", "arguments": {}}
        }));
        assert_eq!(
            headers,
            vec![
                ("Mcp-Method", "tools/call".to_string()),
                ("Mcp-Name", "store".to_string())
            ]
        );
    }

    #[test]
    fn initialize_protocol_version_is_not_promoted_to_header() {
        let headers = headers_for(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {}}
        }));
        assert_eq!(headers, vec![("Mcp-Method", "initialize".to_string())]);
    }

    #[test]
    fn modern_meta_version_becomes_header() {
        let headers = headers_for(serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/cancelled",
            "params": {"_meta": modern_meta()}
        }));
        assert_eq!(
            headers,
            vec![
                ("MCP-Protocol-Version", "2026-07-28".to_string()),
                ("Mcp-Method", "notifications/cancelled".to_string())
            ]
        );
    }

    #[test]
    fn resource_methods_take_name_from_uri() {
        let headers = headers_for(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "resources/read",
            "params": {"uri": "file:///notes.md"}
        }));
        assert!(headers.contains(&("Mcp-Name", "file:///notes.md".to_string())));
    }

    #[test]
    fn names_needing_base64_are_left_off() {
        for name in [
            "caf\u{e9}",
            " lead",
            "trail\t",
            "=?base64?abc?=",
            "line\nbreak",
        ] {
            let headers = headers_for(serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": name}
            }));
            assert!(
                headers.iter().all(|(h, _)| *h != "Mcp-Name"),
                "{name:?} should not be sent verbatim"
            );
        }
        let headers = headers_for(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "has inner space and =?base64?"}
        }));
        assert!(headers.contains(&("Mcp-Name", "has inner space and =?base64?".to_string())));
    }

    #[test]
    fn responses_get_no_headers() {
        assert!(
            headers_for(serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}})).is_empty()
        );
    }
}
