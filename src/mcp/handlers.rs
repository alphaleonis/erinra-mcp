//! MCP tool handler implementations.

use std::collections::HashMap;
use std::sync::Arc;

use crate::db::error::DbError;
use crate::db::types::*;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{ErrorData, tool, tool_router};

use super::types::*;
use super::{ErinraServer, internal_error, json_result, strs, strs_owned, tool_error};

/// Unwrap a DB operation result. User-facing errors (not_found, already_archived,
/// etc.) become tool-level errors visible to the LLM. Internal errors become
/// JSON-RPC protocol errors.
macro_rules! db {
    ($result:expr) => {
        match $result {
            Ok(v) => v,
            Err(e) if e.is_user_facing() => return tool_error(e.to_string()),
            Err(e) => {
                return Err(ErrorData::internal_error(
                    format!("database error: {e}"),
                    None,
                ))
            }
        }
    };
}

/// Filter similar memories by threshold and convert to response type.
pub(crate) fn filter_similar(
    raw: Vec<(Memory, f64)>,
    threshold: f64,
) -> Vec<SimilarMemoryResponse> {
    raw.into_iter()
        .filter(|(_, sim)| *sim >= threshold)
        .map(|(mem, sim)| SimilarMemoryResponse {
            id: mem.id,
            content: mem.content,
            projects: mem.projects,
            memory_type: mem.memory_type,
            tags: mem.tags,
            similarity: sim,
            created_at: mem.created_at,
            truncated: mem.truncated,
        })
        .collect()
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
        let p = params.0;

        // Pre-check: reject before expensive embedding computation.
        if p.content.len() > self.config.max_content_size {
            return tool_error(format!(
                "content is {} bytes, max is {}",
                p.content.len(),
                self.config.max_content_size
            ));
        }

        // Embed content (CPU-bound ONNX inference — run off the async runtime).
        let embedding = self.embed_content(&p.content).await?;

        // DB operations (synchronous I/O — run off the async runtime).
        let db = Arc::clone(&self.db);
        let config = Arc::clone(&self.config);
        let (id, similar_raw) = db!(tokio::task::spawn_blocking(move || {
            let projects_ref = strs_owned(&p.projects);
            let tags_ref = strs_owned(&p.tags);
            let links_ref: Vec<(&str, &str)> = p
                .links
                .iter()
                .map(|l| (l.target_id.as_str(), l.relation.as_str()))
                .collect();

            let db = db.lock().expect("db mutex poisoned");

            let id = db.store(&StoreParams {
                content: &p.content,
                memory_type: p.memory_type.as_deref(),
                projects: &projects_ref,
                tags: &tags_ref,
                links: &links_ref,
                embedding: &embedding,
            })?;

            let similar_raw = db.find_similar(
                &embedding,
                config.similar_limit,
                &[&id],
                Some(config.content_max_length),
            )?;

            Ok::<_, DbError>((id, similar_raw))
        })
        .await
        .map_err(|e| internal_error(e.into()))?);

        tracing::info!(tool = "store", id = %id, "memory stored");
        self.refresh_instructions();
        let similar = filter_similar(similar_raw, self.config.similar_threshold);
        let response = StoreResponse { id, similar };
        json_result(&response)
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
        let p = params.0;

        // Pre-check: reject before expensive embedding computation.
        if let Some(ref content) = p.content
            && content.len() > self.config.max_content_size
        {
            return tool_error(format!(
                "content is {} bytes, max is {}",
                content.len(),
                self.config.max_content_size
            ));
        }

        // Re-embed if content changed (CPU-bound — run off the async runtime).
        let embedding = match &p.content {
            Some(content) => Some(self.embed_content(content).await?),
            None => None,
        };

        // DB operation (synchronous I/O — run off the async runtime).
        let db = Arc::clone(&self.db);
        let memory_type_update = FieldUpdate::from(p.memory_type);
        let result = db!(tokio::task::spawn_blocking(move || {
            let projects_ref = strs(&p.projects);
            let tags_ref = strs(&p.tags);

            let db = db.lock().expect("db mutex poisoned");
            db.update(
                &p.id,
                &crate::db::types::UpdateParams {
                    content: p.content.as_deref(),
                    memory_type: memory_type_update.as_deref(),
                    projects: projects_ref.as_deref(),
                    tags: tags_ref.as_deref(),
                    embedding: embedding.as_deref(),
                },
            )
        })
        .await
        .map_err(|e| internal_error(e.into()))?);

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
        let id = params.0.id;
        let db = Arc::clone(&self.db);
        let result = db!(tokio::task::spawn_blocking(move || {
            let db = db.lock().expect("db mutex poisoned");
            db.archive(&id)
        })
        .await
        .map_err(|e| internal_error(e.into()))?);

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
        let p = params.0;

        if p.source_ids.is_empty() {
            return tool_error("source_ids must not be empty");
        }
        if p.source_ids.len() > 20 {
            return tool_error(format!(
                "too many source_ids ({}, max 20)",
                p.source_ids.len()
            ));
        }
        // Pre-check: reject before expensive embedding computation.
        if p.content.len() > self.config.max_content_size {
            return tool_error(format!(
                "content is {} bytes, max is {}",
                p.content.len(),
                self.config.max_content_size
            ));
        }

        // Embed the merged content (CPU-bound — run off the async runtime).
        let embedding = self.embed_content(&p.content).await?;

        // DB operations (synchronous I/O — run off the async runtime).
        let db = Arc::clone(&self.db);
        let config = Arc::clone(&self.config);
        let (merge_result, similar_raw) = db!(tokio::task::spawn_blocking(move || {
            let source_ids_ref = strs_owned(&p.source_ids);
            let projects_ref = strs_owned(&p.projects);
            let tags_ref = strs_owned(&p.tags);

            let db = db.lock().expect("db mutex poisoned");
            let result = db.merge(&MergeParams {
                source_ids: &source_ids_ref,
                content: &p.content,
                memory_type: p.memory_type.as_deref(),
                projects: &projects_ref,
                tags: &tags_ref,
                embedding: &embedding,
            })?;

            // Find similar to the merged memory.
            let mut exclude: Vec<&str> = vec![result.id.as_str()];
            for id in &result.archived {
                exclude.push(id.as_str());
            }
            let similar_raw = db.find_similar(
                &embedding,
                config.similar_limit,
                &exclude,
                Some(config.content_max_length),
            )?;

            Ok::<_, DbError>((result, similar_raw))
        })
        .await
        .map_err(|e| internal_error(e.into()))?);

        tracing::info!(tool = "merge", id = %merge_result.id, sources = ?merge_result.archived, "memories merged");
        self.refresh_instructions();
        let similar = filter_similar(similar_raw, self.config.similar_threshold);
        let response = MergeResponse {
            id: merge_result.id,
            archived: merge_result.archived,
            similar,
        };
        json_result(&response)
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
        let db = Arc::clone(&self.db);
        let link = db!(tokio::task::spawn_blocking(move || {
            let db = db.lock().expect("db mutex poisoned");
            db.link(&p.source_id, &p.target_id, &p.relation)
        })
        .await
        .map_err(|e| internal_error(e.into()))?);

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

        // Validate inputs before spawning blocking work.
        if p.id.is_none()
            && (p.source_id.is_none() || p.target_id.is_none() || p.relation.is_none())
        {
            return tool_error(
                "provide either 'id' or all of 'source_id', 'target_id', and 'relation'",
            );
        }

        let db = Arc::clone(&self.db);
        let removed = db!(tokio::task::spawn_blocking(move || {
            let db = db.lock().expect("db mutex poisoned");
            if let Some(id) = &p.id {
                db.unlink_by_id(id)
            } else {
                db.unlink_by_endpoints(
                    p.source_id.as_deref().unwrap(),
                    p.target_id.as_deref().unwrap(),
                    p.relation.as_deref().unwrap(),
                )
            }
        })
        .await
        .map_err(|e| internal_error(e.into()))?);

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
        let p = params.0;

        // Embed the query (CPU-bound — run off the async runtime).
        let query_embedding = self.embed_query_text(&p.query).await?;

        // Resolve and validate time filters.
        let time_input = TimeFilterInput {
            created_after: p.created_after.as_deref(),
            created_before: p.created_before.as_deref(),
            updated_after: p.updated_after.as_deref(),
            updated_before: p.updated_before.as_deref(),
            created_max_age_days: p.created_max_age_days,
            created_min_age_days: p.created_min_age_days,
            updated_max_age_days: p.updated_max_age_days,
            updated_min_age_days: p.updated_min_age_days,
        };
        if let Err(msg) = validate_time_filter_input(&time_input) {
            return tool_error(msg);
        }
        let time = resolve_time_filters(&time_input);
        if let Err(msg) = validate_resolved_ranges(&time) {
            return tool_error(msg);
        }

        // DB search (synchronous I/O — run off the async runtime).
        let db = Arc::clone(&self.db);
        let config = Arc::clone(&self.config);
        let reranker = self.reranker.clone();
        let hits = db!(tokio::task::spawn_blocking(move || {
            let projects_ref = strs(&p.projects);
            let tags_ref = strs(&p.tags);

            let db = db.lock().expect("db mutex poisoned");
            db.search(&SearchParams {
                query: &p.query,
                query_embedding: &query_embedding,
                filter: FilterParams {
                    projects: projects_ref.as_deref(),
                    memory_type: p.memory_type.as_deref(),
                    tags: tags_ref.as_deref(),
                    include_global: p.include_global.unwrap_or(true),
                    include_archived: p.include_archived.unwrap_or(false),
                    time: time.as_ref(),
                },
                limit: p.limit.unwrap_or(10),
                offset: p.offset.unwrap_or(0),
                content_max_length: Some(config.content_max_length),
                rrf_k: config.rrf_k,
                reranker: reranker.as_deref(),
                reranker_threshold: config.reranker_threshold,
            })
        })
        .await
        .map_err(|e| internal_error(e.into()))?);

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
        if ids.len() > 100 {
            return tool_error(format!("too many IDs ({}, max 100)", ids.len()));
        }
        let db = Arc::clone(&self.db);
        let memories = db!(tokio::task::spawn_blocking(move || {
            let ids_ref = strs_owned(&ids);
            let db = db.lock().expect("db mutex poisoned");
            db.get(&ids_ref)
        })
        .await
        .map_err(|e| internal_error(e.into()))?);

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
        let p = params.0;

        // Resolve and validate time filters.
        let time_input = TimeFilterInput {
            created_after: p.created_after.as_deref(),
            created_before: p.created_before.as_deref(),
            updated_after: p.updated_after.as_deref(),
            updated_before: p.updated_before.as_deref(),
            created_max_age_days: p.created_max_age_days,
            created_min_age_days: p.created_min_age_days,
            updated_max_age_days: p.updated_max_age_days,
            updated_min_age_days: p.updated_min_age_days,
        };
        if let Err(msg) = validate_time_filter_input(&time_input) {
            return tool_error(msg);
        }
        let time = resolve_time_filters(&time_input);
        if let Err(msg) = validate_resolved_ranges(&time) {
            return tool_error(msg);
        }

        let db = Arc::clone(&self.db);
        let config = Arc::clone(&self.config);
        let result = db!(tokio::task::spawn_blocking(move || {
            let projects_ref = strs(&p.projects);
            let tags_ref = strs(&p.tags);

            let db = db.lock().expect("db mutex poisoned");
            db.list(&ListParams {
                filter: FilterParams {
                    projects: projects_ref.as_deref(),
                    memory_type: p.memory_type.as_deref(),
                    tags: tags_ref.as_deref(),
                    include_global: p.include_global.unwrap_or(true),
                    include_archived: p.include_archived.unwrap_or(false),
                    time: time.as_ref(),
                },
                limit: p.limit.unwrap_or(20),
                offset: p.offset.unwrap_or(0),
                content_max_length: Some(config.content_max_length),
            })
        })
        .await
        .map_err(|e| internal_error(e.into()))?);

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

        // Validate inputs.
        if p.queries.is_empty() {
            return tool_error("queries must not be empty");
        }
        if p.queries.len() > 5 {
            return tool_error(format!("too many queries ({}, max 5)", p.queries.len()));
        }

        let limit = p.limit.unwrap_or(10) as usize;
        if limit == 0 {
            return tool_error("limit must be greater than 0");
        }
        let content_budget = p.content_budget.unwrap_or(2000) as usize;
        if content_budget == 0 {
            return tool_error("content_budget must be greater than 0");
        }
        let include_taxonomy = p.include_taxonomy.unwrap_or(false);
        let query_count = p.queries.len();

        // Embed all queries.
        let mut query_embeddings = Vec::with_capacity(p.queries.len());
        for query in &p.queries {
            let emb = self.embed_query_text(query).await?;
            query_embeddings.push(emb);
        }

        // Run all searches + optional discover in a single blocking block.
        let db = Arc::clone(&self.db);
        let config = Arc::clone(&self.config);
        let reranker = self.reranker.clone();
        let queries = p.queries;
        let projects = p.projects;
        let memory_type = p.memory_type;
        let tags = p.tags;
        let include_global = p.include_global.unwrap_or(true);

        let (merged_hits, taxonomy) = db!(tokio::task::spawn_blocking(move || {
            let projects_ref = strs(&projects);
            let tags_ref = strs(&tags);

            let db = db.lock().expect("db mutex poisoned");

            // Run each query's search, collecting into a HashMap for dedup.
            let mut best: HashMap<String, (SearchHit, usize)> = HashMap::new();

            for (qi, (query, emb)) in queries.iter().zip(query_embeddings.iter()).enumerate() {
                let hits = db.search(&SearchParams {
                    query,
                    query_embedding: emb,
                    filter: FilterParams {
                        projects: projects_ref.as_deref(),
                        memory_type: memory_type.as_deref(),
                        tags: tags_ref.as_deref(),
                        include_global,
                        include_archived: false,
                        ..Default::default()
                    },
                    limit: limit as u32,
                    offset: 0,
                    content_max_length: Some(config.content_max_length),
                    rrf_k: config.rrf_k,
                    reranker: reranker.as_deref(),
                    reranker_threshold: config.reranker_threshold,
                })?;

                for hit in hits.results {
                    let id = hit.memory.id.clone();
                    match best.entry(id) {
                        std::collections::hash_map::Entry::Occupied(mut entry) => {
                            let (existing_hit, existing_qi) = entry.get_mut();
                            if hit.score > existing_hit.score {
                                *existing_hit = hit;
                                *existing_qi = qi;
                            }
                        }
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert((hit, qi));
                        }
                    }
                }
            }

            // Sort by score descending.
            let mut merged: Vec<(SearchHit, usize)> = best.into_values().collect();
            merged.sort_by(|a, b| {
                b.0.score
                    .partial_cmp(&a.0.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            // Apply limit.
            merged.truncate(limit);

            // Optionally get taxonomy.
            let taxonomy = if include_taxonomy {
                Some(db.discover()?)
            } else {
                None
            };

            Ok::<_, DbError>((merged, taxonomy))
        })
        .await
        .map_err(|e| internal_error(e.into()))?);

        // Apply content budget.
        let mut total_chars: usize = 0;
        let mut response_truncated = false;
        let mut memories: Vec<ContextHit> = Vec::new();

        for (hit, qi) in merged_hits {
            let content_len = hit.memory.content.chars().count();
            if total_chars + content_len > content_budget && !memories.is_empty() {
                response_truncated = true;
                break;
            }
            total_chars += content_len;
            memories.push(ContextHit {
                id: hit.memory.id,
                content: hit.memory.content,
                projects: hit.memory.projects,
                memory_type: hit.memory.memory_type,
                tags: hit.memory.tags,
                score: hit.score,
                query_index: qi,
                created_at: hit.memory.created_at,
                truncated: hit.memory.truncated,
            });
        }

        tracing::info!(
            tool = "context",
            queries = query_count,
            results = memories.len(),
            truncated = response_truncated,
            "context search completed"
        );

        let response = ContextResponse {
            memories,
            taxonomy,
            truncated: response_truncated,
        };
        json_result(&response)
    }

    // ── discover ─────────────────────────────────────────────────────────

    /// Returns all known projects, types, tags, and link relations with
    /// usage counts, plus database stats. Use at session start or to
    /// refresh your understanding of the taxonomy.
    #[tool(name = "discover")]
    pub(crate) async fn tool_discover(&self) -> Result<CallToolResult, ErrorData> {
        let db = Arc::clone(&self.db);
        let result = db!(tokio::task::spawn_blocking(move || {
            let db = db.lock().expect("db mutex poisoned");
            db.discover()
        })
        .await
        .map_err(|e| internal_error(e.into()))?);

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

/// Resolved absolute time filters (all owned strings).
/// Created by `resolve_time_filters` from a mix of absolute and relative inputs.
pub(crate) struct ResolvedTimeFilters {
    pub created_after: Option<String>,
    pub created_before: Option<String>,
    pub updated_after: Option<String>,
    pub updated_before: Option<String>,
}

impl ResolvedTimeFilters {
    /// Borrow as `TimeFilters` for passing to `FilterParams`.
    pub fn as_ref(&self) -> TimeFilters<'_> {
        TimeFilters {
            created_after: self.created_after.as_deref(),
            created_before: self.created_before.as_deref(),
            updated_after: self.updated_after.as_deref(),
            updated_before: self.updated_before.as_deref(),
        }
    }
}

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

// ── Embed helpers ───────────────────────────────────────────────────────

impl ErinraServer {
    /// Embed a single document for storage (uses document-prefix semantics).
    pub(crate) async fn embed_content(&self, text: &str) -> Result<Vec<f32>, ErrorData> {
        let embedder = Arc::clone(&self.embedder);
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || {
            let vecs = embedder.embed_documents(&[&text])?;
            vecs.into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("embedder returned no vectors"))
        })
        .await
        .map_err(|e| internal_error(e.into()))?
        .map_err(internal_error)
    }

    /// Embed a single query for search (uses query-prefix semantics).
    pub(crate) async fn embed_query_text(&self, text: &str) -> Result<Vec<f32>, ErrorData> {
        let embedder = Arc::clone(&self.embedder);
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || embedder.embed_query(&text))
            .await
            .map_err(|e| internal_error(e.into()))?
            .map_err(internal_error)
    }
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
}
