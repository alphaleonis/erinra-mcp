//! API route handlers for the web dashboard.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing};

use crate::db::error::DbError;

use super::AppState;

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

/// Build a `ListRequest` from parsed query params.
fn to_list_request(common: &CommonQueryParams) -> crate::service::ListRequest {
    crate::service::ListRequest {
        projects: if common.projects.is_empty() {
            None
        } else {
            Some(common.projects.clone())
        },
        memory_type: common.memory_type.clone(),
        tags: if common.tags.is_empty() {
            None
        } else {
            Some(common.tags.clone())
        },
        include_global: common.include_global,
        include_archived: common.include_archived,
        time: crate::service::ResolvedTimeFilters {
            created_after: common.created_after.clone(),
            created_before: common.created_before.clone(),
            updated_after: common.updated_after.clone(),
            updated_before: common.updated_before.clone(),
        },
        limit: common.limit,
        offset: common.offset,
        content_max_length: common.content_max_length,
    }
}

/// Build a `SearchRequest` from parsed query params and a query string.
fn to_search_request(query: String, common: &CommonQueryParams) -> crate::service::SearchRequest {
    crate::service::SearchRequest {
        query,
        projects: if common.projects.is_empty() {
            None
        } else {
            Some(common.projects.clone())
        },
        memory_type: common.memory_type.clone(),
        tags: if common.tags.is_empty() {
            None
        } else {
            Some(common.tags.clone())
        },
        include_global: common.include_global,
        include_archived: common.include_archived,
        time: crate::service::ResolvedTimeFilters {
            created_after: common.created_after.clone(),
            created_before: common.created_before.clone(),
            updated_after: common.updated_after.clone(),
            updated_before: common.updated_before.clone(),
        },
        limit: common.limit,
        offset: common.offset,
        content_max_length: common.content_max_length,
    }
}

/// Map ServiceError variants to HTTP responses with correct status codes.
fn service_error_response(e: crate::service::ServiceError) -> axum::response::Response {
    use crate::service::ServiceError;
    match e {
        ServiceError::InvalidInput(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
        ServiceError::Db(db_err) => db_error_to_response(db_err),
        ServiceError::Embedding(e) => {
            tracing::error!("embedding error: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
                .into_response()
        }
        ServiceError::TaskJoin(e) => {
            tracing::error!("task join error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
                .into_response()
        }
    }
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
    match state.service.discover().await {
        Ok(result) => Json(result).into_response(),
        Err(e) => service_error_response(e),
    }
}

/// GET /api/memories — list/filter memories with pagination.
async fn list_memories(
    State(state): State<AppState>,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let q = ListMemoriesQuery::from_query(uri.query().unwrap_or(""));
    let req = to_list_request(&q.common);
    match state.service.list(req).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => service_error_response(e),
    }
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

    let req = to_search_request(query_text, &q.common);
    match state.service.search(req).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => service_error_response(e),
    }
}

/// GET /api/memories/:id — get a single memory with its links.
async fn get_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    match state.service.get(std::slice::from_ref(&id)).await {
        Ok(mut memories) if !memories.is_empty() => Json(memories.swap_remove(0)).into_response(),
        Ok(_) => (StatusCode::NOT_FOUND, format!("memory not found: {id}")).into_response(),
        Err(e) => service_error_response(e),
    }
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
    match state.service.archive(&id).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => service_error_response(e),
    }
}

/// POST /api/memories/:id/unarchive — unarchive a single memory.
async fn unarchive_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    match state.service.unarchive(&id).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => service_error_response(e),
    }
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
    match state.service.bulk_archive(&body.ids).await {
        Ok(results) => Json(results).into_response(),
        Err(e) => service_error_response(e),
    }
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
    match state.service.bulk_unarchive(&body.ids).await {
        Ok(results) => Json(results).into_response(),
        Err(e) => service_error_response(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::db::{Database, DbConfig};
    use crate::embedding::MockEmbedder;

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
        let service = crate::service::MemoryService::new(
            Arc::new(Mutex::new(db)),
            Arc::new(MockEmbedder::new(768)),
            None,
            crate::service::ServiceConfig::default(),
        );
        AppState {
            service,
            auth_token: "test-token".to_string(),
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

    // ── query param → service request conversion tests ────────────────

    #[test]
    fn common_to_search_request_maps_all_fields() {
        let mut common = CommonQueryParams::new();
        common.parse_param("project", "alpha");
        common.parse_param("project", "beta");
        common.parse_param("type", "fact");
        common.parse_param("tag", "rust");
        common.parse_param("include_archived", "true");
        common.parse_param("include_global", "false");
        common.parse_param("limit", "50");
        common.parse_param("offset", "10");
        common.parse_param("content_max_length", "200");
        common.parse_param("created_after", "2024-01-01T00:00:00.000Z");
        common.parse_param("updated_before", "2024-12-01T00:00:00.000Z");

        let req = to_search_request("test query".to_string(), &common);
        assert_eq!(req.query, "test query");
        assert_eq!(
            req.projects,
            Some(vec!["alpha".to_string(), "beta".to_string()])
        );
        assert_eq!(req.memory_type, Some("fact".to_string()));
        assert_eq!(req.tags, Some(vec!["rust".to_string()]));
        assert!(req.include_archived);
        assert!(!req.include_global);
        assert_eq!(req.limit, 50);
        assert_eq!(req.offset, 10);
        assert_eq!(req.content_max_length, Some(200));
        assert_eq!(
            req.time.created_after.as_deref(),
            Some("2024-01-01T00:00:00.000Z")
        );
        assert_eq!(
            req.time.updated_before.as_deref(),
            Some("2024-12-01T00:00:00.000Z")
        );
    }

    #[test]
    fn common_to_search_request_empty_projects_yields_none() {
        let common = CommonQueryParams::new();
        let req = to_search_request("q".to_string(), &common);
        assert_eq!(req.projects, None);
        assert_eq!(req.tags, None);
        assert_eq!(req.content_max_length, None);
    }

    #[test]
    fn common_to_list_request_maps_all_fields() {
        let mut common = CommonQueryParams::new();
        common.parse_param("project", "gamma");
        common.parse_param("type", "decision");
        common.parse_param("tag", "arch");
        common.parse_param("include_archived", "true");
        common.parse_param("limit", "30");
        common.parse_param("content_max_length", "100");

        let req = to_list_request(&common);
        assert_eq!(req.projects, Some(vec!["gamma".to_string()]));
        assert_eq!(req.memory_type, Some("decision".to_string()));
        assert_eq!(req.tags, Some(vec!["arch".to_string()]));
        assert!(req.include_archived);
        assert_eq!(req.limit, 30);
        assert_eq!(req.content_max_length, Some(100));
    }

    #[test]
    fn common_to_list_request_defaults() {
        let common = CommonQueryParams::new();
        let req = to_list_request(&common);
        assert_eq!(req.projects, None);
        assert_eq!(req.tags, None);
        assert!(req.include_global);
        assert!(!req.include_archived);
        assert_eq!(req.limit, 20);
        assert_eq!(req.offset, 0);
        assert_eq!(req.content_max_length, None);
    }

    // ── service_error_response tests ──────────────────────────────────

    #[test]
    fn service_error_invalid_input_maps_to_400() {
        use crate::service::ServiceError;
        let err = ServiceError::InvalidInput("bad field".into());
        let response = service_error_response(err);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn service_error_db_not_found_maps_to_404() {
        use crate::service::ServiceError;
        let err = ServiceError::Db(DbError::NotFound {
            entity: "memory",
            id: "abc".into(),
        });
        let response = service_error_response(err);
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn service_error_db_already_archived_maps_to_409() {
        use crate::service::ServiceError;
        let err = ServiceError::Db(DbError::AlreadyArchived {
            id: "abc".into(),
            operation: "update".into(),
        });
        let response = service_error_response(err);
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn service_error_db_not_archived_maps_to_409() {
        use crate::service::ServiceError;
        let err = ServiceError::Db(DbError::NotArchived { id: "abc".into() });
        let response = service_error_response(err);
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn service_error_db_duplicate_link_maps_to_409() {
        use crate::service::ServiceError;
        let err = ServiceError::Db(DbError::DuplicateLink {
            source_id: "a".into(),
            target_id: "b".into(),
            relation: "related_to".into(),
        });
        let response = service_error_response(err);
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn service_error_db_content_too_large_maps_to_413() {
        use crate::service::ServiceError;
        let err = ServiceError::Db(DbError::ContentTooLarge {
            actual: 200_000,
            max: 100_000,
        });
        let response = service_error_response(err);
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn service_error_embedding_maps_to_500() {
        use crate::service::ServiceError;
        let err = ServiceError::Embedding(anyhow::anyhow!("ONNX crashed"));
        let response = service_error_response(err);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn service_error_db_internal_maps_to_500_without_leaking_details() {
        use crate::service::ServiceError;
        let err = ServiceError::Db(DbError::Internal(anyhow::anyhow!(
            "secret disk error details"
        )));
        let response = service_error_response(err);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = axum::body::to_bytes(response.into_body(), 10_000)
            .await
            .unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            !body_str.contains("secret disk error details"),
            "internal error details should not be leaked in the response body"
        );
        assert!(
            body_str.contains("internal error"),
            "response should contain a generic error message, got: {body_str}"
        );
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
}
