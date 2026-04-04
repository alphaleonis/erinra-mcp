//! API route handlers for the web dashboard.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing};

use crate::db::error::{DbError, DbResult};
use crate::db::types::{FilterParams, ListParams, TimeFilters};

use super::AppState;

/// Run a sync Database operation on a blocking thread, returning an HTTP response.
async fn db_handler<T, F>(state: &AppState, op: F) -> axum::response::Response
where
    T: serde::Serialize + Send + 'static,
    F: FnOnce(&crate::db::Database) -> DbResult<T> + Send + 'static,
{
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || {
        let db = db.lock().expect("db mutex poisoned");
        op(&db)
    })
    .await
    {
        Ok(Ok(data)) => Json(data).into_response(),
        Ok(Err(e)) => db_error_to_response(e),
        Err(e) => {
            tracing::error!("db task panicked: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// Map DbError variants to HTTP responses with correct status codes.
fn db_error_to_response(e: DbError) -> axum::response::Response {
    match e {
        DbError::NotFound { .. } => (StatusCode::NOT_FOUND, e.to_string()),
        DbError::InvalidInput { .. } => (StatusCode::BAD_REQUEST, e.to_string()),
        DbError::ContentTooLarge { .. } => (StatusCode::PAYLOAD_TOO_LARGE, e.to_string()),
        DbError::DuplicateLink { .. } => (StatusCode::CONFLICT, e.to_string()),
        DbError::AlreadyArchived { .. } => (StatusCode::CONFLICT, e.to_string()),
        DbError::NotArchived { .. } => (StatusCode::CONFLICT, e.to_string()),
        DbError::Internal(e) => {
            tracing::error!("db error: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
        }
    }
    .into_response()
}

/// Build the API router with all endpoints.
pub fn api_router() -> Router<AppState> {
    Router::new()
        .route("/discover", routing::get(discover))
        .route("/memories", routing::get(list_memories))
        .route("/memories/search", routing::get(search_memories))
        .route("/memories/{id}", routing::get(get_memory))
        .route("/memories/{id}/archive", routing::post(archive_memory))
        .route("/memories/{id}/unarchive", routing::post(unarchive_memory))
        .route("/memories/bulk/archive", routing::post(bulk_archive))
        .route("/memories/bulk/unarchive", routing::post(bulk_unarchive))
}

/// Common query parameters shared by list and search endpoints.
///
/// Parsed manually from the raw query string because `serde_urlencoded`
/// does not support repeated keys (`?project=a&project=b`).
#[derive(Debug)]
struct CommonQueryParams {
    projects: Vec<String>,
    memory_type: Option<String>,
    tags: Vec<String>,
    include_archived: bool,
    include_global: bool,
    limit: u32,
    offset: u32,
    content_max_length: Option<u32>,
    created_after: Option<String>,
    created_before: Option<String>,
    updated_after: Option<String>,
    updated_before: Option<String>,
}

impl CommonQueryParams {
    /// Create with defaults.
    fn new() -> Self {
        Self {
            projects: Vec::new(),
            memory_type: None,
            tags: Vec::new(),
            include_archived: false,
            include_global: true,
            limit: 20,
            offset: 0,
            content_max_length: None,
            created_after: None,
            created_before: None,
            updated_after: None,
            updated_before: None,
        }
    }

    /// Parse a single (key, value) pair. Returns `true` if the key was recognized.
    fn parse_param(&mut self, key: &str, value: &str) -> bool {
        match key {
            "project" => self.projects.push(value.to_string()),
            "type" => self.memory_type = Some(value.to_string()),
            "tag" => self.tags.push(value.to_string()),
            "include_archived" => self.include_archived = value == "true",
            "include_global" => self.include_global = value != "false",
            "limit" => {
                if let Ok(n) = value.parse::<u32>() {
                    self.limit = n.min(200);
                }
            }
            "offset" => {
                if let Ok(n) = value.parse() {
                    self.offset = n;
                }
            }
            "content_max_length" => {
                if let Ok(n) = value.parse() {
                    self.content_max_length = Some(n);
                }
            }
            "created_after" => self.created_after = Some(value.to_string()),
            "created_before" => self.created_before = Some(value.to_string()),
            "updated_after" => self.updated_after = Some(value.to_string()),
            "updated_before" => self.updated_before = Some(value.to_string()),
            _ => return false,
        }
        true
    }

    /// Build `FilterParams` from the parsed params.
    ///
    /// Requires pre-built borrowed slices for projects and tags because
    /// `FilterParams` borrows `&[&str]` and we need the owned `Vec<&str>`
    /// to outlive the returned struct.
    fn filter_params<'a>(
        &'a self,
        project_refs: &'a [&'a str],
        tag_refs: &'a [&'a str],
    ) -> FilterParams<'a> {
        FilterParams {
            projects: if project_refs.is_empty() {
                None
            } else {
                Some(project_refs)
            },
            memory_type: self.memory_type.as_deref(),
            tags: if tag_refs.is_empty() {
                None
            } else {
                Some(tag_refs)
            },
            include_global: self.include_global,
            include_archived: self.include_archived,
            time: TimeFilters {
                created_after: self.created_after.as_deref(),
                created_before: self.created_before.as_deref(),
                updated_after: self.updated_after.as_deref(),
                updated_before: self.updated_before.as_deref(),
            },
        }
    }
}

/// Parsed query parameters for `GET /api/memories`.
#[derive(Debug)]
struct ListMemoriesQuery {
    common: CommonQueryParams,
}

impl ListMemoriesQuery {
    fn from_query(query: &str) -> Self {
        let mut common = CommonQueryParams::new();
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            common.parse_param(key.as_ref(), value.as_ref());
        }
        Self { common }
    }
}

/// GET /api/discover — returns taxonomy (projects, types, tags, relations, stats).
async fn discover(State(state): State<AppState>) -> axum::response::Response {
    db_handler(&state, |db| db.discover()).await
}

/// GET /api/memories — list/filter memories with pagination.
async fn list_memories(
    State(state): State<AppState>,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let q = ListMemoriesQuery::from_query(uri.query().unwrap_or(""));
    db_handler(&state, move |db| {
        let c = &q.common;
        let project_refs: Vec<&str> = c.projects.iter().map(|s| s.as_str()).collect();
        let tag_refs: Vec<&str> = c.tags.iter().map(|s| s.as_str()).collect();

        let params = ListParams {
            filter: c.filter_params(&project_refs, &tag_refs),
            limit: c.limit,
            offset: c.offset,
            content_max_length: c.content_max_length,
        };

        db.list(&params)
    })
    .await
}

/// Parsed query parameters for `GET /api/memories/search`.
#[derive(Debug)]
struct SearchMemoriesQuery {
    q: Option<String>,
    common: CommonQueryParams,
}

impl SearchMemoriesQuery {
    fn from_query(query: &str) -> Self {
        let mut result = Self {
            q: None,
            common: CommonQueryParams::new(),
        };
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "q" => result.q = Some(value.to_string()),
                _ => {
                    result.common.parse_param(key.as_ref(), value.as_ref());
                }
            }
        }
        result
    }
}

/// GET /api/memories/search — hybrid search with relevance scores.
async fn search_memories(
    State(state): State<AppState>,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let q = SearchMemoriesQuery::from_query(uri.query().unwrap_or(""));

    let query_text = match q.q {
        Some(ref text) if !text.is_empty() => text.clone(),
        _ => {
            return (StatusCode::BAD_REQUEST, "missing required parameter: q").into_response();
        }
    };

    // Embed the query on a blocking thread.
    let embedder = state.embedder.clone();
    let qt = query_text.clone();
    let query_embedding = match tokio::task::spawn_blocking(move || embedder.embed_query(&qt)).await
    {
        Ok(Ok(emb)) => emb,
        Ok(Err(e)) => {
            tracing::error!("embedding query failed: {e:#}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "embedding failed").into_response();
        }
        Err(e) => {
            tracing::error!("embedding task panicked: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };

    let db = state.db.clone();
    let reranker = state.reranker.clone();
    let reranker_threshold = state.reranker_threshold;
    match tokio::task::spawn_blocking(move || {
        let db = db.lock().expect("db mutex poisoned");
        let c = &q.common;
        let project_refs: Vec<&str> = c.projects.iter().map(|s| s.as_str()).collect();
        let tag_refs: Vec<&str> = c.tags.iter().map(|s| s.as_str()).collect();

        let params = crate::db::types::SearchParams {
            query: &query_text,
            query_embedding: &query_embedding,
            filter: c.filter_params(&project_refs, &tag_refs),
            limit: c.limit,
            offset: c.offset,
            content_max_length: c.content_max_length,
            rrf_k: 60,
            reranker: reranker.as_deref(),
            reranker_threshold,
        };

        db.search(&params)
    })
    .await
    {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(e)) => db_error_to_response(e),
        Err(e) => {
            tracing::error!("search task panicked: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// GET /api/memories/:id — get a single memory with its links.
async fn get_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    db_handler(&state, move |db| {
        let mut memories = db.get(&[id.as_str()])?;
        Ok(memories.swap_remove(0))
    })
    .await
}

/// Maximum number of IDs allowed in a single bulk request.
const MAX_BULK_IDS: usize = 100;

/// Request body for bulk archive/unarchive endpoints.
#[derive(serde::Deserialize)]
struct BulkIds {
    ids: Vec<String>,
}

/// POST /api/memories/:id/archive — archive a single memory.
async fn archive_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    db_handler(&state, move |db| db.archive(&id)).await
}

/// POST /api/memories/:id/unarchive — unarchive a single memory.
async fn unarchive_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    db_handler(&state, move |db| db.unarchive(&id)).await
}

/// POST /api/memories/bulk/archive — archive multiple memories.
///
/// Processes each ID independently, skipping already-archived or non-existent items.
/// Returns the collected successful results.
async fn bulk_archive(
    State(state): State<AppState>,
    Json(body): Json<BulkIds>,
) -> axum::response::Response {
    if body.ids.len() > MAX_BULK_IDS {
        return (
            StatusCode::BAD_REQUEST,
            format!("too many IDs (max {MAX_BULK_IDS})"),
        )
            .into_response();
    }
    db_handler(&state, move |db| {
        let mut results = Vec::new();
        for id in &body.ids {
            match db.archive(id) {
                Ok(r) => results.push(r),
                Err(e) if e.is_user_facing() => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(results)
    })
    .await
}

/// POST /api/memories/bulk/unarchive — unarchive multiple memories.
///
/// Processes each ID independently, skipping active or non-existent items.
/// Returns the collected successful results.
async fn bulk_unarchive(
    State(state): State<AppState>,
    Json(body): Json<BulkIds>,
) -> axum::response::Response {
    if body.ids.len() > MAX_BULK_IDS {
        return (
            StatusCode::BAD_REQUEST,
            format!("too many IDs (max {MAX_BULK_IDS})"),
        )
            .into_response();
    }
    db_handler(&state, move |db| {
        let mut results = Vec::new();
        for id in &body.ids {
            match db.unarchive(id) {
                Ok(r) => results.push(r),
                Err(e) if e.is_user_facing() => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(results)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::db::types::StoreParams;
    use crate::db::{Database, DbConfig};
    use crate::embedding::{Embedder, MockEmbedder};

    // ── CommonQueryParams unit tests ─────────────────────────────────

    #[test]
    fn common_query_params_parses_all_shared_params() {
        let mut p = CommonQueryParams::new();

        // Defaults
        assert!(p.projects.is_empty());
        assert_eq!(p.memory_type, None);
        assert!(p.tags.is_empty());
        assert!(!p.include_archived);
        assert!(p.include_global);
        assert_eq!(p.limit, 20);
        assert_eq!(p.offset, 0);
        assert_eq!(p.content_max_length, None);
        assert_eq!(p.created_after, None);

        // Parse various params
        assert!(p.parse_param("project", "alpha"));
        assert!(p.parse_param("project", "beta"));
        assert!(p.parse_param("type", "note"));
        assert!(p.parse_param("tag", "rust"));
        assert!(p.parse_param("tag", "async"));
        assert!(p.parse_param("include_archived", "true"));
        assert!(p.parse_param("include_global", "false"));
        assert!(p.parse_param("limit", "50"));
        assert!(p.parse_param("offset", "10"));
        assert!(p.parse_param("content_max_length", "500"));
        assert!(p.parse_param("created_after", "2024-01-01T00:00:00.000Z"));
        assert!(p.parse_param("created_before", "2024-12-31T00:00:00.000Z"));
        assert!(p.parse_param("updated_after", "2024-03-01T00:00:00.000Z"));
        assert!(p.parse_param("updated_before", "2024-09-01T00:00:00.000Z"));

        // Unrecognized key returns false
        assert!(!p.parse_param("unknown_key", "value"));

        // Verify parsed values
        assert_eq!(p.projects, vec!["alpha", "beta"]);
        assert_eq!(p.memory_type.as_deref(), Some("note"));
        assert_eq!(p.tags, vec!["rust", "async"]);
        assert!(p.include_archived);
        assert!(!p.include_global);
        assert_eq!(p.limit, 50);
        assert_eq!(p.offset, 10);
        assert_eq!(p.content_max_length, Some(500));
        assert_eq!(p.created_after.as_deref(), Some("2024-01-01T00:00:00.000Z"));
        assert_eq!(
            p.created_before.as_deref(),
            Some("2024-12-31T00:00:00.000Z")
        );
        assert_eq!(p.updated_after.as_deref(), Some("2024-03-01T00:00:00.000Z"));
        assert_eq!(
            p.updated_before.as_deref(),
            Some("2024-09-01T00:00:00.000Z")
        );
    }

    #[test]
    fn common_query_params_limit_capped_at_200() {
        let mut p = CommonQueryParams::new();
        p.parse_param("limit", "999");
        assert_eq!(p.limit, 200);
    }

    /// Create an AppState with an in-memory database for testing.
    fn test_state() -> AppState {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        AppState {
            db: Arc::new(Mutex::new(db)),
            embedder: Arc::new(MockEmbedder::new(768)),
            reranker: None,
            reranker_threshold: 0.0,
            auth_token: "test-token".to_string(),
            mcp_config: crate::mcp::ServerConfig::default(),
        }
    }

    const TEST_TOKEN: &str = "test-token";

    /// Build an authenticated GET request.
    fn auth_get(uri: &str) -> Request<Body> {
        Request::get(uri)
            .header("Authorization", format!("Bearer {TEST_TOKEN}"))
            .body(Body::empty())
            .unwrap()
    }

    /// Build an authenticated POST request with JSON body.
    fn auth_post(uri: &str, body: Body) -> Request<Body> {
        Request::post(uri)
            .header("Authorization", format!("Bearer {TEST_TOKEN}"))
            .header("Content-Type", "application/json")
            .body(body)
            .unwrap()
    }

    /// Store a memory and return its ID.
    fn store_memory(state: &AppState, content: &str) -> String {
        let emb = MockEmbedder::new(768);
        let embedding = emb.embed_documents(&[content]).unwrap().remove(0);
        let db = state.db.lock().unwrap();
        db.store(&StoreParams {
            content,
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &embedding,
        })
        .unwrap()
    }

    /// Override the created_at timestamp for a memory (for time-filter tests).
    fn set_created_at(state: &AppState, id: &str, timestamp: &str) {
        let db = state.db.lock().unwrap();
        db.conn()
            .execute(
                "UPDATE memories SET created_at = ?1 WHERE id = ?2",
                rusqlite::params![timestamp, id],
            )
            .unwrap();
    }

    /// Override the updated_at timestamp for a memory (for time-filter tests).
    fn set_updated_at(state: &AppState, id: &str, timestamp: &str) {
        let db = state.db.lock().unwrap();
        db.conn()
            .execute(
                "UPDATE memories SET updated_at = ?1 WHERE id = ?2",
                rusqlite::params![timestamp, id],
            )
            .unwrap();
    }

    /// Parse the JSON response body as a Value and extract memory IDs + total.
    async fn parse_list_response(response: axum::response::Response) -> (Vec<String>, i64) {
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let total = v["total"].as_i64().unwrap();
        let ids: Vec<String> = v["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_string())
            .collect();
        (ids, total)
    }

    #[tokio::test]
    async fn list_memories_created_after_filters_by_creation_time() {
        let state = test_state();

        // Store two memories and set their creation times.
        let old_id = store_memory(&state, "old memory");
        let new_id = store_memory(&state, "new memory");
        set_created_at(&state, &old_id, "2024-01-01T00:00:00.000Z");
        set_created_at(&state, &new_id, "2024-06-01T00:00:00.000Z");

        // Request memories created after mid-point.
        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_get(
                "/api/memories?created_after=2024-03-01T00:00:00.000Z",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (ids, total) = parse_list_response(response).await;
        assert_eq!(total, 1);
        assert_eq!(ids, vec![new_id]);
    }

    #[tokio::test]
    async fn list_memories_updated_after_filters_by_update_time() {
        let state = test_state();

        let id_a = store_memory(&state, "memory A");
        let id_b = store_memory(&state, "memory B");
        set_updated_at(&state, &id_a, "2024-01-01T00:00:00.000Z");
        set_updated_at(&state, &id_b, "2024-06-01T00:00:00.000Z");

        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_get(
                "/api/memories?updated_after=2024-03-01T00:00:00.000Z",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (ids, total) = parse_list_response(response).await;
        assert_eq!(total, 1);
        assert_eq!(ids, vec![id_b]);
    }

    /// Store a memory with a specific project and return its ID.
    fn store_memory_with_project(state: &AppState, content: &str, project: &str) -> String {
        let emb = MockEmbedder::new(768);
        let embedding = emb.embed_documents(&[content]).unwrap().remove(0);
        let db = state.db.lock().unwrap();
        db.store(&StoreParams {
            content,
            memory_type: None,
            projects: &[project],
            tags: &[],
            links: &[],
            embedding: &embedding,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn list_memories_time_filters_combine_with_project_filter() {
        let state = test_state();

        // Memory A: project "alpha", old.
        let id_a = store_memory_with_project(&state, "alpha old", "alpha");
        // Memory B: project "alpha", new.
        let id_b = store_memory_with_project(&state, "alpha new", "alpha");
        // Memory C: project "beta", new.
        let id_c = store_memory_with_project(&state, "beta new", "beta");

        set_created_at(&state, &id_a, "2024-01-01T00:00:00.000Z");
        set_created_at(&state, &id_b, "2024-06-01T00:00:00.000Z");
        set_created_at(&state, &id_c, "2024-06-01T00:00:00.000Z");

        // Filter: project=alpha AND created_after=March → only id_b.
        let app = super::super::app_router(state);
        let response = app
            .oneshot(
                auth_get("/api/memories?project=alpha&created_after=2024-03-01T00:00:00.000Z&include_global=false"),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (ids, total) = parse_list_response(response).await;
        assert_eq!(total, 1);
        assert_eq!(ids, vec![id_b]);
    }

    #[tokio::test]
    async fn list_memories_time_filters_combine_with_and_semantics() {
        let state = test_state();

        // Create three memories with different creation times.
        let id_early = store_memory(&state, "early");
        let id_mid = store_memory(&state, "mid");
        let id_late = store_memory(&state, "late");
        set_created_at(&state, &id_early, "2024-01-01T00:00:00.000Z");
        set_created_at(&state, &id_mid, "2024-06-01T00:00:00.000Z");
        set_created_at(&state, &id_late, "2024-12-01T00:00:00.000Z");

        // Window: only mid should match (after Jan, before Dec).
        let app = super::super::app_router(state);
        let response = app
            .oneshot(
                auth_get("/api/memories?created_after=2024-03-01T00:00:00.000Z&created_before=2024-09-01T00:00:00.000Z"),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (ids, total) = parse_list_response(response).await;
        assert_eq!(total, 1);
        assert_eq!(ids, vec![id_mid]);
    }

    #[tokio::test]
    async fn list_memories_updated_before_filters_by_update_time() {
        let state = test_state();

        let id_a = store_memory(&state, "memory A");
        let id_b = store_memory(&state, "memory B");
        set_updated_at(&state, &id_a, "2024-01-01T00:00:00.000Z");
        set_updated_at(&state, &id_b, "2024-06-01T00:00:00.000Z");

        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_get(
                "/api/memories?updated_before=2024-03-01T00:00:00.000Z",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (ids, total) = parse_list_response(response).await;
        assert_eq!(total, 1);
        assert_eq!(ids, vec![id_a]);
    }

    #[tokio::test]
    async fn list_memories_created_before_filters_by_creation_time() {
        let state = test_state();

        let old_id = store_memory(&state, "old memory");
        let _new_id = store_memory(&state, "new memory");
        set_created_at(&state, &old_id, "2024-01-01T00:00:00.000Z");
        set_created_at(&state, &_new_id, "2024-06-01T00:00:00.000Z");

        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_get(
                "/api/memories?created_before=2024-03-01T00:00:00.000Z",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (ids, total) = parse_list_response(response).await;
        assert_eq!(total, 1);
        assert_eq!(ids, vec![old_id]);
    }

    #[test]
    fn db_error_not_found_maps_to_404() {
        let err = DbError::NotFound {
            entity: "memory",
            id: "abc".into(),
        };
        let response = db_error_to_response(err);
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn db_error_invalid_input_maps_to_400() {
        let err = DbError::InvalidInput {
            message: "bad field".into(),
        };
        let response = db_error_to_response(err);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn db_error_content_too_large_maps_to_413() {
        let err = DbError::ContentTooLarge {
            actual: 200_000,
            max: 100_000,
        };
        let response = db_error_to_response(err);
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn db_error_duplicate_link_maps_to_409() {
        let err = DbError::DuplicateLink {
            source_id: "a".into(),
            target_id: "b".into(),
            relation: "related_to".into(),
        };
        let response = db_error_to_response(err);
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn db_error_already_archived_maps_to_409() {
        let err = DbError::AlreadyArchived {
            id: "abc".into(),
            operation: "update".into(),
        };
        let response = db_error_to_response(err);
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn db_error_not_archived_maps_to_409() {
        let err = DbError::NotArchived { id: "abc".into() };
        let response = db_error_to_response(err);
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn db_handler_returns_json_200_on_success() {
        let state = test_state();

        let response = db_handler(&state, |_db| Ok(serde_json::json!({"ok": true}))).await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn db_handler_returns_error_status_on_db_error() {
        let state = test_state();

        let response = db_handler::<serde_json::Value, _>(&state, |_db| {
            Err(DbError::NotFound {
                entity: "memory",
                id: "missing-id".into(),
            })
        })
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn search_returns_400_when_q_is_missing() {
        let state = test_state();
        let app = super::super::app_router(state);
        let response = app.oneshot(auth_get("/api/memories/search")).await.unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn search_returns_400_when_q_is_empty() {
        let state = test_state();
        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_get("/api/memories/search?q="))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// Parse the JSON search response body, extracting result IDs, scores, and total.
    async fn parse_search_response(
        response: axum::response::Response,
    ) -> (Vec<(String, f64)>, i64) {
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let total = v["total"].as_i64().unwrap();
        let results: Vec<(String, f64)> = v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["memory"]["id"].as_str().unwrap().to_string(),
                    r["score"].as_f64().unwrap(),
                )
            })
            .collect();
        (results, total)
    }

    #[tokio::test]
    async fn search_returns_results_with_scores() {
        let state = test_state();
        store_memory(
            &state,
            "Rust error handling patterns with anyhow and thiserror",
        );
        store_memory(&state, "SQLite WAL mode for concurrent access");
        store_memory(&state, "Python decorators for metaprogramming");

        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_get("/api/memories/search?q=error+handling"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (results, total) = parse_search_response(response).await;
        assert!(total > 0, "should return at least one result");

        // Each result should have a positive score.
        for (id, score) in &results {
            assert!(
                *score > 0.0,
                "result {id} should have positive score, got {score}"
            );
        }

        // Response should include memory fields.
        // (Verified indirectly by parse_search_response extracting id from memory.id)
    }

    #[tokio::test]
    async fn search_results_include_links() {
        let state = test_state();
        let id_a = store_memory(&state, "Memory A about Rust error handling");
        let id_b = store_memory(&state, "Memory B about anyhow crate");

        // Create a link between A and B.
        {
            let db = state.db.lock().unwrap();
            db.link(&id_a, &id_b, "related_to").unwrap();
        }

        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_get("/api/memories/search?q=Rust+error"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Find the result for memory A and verify it has outgoing links.
        let result_a = v["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["memory"]["id"].as_str().unwrap() == id_a)
            .expect("memory A should be in search results");
        assert!(
            !result_a["outgoing_links"].as_array().unwrap().is_empty(),
            "memory A should have outgoing links"
        );
    }

    #[tokio::test]
    async fn search_composes_with_project_filter() {
        let state = test_state();
        store_memory_with_project(&state, "Rust error handling in erinra", "erinra");
        store_memory_with_project(&state, "Rust error handling in vestige", "vestige");

        let app = super::super::app_router(state);

        // Search with project filter — should only return erinra memory.
        let response = app
            .oneshot(auth_get(
                "/api/memories/search?q=error+handling&project=erinra&include_global=false",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (results, total) = parse_search_response(response).await;
        assert_eq!(
            total, 1,
            "should return exactly one result for project=erinra"
        );
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn search_paginates_with_limit_and_offset() {
        let state = test_state();
        // Store enough memories to test pagination.
        for i in 0..5 {
            store_memory(&state, &format!("Memory about Rust pattern {i}"));
        }

        let app = super::super::app_router(state.clone());

        // First page: limit=2
        let response = app
            .oneshot(auth_get("/api/memories/search?q=Rust+pattern&limit=2"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let (results_page1, _) = parse_search_response(response).await;
        assert_eq!(results_page1.len(), 2, "limit=2 should return 2 results");

        // Second page: limit=2, offset=2
        let app2 = super::super::app_router(state);
        let response = app2
            .oneshot(auth_get(
                "/api/memories/search?q=Rust+pattern&limit=2&offset=2",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let (results_page2, _) = parse_search_response(response).await;
        assert_eq!(
            results_page2.len(),
            2,
            "offset=2,limit=2 should return 2 results"
        );

        // Pages should have different results.
        let page1_ids: Vec<&str> = results_page1.iter().map(|(id, _)| id.as_str()).collect();
        let page2_ids: Vec<&str> = results_page2.iter().map(|(id, _)| id.as_str()).collect();
        for id in &page2_ids {
            assert!(
                !page1_ids.contains(id),
                "page 2 should not contain IDs from page 1"
            );
        }
    }

    #[tokio::test]
    async fn search_total_reflects_all_matches_not_page_size() {
        let state = test_state();
        // Store 5 memories that all match the query.
        for i in 0..5 {
            store_memory(&state, &format!("Memory about Rust pattern {i}"));
        }

        // Request only 2 results — total should still be 5 (all matches).
        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_get("/api/memories/search?q=Rust+pattern&limit=2"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (results, total) = parse_search_response(response).await;
        assert_eq!(results.len(), 2, "page should have 2 results");
        assert_eq!(
            total, 5,
            "total should be 5 (all matching memories), not {} (page size)",
            total
        );
    }

    #[tokio::test]
    async fn search_created_after_filters_by_creation_time() {
        let state = test_state();

        // Store two memories with different creation times.
        let old_id = store_memory(&state, "Rust error handling old pattern");
        let new_id = store_memory(&state, "Rust error handling new pattern");
        set_created_at(&state, &old_id, "2024-01-01T00:00:00.000Z");
        set_created_at(&state, &new_id, "2024-06-01T00:00:00.000Z");

        // Search with created_after — should only return the new memory.
        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_get(
                "/api/memories/search?q=Rust+error+handling&created_after=2024-03-01T00:00:00.000Z",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (results, total) = parse_search_response(response).await;
        assert_eq!(total, 1, "only the new memory should match");
        assert_eq!(results[0].0, new_id);
    }

    #[tokio::test]
    async fn search_created_before_filters_by_creation_time() {
        let state = test_state();

        let old_id = store_memory(&state, "Rust error handling old approach");
        let _new_id = store_memory(&state, "Rust error handling new approach");
        set_created_at(&state, &old_id, "2024-01-01T00:00:00.000Z");
        set_created_at(&state, &_new_id, "2024-06-01T00:00:00.000Z");

        let app = super::super::app_router(state);
        let response = app
            .oneshot(
                auth_get("/api/memories/search?q=Rust+error+handling&created_before=2024-03-01T00:00:00.000Z"),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (results, total) = parse_search_response(response).await;
        assert_eq!(total, 1, "only the old memory should match");
        assert_eq!(results[0].0, old_id);
    }

    #[tokio::test]
    async fn search_time_filters_compose_with_project_filter() {
        let state = test_state();

        let old_alpha =
            store_memory_with_project(&state, "Rust patterns old alpha context", "alpha");
        let new_alpha =
            store_memory_with_project(&state, "Rust patterns new alpha context", "alpha");
        let _new_beta = store_memory_with_project(&state, "Rust patterns new beta context", "beta");
        set_created_at(&state, &old_alpha, "2024-01-01T00:00:00.000Z");
        set_created_at(&state, &new_alpha, "2024-06-01T00:00:00.000Z");
        set_created_at(&state, &_new_beta, "2024-06-01T00:00:00.000Z");

        // Search: project=alpha AND created_after=March → only new_alpha.
        let app = super::super::app_router(state);
        let response = app
            .oneshot(
                auth_get("/api/memories/search?q=Rust+patterns&project=alpha&created_after=2024-03-01T00:00:00.000Z&include_global=false"),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (results, total) = parse_search_response(response).await;
        assert_eq!(total, 1);
        assert_eq!(results[0].0, new_alpha);
    }

    // ── archive/unarchive endpoint tests ──────────────────────────────

    #[tokio::test]
    async fn post_archive_returns_200_with_archive_result() {
        let state = test_state();
        let id = store_memory(&state, "archive via api");

        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_post(
                &format!("/api/memories/{id}/archive"),
                Body::empty(),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(result["id"], id);
        assert!(result["archived_at"].is_string());
    }

    #[tokio::test]
    async fn post_archive_not_found_returns_404() {
        let state = test_state();

        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_post(
                "/api/memories/nonexistent-id/archive",
                Body::empty(),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn post_archive_already_archived_returns_409() {
        let state = test_state();
        let id = store_memory(&state, "double archive");

        // Archive it first
        {
            let db = state.db.lock().unwrap();
            db.archive(&id).unwrap();
        }

        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_post(
                &format!("/api/memories/{id}/archive"),
                Body::empty(),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn post_unarchive_returns_200_with_unarchive_result() {
        let state = test_state();
        let id = store_memory(&state, "unarchive via api");

        // Archive it first
        {
            let db = state.db.lock().unwrap();
            db.archive(&id).unwrap();
        }

        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_post(
                &format!("/api/memories/{id}/unarchive"),
                Body::empty(),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(result["id"], id);
        assert!(result["updated_at"].is_string());
    }

    #[tokio::test]
    async fn post_bulk_archive_returns_results() {
        let state = test_state();
        let id1 = store_memory(&state, "bulk archive 1");
        let id2 = store_memory(&state, "bulk archive 2");
        let id3 = store_memory(&state, "bulk archive 3");

        // Archive id3 beforehand — it should be skipped
        {
            let db = state.db.lock().unwrap();
            db.archive(&id3).unwrap();
        }

        let app = super::super::app_router(state);
        let body = serde_json::json!({ "ids": [id1, id2, id3] });
        let response = app
            .oneshot(auth_post(
                "/api/memories/bulk/archive",
                Body::from(serde_json::to_vec(&body).unwrap()),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let results: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        // Only id1 and id2 should be archived (id3 was already archived)
        assert_eq!(results.len(), 2);
        let result_ids: Vec<&str> = results.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert!(result_ids.contains(&id1.as_str()));
        assert!(result_ids.contains(&id2.as_str()));
    }

    #[tokio::test]
    async fn post_bulk_unarchive_returns_results() {
        let state = test_state();
        let id1 = store_memory(&state, "bulk unarchive 1");
        let id2 = store_memory(&state, "bulk unarchive 2");
        let id3 = store_memory(&state, "bulk unarchive 3");

        // Archive id1 and id2, leave id3 active — it should be skipped
        {
            let db = state.db.lock().unwrap();
            db.archive(&id1).unwrap();
            db.archive(&id2).unwrap();
        }

        let app = super::super::app_router(state);
        let body = serde_json::json!({ "ids": [id1, id2, id3] });
        let response = app
            .oneshot(auth_post(
                "/api/memories/bulk/unarchive",
                Body::from(serde_json::to_vec(&body).unwrap()),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let results: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        // Only id1 and id2 should be unarchived (id3 was already active)
        assert_eq!(results.len(), 2);
        let result_ids: Vec<&str> = results.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert!(result_ids.contains(&id1.as_str()));
        assert!(result_ids.contains(&id2.as_str()));
    }

    #[tokio::test]
    async fn post_unarchive_not_found_returns_404() {
        let state = test_state();

        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_post(
                "/api/memories/nonexistent-id/unarchive",
                Body::empty(),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn post_unarchive_active_memory_returns_409() {
        let state = test_state();
        let id = store_memory(&state, "active unarchive attempt");

        let app = super::super::app_router(state);
        let response = app
            .oneshot(auth_post(
                &format!("/api/memories/{id}/unarchive"),
                Body::empty(),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn db_error_internal_maps_to_500_with_generic_body() {
        let err = DbError::Internal(anyhow::anyhow!("secret disk error details"));
        let response = db_error_to_response(err);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body_str.contains("internal error"),
            "body should contain 'internal error', got: {body_str}"
        );
        assert!(
            !body_str.contains("secret disk error details"),
            "body should not leak internal error details"
        );
    }
}
