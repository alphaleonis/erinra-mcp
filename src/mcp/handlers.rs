//! MCP tool handler implementations.

use crate::db::types::*;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{ErrorData, tool, tool_router};

use super::types::*;
use super::{ErinraServer, internal_error, json_result, tool_error};

/// Route a `ServiceResult` to MCP handler outcomes:
/// - `Ok(v)` → unwrapped value
/// - `InvalidInput` → tool_error (visible to LLM)
/// - `Db` user-facing (NotFound, AlreadyArchived, etc.) → tool_error
/// - `Db` internal, `Embedding`, `TaskJoin` → JSON-RPC protocol error
macro_rules! svc_result {
    ($result:expr) => {
        match $result {
            Ok(v) => v,
            Err(crate::service::ServiceError::InvalidInput(msg)) => return tool_error(msg),
            Err(crate::service::ServiceError::Db(e)) if e.is_user_facing() => {
                return tool_error(e.to_string())
            }
            Err(e) => return Err(internal_error(e.into())),
        }
    };
}

/// Non-macro version for unit testing: maps `ServiceError` to the same
/// handler outcome as `svc_result!` but returns a `Result` instead of
/// using early returns.
#[cfg(test)]
fn handle_service_error(
    err: crate::service::ServiceError,
) -> Result<(), Result<CallToolResult, ErrorData>> {
    match err {
        crate::service::ServiceError::InvalidInput(msg) => Err(tool_error(msg)),
        crate::service::ServiceError::Db(e) if e.is_user_facing() => Err(tool_error(e.to_string())),
        e => Err(Err(internal_error(e.into()))),
    }
}

// ── Request conversions (mcp → service) ─────────────────────────────────

impl From<StoreInput> for crate::service::StoreRequest {
    fn from(input: StoreInput) -> Self {
        Self {
            content: input.content,
            memory_type: input.memory_type,
            projects: input.projects,
            tags: input.tags,
            links: input
                .links
                .into_iter()
                .map(|l| (l.target_id, l.relation))
                .collect(),
        }
    }
}

impl From<UpdateInput> for crate::service::UpdateRequest {
    fn from(input: UpdateInput) -> Self {
        Self {
            id: input.id,
            content: input.content,
            memory_type: FieldUpdate::from(input.memory_type),
            projects: input.projects,
            tags: input.tags,
        }
    }
}

impl From<MergeInput> for crate::service::MergeRequest {
    fn from(input: MergeInput) -> Self {
        Self {
            source_ids: input.source_ids,
            content: input.content,
            memory_type: input.memory_type,
            projects: input.projects,
            tags: input.tags,
        }
    }
}

impl From<ContextInput> for crate::service::ContextRequest {
    fn from(input: ContextInput) -> Self {
        Self {
            queries: input.queries,
            projects: input.projects,
            memory_type: input.memory_type,
            tags: input.tags,
            include_global: input.include_global.unwrap_or(true),
            include_taxonomy: input.include_taxonomy.unwrap_or(false),
            content_budget: input.content_budget.unwrap_or(2000) as usize,
            limit: input.limit.unwrap_or(10) as usize,
        }
    }
}

// ── Tool implementations ────────────────────────────────────────────────

#[tool_router]
impl ErinraServer {
    // ── store ────────────────────────────────────────────────────────────

    /// Create a new memory. Always creates, never overwrites.
    /// Returns the new ID and top similar existing memories so you can
    /// detect duplicates, contradictions, or related knowledge.
    #[tool(name = "store")]
    pub(crate) async fn tool_store(
        &self,
        params: Parameters<StoreInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let req = crate::service::StoreRequest::from(params.0);
        let result = svc_result!(self.service.store(req).await);
        tracing::info!(tool = "store", id = %result.id, "memory stored");
        self.refresh_instructions();
        json_result(&StoreResponse::from(result))
    }

    // ── update ───────────────────────────────────────────────────────────

    /// Modify an existing memory. Only provided fields are changed.
    /// Passing projects or tags replaces all existing values.
    /// Re-embeds automatically if content changes.
    #[tool(name = "update")]
    pub(crate) async fn tool_update(
        &self,
        params: Parameters<UpdateInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let req = crate::service::UpdateRequest::from(params.0);
        let result = svc_result!(self.service.update(req).await);
        tracing::info!(tool = "update", id = %result.id, "memory updated");
        self.refresh_instructions();
        json_result(&result)
    }

    // ── archive ──────────────────────────────────────────────────────────

    /// Soft-delete a memory by setting archived_at. Never hard-deletes.
    /// Archived memories are excluded from search and list by default.
    #[tool(name = "archive")]
    pub(crate) async fn tool_archive(
        &self,
        params: Parameters<ArchiveInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = svc_result!(self.service.archive(&params.0.id).await);
        tracing::info!(tool = "archive", id = %result.id, "memory archived");
        self.refresh_instructions();
        json_result(&result)
    }

    // ── merge ────────────────────────────────────────────────────────────

    /// Combine multiple memories into one. You provide the merged content.
    /// Source memories are archived with 'supersedes' links to the new memory.
    /// Returns similar memories like store.
    #[tool(name = "merge")]
    pub(crate) async fn tool_merge(
        &self,
        params: Parameters<MergeInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let req = crate::service::MergeRequest::from(params.0);
        let result = svc_result!(self.service.merge(req).await);
        tracing::info!(tool = "merge", id = %result.id, sources = ?result.archived, "memories merged");
        self.refresh_instructions();
        json_result(&MergeResponse::from(result))
    }

    // ── link ─────────────────────────────────────────────────────────────

    /// Create a directed relationship between two memories.
    /// Common relations: supersedes, related_to, caused_by, context_for.
    #[tool(name = "link")]
    pub(crate) async fn tool_link(
        &self,
        params: Parameters<LinkInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        let link = svc_result!(
            self.service
                .link(&p.source_id, &p.target_id, &p.relation)
                .await
        );
        tracing::info!(tool = "link", id = %link.id, source = %link.source_id, target = %link.target_id, relation = %link.relation, "link created");
        json_result(&link)
    }

    // ── unlink ───────────────────────────────────────────────────────────

    /// Remove a relationship. Specify either the link ID (option A),
    /// or source_id + target_id + relation (option B).
    #[tool(name = "unlink")]
    pub(crate) async fn tool_unlink(
        &self,
        params: Parameters<UnlinkInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;

        // Validate inputs before calling service.
        if p.id.is_none()
            && (p.source_id.is_none() || p.target_id.is_none() || p.relation.is_none())
        {
            return tool_error(
                "provide either 'id' or all of 'source_id', 'target_id', and 'relation'",
            );
        }

        let removed = if let Some(id) = &p.id {
            svc_result!(self.service.unlink_by_id(id).await)
        } else {
            svc_result!(
                self.service
                    .unlink_by_endpoints(
                        p.source_id.as_deref().unwrap(),
                        p.target_id.as_deref().unwrap(),
                        p.relation.as_deref().unwrap(),
                    )
                    .await
            )
        };

        tracing::info!(tool = "unlink", removed = removed, "link(s) removed");
        json_result(&UnlinkResponse { removed })
    }

    // ── search ───────────────────────────────────────────────────────────

    /// Find memories by semantic similarity and keyword matching.
    /// Combines vector search and full-text search, ranked by relevance.
    #[tool(name = "search")]
    pub(crate) async fn tool_search(
        &self,
        params: Parameters<SearchInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let req = match search_request(params.0) {
            Ok(req) => req,
            Err(msg) => return tool_error(msg),
        };
        let hits = svc_result!(self.service.search(req).await);
        let results: Vec<SearchHitResponse> = hits
            .results
            .into_iter()
            .map(SearchHitResponse::from)
            .collect();
        json_result(&results)
    }

    // ── get ──────────────────────────────────────────────────────────────

    /// Fetch full details of specific memories by ID,
    /// including all links in both directions. Always returns full content.
    #[tool(name = "get")]
    pub(crate) async fn tool_get(
        &self,
        params: Parameters<GetInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let ids = params.0.ids;
        let memories: Vec<MemoryWithLinks> = svc_result!(self.service.get(&ids).await);
        let results: Vec<MemoryFullResponse> =
            memories.into_iter().map(MemoryFullResponse::from).collect();
        json_result(&results)
    }

    // ── list ─────────────────────────────────────────────────────────────

    /// Browse memories with filters. No search query — just filter by
    /// project, type, tags, or time range. Returns paginated results with total count.
    #[tool(name = "list")]
    pub(crate) async fn tool_list(
        &self,
        params: Parameters<ListInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let req = match list_request(params.0) {
            Ok(req) => req,
            Err(msg) => return tool_error(msg),
        };
        let result = svc_result!(self.service.list(req).await);
        let response = ListResponse {
            memories: result
                .memories
                .into_iter()
                .map(MemoryResponseSerde::from)
                .collect(),
            total: result.total,
        };
        json_result(&response)
    }

    // ── context ──────────────────────────────────────────────────────────

    /// Batched session-start search: run 1-5 queries at once, get
    /// deduplicated results within a content budget, optionally with
    /// the full taxonomy. Ideal for warming up context at the start of a
    /// coding session.
    #[tool(name = "context")]
    pub(crate) async fn tool_context(
        &self,
        params: Parameters<ContextInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        let query_count = p.queries.len();
        let req = crate::service::ContextRequest::from(p);
        let result = svc_result!(self.service.context(req).await);

        tracing::info!(
            tool = "context",
            queries = query_count,
            results = result.hits.len(),
            truncated = result.truncated,
            "context search completed"
        );

        json_result(&ContextResponse::from(result))
    }

    // ── discover ─────────────────────────────────────────────────────────

    /// Returns all known projects, types, tags, and link relations with
    /// usage counts, plus database stats. Use at session start or to
    /// refresh your understanding of the taxonomy.
    #[tool(name = "discover")]
    pub(crate) async fn tool_discover(&self) -> Result<CallToolResult, ErrorData> {
        let result = svc_result!(self.service.discover().await);
        json_result(&result)
    }
}

impl ErinraServer {
    /// Expose the macro-generated tool_router() to the parent module.
    pub(crate) fn create_tool_router() -> ToolRouter<Self> {
        Self::tool_router()
    }
}

// ── Time filter helpers ─────────────────────────────────────────────────

use crate::service::ResolvedTimeFilters;

/// Format a `SystemTime` as ISO 8601 with millisecond precision matching SQLite's
/// `strftime('%Y-%m-%dT%H:%M:%fZ', 'now')` format.
fn format_utc(time: std::time::SystemTime) -> String {
    let duration = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let millis = duration.subsec_millis();

    // Compute calendar date/time from Unix timestamp (UTC).
    let days = (secs / 86400) as i64;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Days since Unix epoch to (year, month, day) — civil calendar algorithm.
    // Adapted from Howard Hinnant's chrono-compatible algorithm.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, m, d, hours, minutes, seconds, millis
    )
}

/// Validate that a user-provided timestamp looks like ISO 8601 (YYYY-MM-DDThh:mm:ss...).
fn validate_timestamp(ts: &str, field_name: &str) -> Result<(), String> {
    if ts.len() < 19
        || ts.as_bytes().get(4) != Some(&b'-')
        || ts.as_bytes().get(7) != Some(&b'-')
        || ts.as_bytes().get(10) != Some(&b'T')
    {
        return Err(format!(
            "{field_name}: expected ISO 8601 timestamp (e.g. 2025-01-15T00:00:00Z), got: {ts}"
        ));
    }
    Ok(())
}

/// Validate all user-provided absolute timestamps in a `TimeFilterInput`.
fn validate_time_filter_input(input: &TimeFilterInput) -> Result<(), String> {
    if let Some(ts) = input.created_after {
        validate_timestamp(ts, "created_after")?;
    }
    if let Some(ts) = input.created_before {
        validate_timestamp(ts, "created_before")?;
    }
    if let Some(ts) = input.updated_after {
        validate_timestamp(ts, "updated_after")?;
    }
    if let Some(ts) = input.updated_before {
        validate_timestamp(ts, "updated_before")?;
    }
    Ok(())
}

/// Validate, resolve, and re-validate time filters in one step.
/// Combines `validate_time_filter_input` + `resolve_time_filters` +
/// `validate_resolved_ranges` to eliminate duplication in `search_request`
/// and `list_request`.
fn resolve_and_validate_time(input: &TimeFilterInput) -> Result<ResolvedTimeFilters, String> {
    validate_time_filter_input(input)?;
    let resolved = resolve_time_filters(input);
    validate_resolved_ranges(&resolved)?;
    Ok(resolved)
}

/// Validate that resolved time ranges are non-empty (after < before).
fn validate_resolved_ranges(resolved: &ResolvedTimeFilters) -> Result<(), String> {
    if let (Some(after), Some(before)) = (&resolved.created_after, &resolved.created_before)
        && after >= before
    {
        return Err("created_after must be before created_before".to_string());
    }
    if let (Some(after), Some(before)) = (&resolved.updated_after, &resolved.updated_before)
        && after >= before
    {
        return Err("updated_after must be before updated_before".to_string());
    }
    Ok(())
}

/// Input for `resolve_time_filters`: combines absolute and relative time fields.
#[derive(Default)]
pub(crate) struct TimeFilterInput<'a> {
    pub created_after: Option<&'a str>,
    pub created_before: Option<&'a str>,
    pub updated_after: Option<&'a str>,
    pub updated_before: Option<&'a str>,
    pub created_max_age_days: Option<u32>,
    pub created_min_age_days: Option<u32>,
    pub updated_max_age_days: Option<u32>,
    pub updated_min_age_days: Option<u32>,
}

/// Convert relative time fields to absolute timestamps and merge with any
/// existing absolute fields, keeping the tighter bound when both are present.
///
/// - `max_age_days` converts to `_after` (lower bound: must be newer than N days ago)
/// - `min_age_days` converts to `_before` (upper bound: must be older than N days ago)
pub(crate) fn resolve_time_filters(input: &TimeFilterInput) -> ResolvedTimeFilters {
    let now = std::time::SystemTime::now();

    fn days_ago(now: std::time::SystemTime, days: u32) -> String {
        let duration = std::time::Duration::from_secs(days as u64 * 86400);
        let past = now.checked_sub(duration).unwrap_or(std::time::UNIX_EPOCH);
        format_utc(past)
    }

    // For `_after` bounds: keep the later (tighter) timestamp.
    fn pick_later(a: Option<&str>, b: Option<String>) -> Option<String> {
        match (a, b) {
            (Some(abs), Some(rel)) => {
                if abs > rel.as_str() {
                    Some(abs.to_string())
                } else {
                    Some(rel)
                }
            }
            (Some(abs), None) => Some(abs.to_string()),
            (None, Some(rel)) => Some(rel),
            (None, None) => None,
        }
    }

    // For `_before` bounds: keep the earlier (tighter) timestamp.
    fn pick_earlier(a: Option<&str>, b: Option<String>) -> Option<String> {
        match (a, b) {
            (Some(abs), Some(rel)) => {
                if abs < rel.as_str() {
                    Some(abs.to_string())
                } else {
                    Some(rel)
                }
            }
            (Some(abs), None) => Some(abs.to_string()),
            (None, Some(rel)) => Some(rel),
            (None, None) => None,
        }
    }

    let created_max_age_ts = input.created_max_age_days.map(|d| days_ago(now, d));
    let created_min_age_ts = input.created_min_age_days.map(|d| days_ago(now, d));
    let updated_max_age_ts = input.updated_max_age_days.map(|d| days_ago(now, d));
    let updated_min_age_ts = input.updated_min_age_days.map(|d| days_ago(now, d));

    ResolvedTimeFilters {
        created_after: pick_later(input.created_after, created_max_age_ts),
        created_before: pick_earlier(input.created_before, created_min_age_ts),
        updated_after: pick_later(input.updated_after, updated_max_age_ts),
        updated_before: pick_earlier(input.updated_before, updated_min_age_ts),
    }
}

// ── Fallible request conversions (time filter resolution) ───────────────

/// Build a `SearchRequest` from `SearchInput`, resolving and validating
/// time filters. Returns an error message suitable for `tool_error` on failure.
pub(crate) fn search_request(input: SearchInput) -> Result<crate::service::SearchRequest, String> {
    let time_input = TimeFilterInput {
        created_after: input.created_after.as_deref(),
        created_before: input.created_before.as_deref(),
        updated_after: input.updated_after.as_deref(),
        updated_before: input.updated_before.as_deref(),
        created_max_age_days: input.created_max_age_days,
        created_min_age_days: input.created_min_age_days,
        updated_max_age_days: input.updated_max_age_days,
        updated_min_age_days: input.updated_min_age_days,
    };
    let resolved = resolve_and_validate_time(&time_input)?;

    Ok(crate::service::SearchRequest {
        query: input.query,
        projects: input.projects,
        memory_type: input.memory_type,
        tags: input.tags,
        include_global: input.include_global.unwrap_or(true),
        include_archived: input.include_archived.unwrap_or(false),
        time: resolved,
        limit: input.limit.unwrap_or(10),
        offset: input.offset.unwrap_or(0),
        content_max_length: None,
    })
}

/// Build a `ListRequest` from `ListInput`, resolving and validating
/// time filters. Returns an error message suitable for `tool_error` on failure.
pub(crate) fn list_request(input: ListInput) -> Result<crate::service::ListRequest, String> {
    let time_input = TimeFilterInput {
        created_after: input.created_after.as_deref(),
        created_before: input.created_before.as_deref(),
        updated_after: input.updated_after.as_deref(),
        updated_before: input.updated_before.as_deref(),
        created_max_age_days: input.created_max_age_days,
        created_min_age_days: input.created_min_age_days,
        updated_max_age_days: input.updated_max_age_days,
        updated_min_age_days: input.updated_min_age_days,
    };
    let resolved = resolve_and_validate_time(&time_input)?;

    Ok(crate::service::ListRequest {
        projects: input.projects,
        memory_type: input.memory_type,
        tags: input.tags,
        include_global: input.include_global.unwrap_or(true),
        include_archived: input.include_archived.unwrap_or(false),
        time: resolved,
        limit: input.limit.unwrap_or(20),
        offset: input.offset.unwrap_or(0),
        content_max_length: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_utc_produces_valid_iso8601() {
        let epoch = std::time::UNIX_EPOCH;
        assert_eq!(format_utc(epoch), "1970-01-01T00:00:00.000Z");

        // Known timestamp: 2025-06-15T15:10:45.123Z (Unix 1750000245.123)
        let ts = epoch + std::time::Duration::new(1750000245, 123_000_000);
        assert_eq!(format_utc(ts), "2025-06-15T15:10:45.123Z");

        // Leap year: 2024-02-29T00:00:00Z = Unix 1709164800
        let ts = epoch + std::time::Duration::from_secs(1709164800);
        assert_eq!(format_utc(ts), "2024-02-29T00:00:00.000Z");

        // Century year: 2000-03-01T00:00:00Z = Unix 951868800
        let ts = epoch + std::time::Duration::from_secs(951868800);
        assert_eq!(format_utc(ts), "2000-03-01T00:00:00.000Z");
    }

    #[test]
    fn resolve_absolute_only_passes_through() {
        let resolved = resolve_time_filters(&TimeFilterInput {
            created_after: Some("2025-01-01T00:00:00.000Z"),
            created_before: Some("2026-01-01T00:00:00.000Z"),
            ..Default::default()
        });
        assert_eq!(
            resolved.created_after.as_deref(),
            Some("2025-01-01T00:00:00.000Z")
        );
        assert_eq!(
            resolved.created_before.as_deref(),
            Some("2026-01-01T00:00:00.000Z")
        );
        assert!(resolved.updated_after.is_none());
        assert!(resolved.updated_before.is_none());
    }

    #[test]
    fn resolve_relative_max_age_produces_after() {
        // max_age_days = 7 should produce a created_after ~7 days ago.
        let resolved = resolve_time_filters(&TimeFilterInput {
            created_max_age_days: Some(7),
            ..Default::default()
        });
        assert!(
            resolved.created_after.is_some(),
            "max_age_days should produce created_after"
        );
        // The resolved timestamp should be recent (within the last 8 days).
        let ts = resolved.created_after.unwrap();
        assert!(
            ts.as_str() > "2026-03-20",
            "resolved timestamp should be recent: {ts}"
        );
    }

    #[test]
    fn resolve_relative_min_age_produces_before() {
        // min_age_days = 7 should produce a created_before ~7 days ago.
        let resolved = resolve_time_filters(&TimeFilterInput {
            created_min_age_days: Some(7),
            ..Default::default()
        });
        assert!(
            resolved.created_before.is_some(),
            "min_age_days should produce created_before"
        );
        let ts = resolved.created_before.unwrap();
        assert!(
            ts.as_str() > "2026-03-20",
            "resolved timestamp should be recent: {ts}"
        );
    }

    #[test]
    fn resolve_absolute_and_relative_keeps_tighter_after_bound() {
        // created_after = "2026-01-15" + created_max_age_days = 7
        // If the relative timestamp (7 days ago ~= 2026-03-22) is later than
        // the absolute (2026-01-15), the relative wins.
        let resolved = resolve_time_filters(&TimeFilterInput {
            created_after: Some("2026-01-15T00:00:00.000Z"),
            created_max_age_days: Some(7),
            ..Default::default()
        });
        let after = resolved.created_after.unwrap();
        // The relative (7 days ago) is later than 2026-01-15, so it should win.
        assert!(
            after.as_str() > "2026-01-15T00:00:00.000Z",
            "tighter relative bound should win: {after}"
        );
    }

    #[test]
    fn resolve_absolute_and_relative_keeps_tighter_before_bound() {
        // created_before = "2026-03-10" + created_min_age_days = 7
        // The relative timestamp (~2026-03-22) is later than the absolute (2026-03-10),
        // so the absolute wins (earlier = tighter for upper bound).
        let resolved = resolve_time_filters(&TimeFilterInput {
            created_before: Some("2026-03-10T00:00:00.000Z"),
            created_min_age_days: Some(7),
            ..Default::default()
        });
        let before = resolved.created_before.unwrap();
        assert_eq!(
            before, "2026-03-10T00:00:00.000Z",
            "tighter absolute bound should win"
        );
    }

    // ── Behavior 2: ServiceError → handler result routing ──

    #[test]
    fn service_error_invalid_input_maps_to_tool_error() {
        use crate::service::ServiceError;

        let err = ServiceError::InvalidInput("bad input".to_string());
        let result = handle_service_error(err);
        // Should be a tool error (Ok(CallToolResult with is_error))
        let Err(Ok(call_result)) = result else {
            panic!("expected tool error");
        };
        assert!(call_result.is_error.unwrap_or(false));
    }

    #[test]
    fn service_error_db_not_found_maps_to_tool_error() {
        use crate::db::error::DbError;
        use crate::service::ServiceError;

        let err = ServiceError::Db(DbError::NotFound {
            entity: "memory",
            id: "some-id".to_string(),
        });
        let result = handle_service_error(err);
        let Err(Ok(call_result)) = result else {
            panic!("expected tool error");
        };
        assert!(call_result.is_error.unwrap_or(false));
    }

    #[test]
    fn service_error_db_already_archived_maps_to_tool_error() {
        use crate::db::error::DbError;
        use crate::service::ServiceError;

        let err = ServiceError::Db(DbError::AlreadyArchived {
            id: "some-id".to_string(),
            operation: "update".to_string(),
        });
        let result = handle_service_error(err);
        let Err(Ok(call_result)) = result else {
            panic!("expected tool error");
        };
        assert!(call_result.is_error.unwrap_or(false));
    }

    #[test]
    fn service_error_embedding_maps_to_protocol_error() {
        use crate::service::ServiceError;

        let err = ServiceError::Embedding(anyhow::anyhow!("ONNX crashed"));
        let result = handle_service_error(err);
        let Err(Err(_error_data)) = result else {
            panic!("expected protocol error");
        };
    }

    // ── Behavior 3: SearchInput → SearchRequest time filter resolution ──

    #[test]
    fn search_request_from_input_no_time_filters() {
        let input = SearchInput {
            query: "test query".to_string(),
            projects: Some(vec!["proj-a".to_string()]),
            memory_type: Some("pattern".to_string()),
            tags: Some(vec!["rust".to_string()]),
            include_global: Some(false),
            include_archived: Some(true),
            created_after: None,
            created_before: None,
            updated_after: None,
            updated_before: None,
            created_max_age_days: None,
            created_min_age_days: None,
            updated_max_age_days: None,
            updated_min_age_days: None,
            limit: Some(5),
            offset: Some(10),
        };

        let req = search_request(input).unwrap();
        assert_eq!(req.query, "test query");
        assert_eq!(req.projects, Some(vec!["proj-a".to_string()]));
        assert_eq!(req.memory_type, Some("pattern".to_string()));
        assert_eq!(req.tags, Some(vec!["rust".to_string()]));
        assert!(!req.include_global);
        assert!(req.include_archived);
        assert_eq!(req.limit, 5);
        assert_eq!(req.offset, 10);
        assert!(req.time.created_after.is_none());
    }

    #[test]
    fn search_request_defaults() {
        let input = SearchInput {
            query: "q".to_string(),
            projects: None,
            memory_type: None,
            tags: None,
            include_global: None,
            include_archived: None,
            created_after: None,
            created_before: None,
            updated_after: None,
            updated_before: None,
            created_max_age_days: None,
            created_min_age_days: None,
            updated_max_age_days: None,
            updated_min_age_days: None,
            limit: None,
            offset: None,
        };

        let req = search_request(input).unwrap();
        assert!(req.include_global); // default true
        assert!(!req.include_archived); // default false
        assert_eq!(req.limit, 10); // default 10
        assert_eq!(req.offset, 0); // default 0
    }

    #[test]
    fn search_request_bad_timestamp_fails() {
        let input = SearchInput {
            query: "q".to_string(),
            projects: None,
            memory_type: None,
            tags: None,
            include_global: None,
            include_archived: None,
            created_after: Some("not-a-timestamp".to_string()),
            created_before: None,
            updated_after: None,
            updated_before: None,
            created_max_age_days: None,
            created_min_age_days: None,
            updated_max_age_days: None,
            updated_min_age_days: None,
            limit: None,
            offset: None,
        };

        let err = search_request(input).unwrap_err();
        assert!(err.contains("created_after"));
        assert!(err.contains("ISO 8601"));
    }

    #[test]
    fn search_request_inverted_range_fails() {
        let input = SearchInput {
            query: "q".to_string(),
            projects: None,
            memory_type: None,
            tags: None,
            include_global: None,
            include_archived: None,
            created_after: Some("2026-06-01T00:00:00.000Z".to_string()),
            created_before: Some("2026-01-01T00:00:00.000Z".to_string()),
            updated_after: None,
            updated_before: None,
            created_max_age_days: None,
            created_min_age_days: None,
            updated_max_age_days: None,
            updated_min_age_days: None,
            limit: None,
            offset: None,
        };

        let err = search_request(input).unwrap_err();
        assert!(err.contains("created_after must be before created_before"));
    }

    // ── Behavior 6: ListInput → ListRequest ──

    #[test]
    fn list_request_from_input() {
        let input = ListInput {
            projects: Some(vec!["proj-a".to_string()]),
            memory_type: Some("pattern".to_string()),
            tags: Some(vec!["rust".to_string()]),
            include_global: Some(false),
            include_archived: Some(true),
            created_after: None,
            created_before: None,
            updated_after: None,
            updated_before: None,
            created_max_age_days: None,
            created_min_age_days: None,
            updated_max_age_days: None,
            updated_min_age_days: None,
            limit: Some(5),
            offset: Some(10),
        };

        let req = list_request(input).unwrap();
        assert_eq!(req.projects, Some(vec!["proj-a".to_string()]));
        assert_eq!(req.memory_type, Some("pattern".to_string()));
        assert!(!req.include_global);
        assert!(req.include_archived);
        assert_eq!(req.limit, 5);
        assert_eq!(req.offset, 10);
    }

    #[test]
    fn list_request_defaults() {
        let input = ListInput {
            projects: None,
            memory_type: None,
            tags: None,
            include_global: None,
            include_archived: None,
            created_after: None,
            created_before: None,
            updated_after: None,
            updated_before: None,
            created_max_age_days: None,
            created_min_age_days: None,
            updated_max_age_days: None,
            updated_min_age_days: None,
            limit: None,
            offset: None,
        };

        let req = list_request(input).unwrap();
        assert!(req.include_global); // default true
        assert!(!req.include_archived); // default false
        assert_eq!(req.limit, 20); // default 20
        assert_eq!(req.offset, 0); // default 0
    }
}
