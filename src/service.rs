//! MemoryService: orchestration layer between transports (MCP/HTTP) and storage.
//!
//! Owns the embed -> DB -> post-process pipeline. Both MCP handlers and web
//! routes become thin shims that parse wire format, call the service, and
//! convert the result.

use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::db::Database;
use crate::db::error::DbError;
use crate::db::types::*;
use crate::embedding::{Embedder, Reranker};

// ── Configuration ──────────────────────────────────────────────────────

/// Service-level configuration for memory operations.
#[derive(Clone, Debug)]
pub struct ServiceConfig {
    /// Number of similar memories returned by `store` and `merge`.
    pub similar_limit: usize,
    /// Minimum cosine similarity to include in similar results.
    pub similar_threshold: f64,
    /// Default content truncation for store/search/list responses.
    pub content_max_length: u32,
    /// RRF constant for hybrid search result merging.
    pub rrf_k: u32,
    /// Maximum content size in bytes (enforced on store/update/merge).
    pub max_content_size: usize,
    /// Minimum reranker score to include in search results.
    pub reranker_threshold: f64,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        let store = crate::config::StoreConfig::default();
        Self {
            similar_limit: store.similar_limit,
            similar_threshold: store.similar_threshold,
            content_max_length: store.content_max_length,
            rrf_k: crate::config::SearchConfig::default().rrf_k,
            max_content_size: store.max_content_size,
            reranker_threshold: crate::config::RerankerConfig::default().threshold,
        }
    }
}

impl From<&Config> for ServiceConfig {
    fn from(config: &Config) -> Self {
        Self {
            similar_limit: config.store.similar_limit,
            similar_threshold: config.store.similar_threshold,
            content_max_length: config.store.content_max_length,
            rrf_k: config.search.rrf_k,
            max_content_size: config.store.max_content_size,
            reranker_threshold: config.reranker.threshold,
        }
    }
}

// ── Error type ─────────────────────────────────────────────────────────

/// Unified error type for service operations.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("embedding failed: {0}")]
    Embedding(#[from] anyhow::Error),
    #[error("{0}")]
    InvalidInput(String),
    #[error("internal task failure: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),
}

impl ServiceError {
    /// Returns `true` if the error message is safe to show to the end user
    /// (as opposed to internal/infrastructure errors).
    pub fn is_user_facing(&self) -> bool {
        match self {
            Self::Db(e) => e.is_user_facing(),
            Self::Embedding(_) => false,
            Self::InvalidInput(_) => true,
            Self::TaskJoin(_) => false,
        }
    }
}

/// Alias for service results.
pub type ServiceResult<T> = Result<T, ServiceError>;

// ── Request types (owned, no lifetimes) ────────────────────────────────

/// Request to store a new memory.
#[derive(Debug)]
pub struct StoreRequest {
    pub content: String,
    pub memory_type: Option<String>,
    pub projects: Vec<String>,
    pub tags: Vec<String>,
    pub links: Vec<(String, String)>, // (target_id, relation)
}

/// Request to update an existing memory.
#[derive(Debug)]
pub struct UpdateRequest {
    pub id: String,
    pub content: Option<String>,
    pub memory_type: FieldUpdate<String>,
    pub projects: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
}

/// Request to merge multiple memories into one.
#[derive(Debug)]
pub struct MergeRequest {
    pub source_ids: Vec<String>,
    pub content: String,
    pub memory_type: Option<String>,
    pub projects: Vec<String>,
    pub tags: Vec<String>,
}

/// Request to list memories with filters (no search query).
#[derive(Debug)]
pub struct ListRequest {
    pub projects: Option<Vec<String>>,
    pub memory_type: Option<String>,
    pub tags: Option<Vec<String>>,
    pub include_global: bool,
    pub include_archived: bool,
    pub time: ResolvedTimeFilters,
    pub limit: u32,
    pub offset: u32,
    /// Per-request content truncation override. When `None`, uses config default.
    pub content_max_length: Option<u32>,
}

/// Request for batched session-start context search.
#[derive(Debug)]
pub struct ContextRequest {
    pub queries: Vec<String>,
    pub projects: Option<Vec<String>>,
    pub memory_type: Option<String>,
    pub tags: Option<Vec<String>>,
    pub include_global: bool,
    pub limit: usize,
    pub content_budget: usize,
    pub include_taxonomy: bool,
}

/// Request to search memories by semantic similarity and keyword matching.
#[derive(Debug)]
pub struct SearchRequest {
    pub query: String,
    pub projects: Option<Vec<String>>,
    pub memory_type: Option<String>,
    pub tags: Option<Vec<String>>,
    pub include_global: bool,
    pub include_archived: bool,
    pub time: ResolvedTimeFilters,
    pub limit: u32,
    pub offset: u32,
    /// Per-request content truncation override. When `None`, uses config default.
    pub content_max_length: Option<u32>,
}

/// Resolved absolute time filters (all owned strings).
/// Created by resolving a mix of absolute and relative time inputs.
#[derive(Debug, Default)]
pub struct ResolvedTimeFilters {
    pub created_after: Option<String>,
    pub created_before: Option<String>,
    pub updated_after: Option<String>,
    pub updated_before: Option<String>,
}

impl ResolvedTimeFilters {
    /// Borrow as `TimeFilters` for passing to `FilterParams`.
    pub fn as_time_filters(&self) -> TimeFilters<'_> {
        TimeFilters {
            created_after: self.created_after.as_deref(),
            created_before: self.created_before.as_deref(),
            updated_after: self.updated_after.as_deref(),
            updated_before: self.updated_before.as_deref(),
        }
    }
}

// ── Response types ─────────────────────────────────────────────────────

/// Result of a store operation: the new ID plus threshold-filtered similar memories.
#[derive(Debug)]
pub struct StoredMemory {
    pub id: String,
    pub similar: Vec<(Memory, f64)>,
}

/// Result of a merge operation.
#[derive(Debug)]
pub struct MergedMemory {
    pub id: String,
    pub archived: Vec<String>,
    pub similar: Vec<(Memory, f64)>,
}

/// Result of a context search.
#[derive(Debug)]
pub struct ContextResult {
    pub hits: Vec<ContextHitInner>,
    pub taxonomy: Option<DiscoverResult>,
    pub truncated: bool,
}

/// A single hit from the context search.
#[derive(Debug)]
pub struct ContextHitInner {
    pub memory: Memory,
    pub score: f64,
    pub query_index: usize,
}

// ── Service ────────────────────────────────────────────────────────────

/// Orchestration layer owning embed -> DB -> post-process pipelines.
#[derive(Clone)]
pub struct MemoryService {
    db: Arc<Mutex<Database>>,
    embedder: Arc<dyn Embedder>,
    reranker: Option<Arc<dyn Reranker>>,
    config: Arc<ServiceConfig>,
}

impl MemoryService {
    pub fn new(
        db: Arc<Mutex<Database>>,
        embedder: Arc<dyn Embedder>,
        reranker: Option<Arc<dyn Reranker>>,
        config: ServiceConfig,
    ) -> Self {
        Self {
            db,
            embedder,
            reranker,
            config: Arc::new(config),
        }
    }

    // ── Internal helpers ────────────────────────────────────────────────

    /// Validate that content doesn't exceed the configured maximum size.
    fn validate_content_size(&self, content: &str) -> ServiceResult<()> {
        if content.len() > self.config.max_content_size {
            return Err(ServiceError::InvalidInput(format!(
                "content is {} bytes, max is {}",
                content.len(),
                self.config.max_content_size,
            )));
        }
        Ok(())
    }

    /// Embed a single document for storage (uses document-prefix semantics).
    async fn embed_document(&self, text: &str) -> ServiceResult<Vec<f32>> {
        let embedder = Arc::clone(&self.embedder);
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || embedder.embed_one(&text))
            .await
            .map_err(ServiceError::TaskJoin)?
            .map_err(ServiceError::Embedding)
    }

    /// Embed a single query for search (uses query-prefix semantics).
    async fn embed_query(&self, text: &str) -> ServiceResult<Vec<f32>> {
        let embedder = Arc::clone(&self.embedder);
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || embedder.embed_query(&text))
            .await
            .map_err(ServiceError::TaskJoin)?
            .map_err(ServiceError::Embedding)
    }

    /// Filter similar memories by the configured threshold.
    fn filter_by_threshold(&self, raw: Vec<(Memory, f64)>) -> Vec<(Memory, f64)> {
        raw.into_iter()
            .filter(|(_, score)| *score >= self.config.similar_threshold)
            .collect()
    }

    /// Run a blocking closure that operates on the database.
    /// Handles spawn_blocking + mutex acquisition + error mapping.
    async fn db_op<F, T>(&self, op: F) -> ServiceResult<T>
    where
        F: FnOnce(&Database) -> Result<T, DbError> + Send + 'static,
        T: Send + 'static,
    {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let db = db.lock().expect("db mutex poisoned");
            op(&db)
        })
        .await
        .map_err(ServiceError::TaskJoin)?
        .map_err(ServiceError::from)
    }

    // ── Compound operations ────────────────────────────────────────────

    /// Batched session-start search: run multiple queries, get deduplicated
    /// results within a content budget, optionally with the full taxonomy.
    pub async fn context(&self, req: ContextRequest) -> ServiceResult<ContextResult> {
        if req.queries.is_empty() {
            return Err(ServiceError::InvalidInput(
                "queries must not be empty".into(),
            ));
        }
        if req.queries.len() > 5 {
            return Err(ServiceError::InvalidInput(format!(
                "too many queries ({}, max 5)",
                req.queries.len()
            )));
        }
        if req.limit == 0 {
            return Err(ServiceError::InvalidInput(
                "limit must be greater than 0".into(),
            ));
        }
        if req.content_budget == 0 {
            return Err(ServiceError::InvalidInput(
                "content_budget must be greater than 0".into(),
            ));
        }
        // Embed all queries.
        let mut query_embeddings = Vec::with_capacity(req.queries.len());
        for query in &req.queries {
            query_embeddings.push(self.embed_query(query).await?);
        }

        // Run all searches + optional discover in a single blocking block.
        let config = Arc::clone(&self.config);
        let reranker = self.reranker.clone();
        let limit = req.limit;

        let (merged_hits, taxonomy) = self
            .db_op(move |db| {
                let projects_ref: Option<Vec<&str>> = req
                    .projects
                    .as_ref()
                    .map(|v| v.iter().map(|s| s.as_str()).collect());
                let tags_ref: Option<Vec<&str>> = req
                    .tags
                    .as_ref()
                    .map(|v| v.iter().map(|s| s.as_str()).collect());

                // Run each query's search, collecting into a HashMap for dedup.
                let mut best: std::collections::HashMap<String, (SearchHit, usize)> =
                    std::collections::HashMap::new();

                for (qi, (query, emb)) in
                    req.queries.iter().zip(query_embeddings.iter()).enumerate()
                {
                    let hits = db.search(&SearchParams {
                        query,
                        query_embedding: emb,
                        filter: FilterParams {
                            projects: projects_ref.as_deref(),
                            memory_type: req.memory_type.as_deref(),
                            tags: tags_ref.as_deref(),
                            include_global: req.include_global,
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
                let taxonomy = if req.include_taxonomy {
                    Some(db.discover()?)
                } else {
                    None
                };

                Ok((merged, taxonomy))
            })
            .await?;

        // Apply content budget.
        let mut total_chars: usize = 0;
        let mut truncated = false;
        let mut hits: Vec<ContextHitInner> = Vec::new();

        for (hit, qi) in merged_hits {
            let content_len = hit.memory.content.chars().count();
            if total_chars + content_len > req.content_budget && !hits.is_empty() {
                truncated = true;
                break;
            }
            total_chars += content_len;
            hits.push(ContextHitInner {
                memory: hit.memory,
                score: hit.score,
                query_index: qi,
            });
        }

        Ok(ContextResult {
            hits,
            taxonomy,
            truncated,
        })
    }

    /// Config accessor for transport-specific needs.
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    /// Database accessor for components that need direct DB access
    /// (e.g., building MCP instructions from taxonomy).
    pub fn db(&self) -> &Arc<Mutex<Database>> {
        &self.db
    }

    // ── Write operations ───────────────────────────────────────────────

    /// Store a new memory: embed content, persist to DB, find similar memories.
    pub async fn store(&self, req: StoreRequest) -> ServiceResult<StoredMemory> {
        self.validate_content_size(&req.content)?;
        let embedding = self.embed_document(&req.content).await?;

        let config = Arc::clone(&self.config);
        let (id, similar_raw) = self
            .db_op(move |db| {
                let projects_ref: Vec<&str> = req.projects.iter().map(|s| s.as_str()).collect();
                let tags_ref: Vec<&str> = req.tags.iter().map(|s| s.as_str()).collect();
                let links_ref: Vec<(&str, &str)> = req
                    .links
                    .iter()
                    .map(|(t, r)| (t.as_str(), r.as_str()))
                    .collect();

                let id = db.store(&StoreParams {
                    content: &req.content,
                    memory_type: req.memory_type.as_deref(),
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

                Ok((id, similar_raw))
            })
            .await?;

        Ok(StoredMemory {
            id,
            similar: self.filter_by_threshold(similar_raw),
        })
    }

    /// Merge multiple memories into one new memory. Source memories are archived.
    /// Returns the new ID, archived source IDs, and similar memories (excluding
    /// both the new and archived IDs).
    pub async fn merge(&self, req: MergeRequest) -> ServiceResult<MergedMemory> {
        if req.source_ids.is_empty() {
            return Err(ServiceError::InvalidInput(
                "source_ids must not be empty".into(),
            ));
        }
        if req.source_ids.len() > 20 {
            return Err(ServiceError::InvalidInput(format!(
                "too many source_ids ({}, max 20)",
                req.source_ids.len()
            )));
        }
        self.validate_content_size(&req.content)?;
        let embedding = self.embed_document(&req.content).await?;

        let config = Arc::clone(&self.config);
        let (merge_result, similar_raw) = self
            .db_op(move |db| {
                let source_ids_ref: Vec<&str> = req.source_ids.iter().map(|s| s.as_str()).collect();
                let projects_ref: Vec<&str> = req.projects.iter().map(|s| s.as_str()).collect();
                let tags_ref: Vec<&str> = req.tags.iter().map(|s| s.as_str()).collect();

                let result = db.merge(&MergeParams {
                    source_ids: &source_ids_ref,
                    content: &req.content,
                    memory_type: req.memory_type.as_deref(),
                    projects: &projects_ref,
                    tags: &tags_ref,
                    embedding: &embedding,
                })?;

                // Find similar, excluding the new ID and all archived source IDs.
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

                Ok((result, similar_raw))
            })
            .await?;

        Ok(MergedMemory {
            id: merge_result.id,
            archived: merge_result.archived,
            similar: self.filter_by_threshold(similar_raw),
        })
    }

    // ── Mutation pass-throughs (DB only) ─────────────────────────────

    /// Archive a memory (soft-delete).
    pub async fn archive(&self, id: &str) -> ServiceResult<ArchiveResult> {
        let id = id.to_owned();
        self.db_op(move |db| db.archive(&id)).await
    }

    /// Unarchive a memory (restore from soft-delete).
    pub async fn unarchive(&self, id: &str) -> ServiceResult<UnarchiveResult> {
        let id = id.to_owned();
        self.db_op(move |db| db.unarchive(&id)).await
    }

    /// Archive multiple memories. Skips user-facing errors (not-found, already-archived).
    pub async fn bulk_archive(&self, ids: &[String]) -> ServiceResult<Vec<ArchiveResult>> {
        let ids = ids.to_vec();
        self.db_op(move |db| {
            let mut results = Vec::new();
            for id in &ids {
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

    /// Unarchive multiple memories. Skips user-facing errors (not-found, not-archived).
    pub async fn bulk_unarchive(&self, ids: &[String]) -> ServiceResult<Vec<UnarchiveResult>> {
        let ids = ids.to_vec();
        self.db_op(move |db| {
            let mut results = Vec::new();
            for id in &ids {
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

    /// Create a directed link between two memories.
    pub async fn link(&self, source: &str, target: &str, relation: &str) -> ServiceResult<Link> {
        let source = source.to_owned();
        let target = target.to_owned();
        let relation = relation.to_owned();
        self.db_op(move |db| db.link(&source, &target, &relation))
            .await
    }

    /// Remove a link by its ID.
    pub async fn unlink_by_id(&self, id: &str) -> ServiceResult<usize> {
        let id = id.to_owned();
        self.db_op(move |db| db.unlink_by_id(&id)).await
    }

    /// Remove links matching source, target, and relation.
    pub async fn unlink_by_endpoints(
        &self,
        source: &str,
        target: &str,
        relation: &str,
    ) -> ServiceResult<usize> {
        let source = source.to_owned();
        let target = target.to_owned();
        let relation = relation.to_owned();
        self.db_op(move |db| db.unlink_by_endpoints(&source, &target, &relation))
            .await
    }

    // ── Update operations ──────────────────────────────────────────────

    /// Update an existing memory. Re-embeds automatically if content changes.
    pub async fn update(&self, req: UpdateRequest) -> ServiceResult<UpdateResult> {
        if let Some(ref content) = req.content {
            self.validate_content_size(content)?;
        }

        // Re-embed only if content changed.
        let embedding = match &req.content {
            Some(content) => Some(self.embed_document(content).await?),
            None => None,
        };

        self.db_op(move |db| {
            let projects_ref: Option<Vec<&str>> = req
                .projects
                .as_ref()
                .map(|v| v.iter().map(|s| s.as_str()).collect());
            let tags_ref: Option<Vec<&str>> = req
                .tags
                .as_ref()
                .map(|v| v.iter().map(|s| s.as_str()).collect());

            db.update(
                &req.id,
                &UpdateParams {
                    content: req.content.as_deref(),
                    memory_type: req.memory_type.as_deref(),
                    projects: projects_ref.as_deref(),
                    tags: tags_ref.as_deref(),
                    embedding: embedding.as_deref(),
                },
            )
        })
        .await
    }

    // ── Read operations ─────────────────────────────────────────────────

    /// Search memories by semantic similarity and keyword matching.
    /// Config values (rrf_k, reranker_threshold, content_max_length) are
    /// applied internally -- callers don't need to know about them.
    pub async fn search(&self, req: SearchRequest) -> ServiceResult<SearchResult> {
        let query_embedding = self.embed_query(&req.query).await?;

        let config = Arc::clone(&self.config);
        let reranker = self.reranker.clone();
        self.db_op(move |db| {
            let projects_ref: Option<Vec<&str>> = req
                .projects
                .as_ref()
                .map(|v| v.iter().map(|s| s.as_str()).collect());
            let tags_ref: Option<Vec<&str>> = req
                .tags
                .as_ref()
                .map(|v| v.iter().map(|s| s.as_str()).collect());

            let content_max_length = req.content_max_length.unwrap_or(config.content_max_length);

            db.search(&SearchParams {
                query: &req.query,
                query_embedding: &query_embedding,
                filter: FilterParams {
                    projects: projects_ref.as_deref(),
                    memory_type: req.memory_type.as_deref(),
                    tags: tags_ref.as_deref(),
                    include_global: req.include_global,
                    include_archived: req.include_archived,
                    time: req.time.as_time_filters(),
                },
                limit: req.limit,
                offset: req.offset,
                content_max_length: Some(content_max_length),
                rrf_k: config.rrf_k,
                reranker: reranker.as_deref(),
                reranker_threshold: config.reranker_threshold,
            })
        })
        .await
    }

    /// List memories with filters (no search query). Config-applied content truncation.
    pub async fn list(&self, req: ListRequest) -> ServiceResult<ListResult> {
        let config = Arc::clone(&self.config);
        self.db_op(move |db| {
            let projects_ref: Option<Vec<&str>> = req
                .projects
                .as_ref()
                .map(|v| v.iter().map(|s| s.as_str()).collect());
            let tags_ref: Option<Vec<&str>> = req
                .tags
                .as_ref()
                .map(|v| v.iter().map(|s| s.as_str()).collect());

            let content_max_length = req.content_max_length.unwrap_or(config.content_max_length);

            db.list(&ListParams {
                filter: FilterParams {
                    projects: projects_ref.as_deref(),
                    memory_type: req.memory_type.as_deref(),
                    tags: tags_ref.as_deref(),
                    include_global: req.include_global,
                    include_archived: req.include_archived,
                    time: req.time.as_time_filters(),
                },
                limit: req.limit,
                offset: req.offset,
                content_max_length: Some(content_max_length),
            })
        })
        .await
    }

    /// Fetch full details of specific memories by ID, including all links.
    pub async fn get(&self, ids: &[String]) -> ServiceResult<Vec<MemoryWithLinks>> {
        if ids.len() > 100 {
            return Err(ServiceError::InvalidInput(format!(
                "too many IDs ({}, max 100)",
                ids.len()
            )));
        }
        let ids = ids.to_vec();
        self.db_op(move |db| {
            let ids_ref: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
            db.get(&ids_ref)
        })
        .await
    }

    /// Get the full taxonomy: all projects, types, tags, relations, and stats.
    pub async fn discover(&self) -> ServiceResult<DiscoverResult> {
        self.db_op(|db| db.discover()).await
    }

    // ── Composable stage ───────────────────────────────────────────────

    /// Find memories similar to a given embedding, filtered by threshold.
    /// Excludes specified IDs from results.
    pub async fn find_similar(
        &self,
        embedding: &[f32],
        exclude_ids: &[&str],
        content_max_length: Option<u32>,
    ) -> ServiceResult<Vec<(Memory, f64)>> {
        let config = Arc::clone(&self.config);
        let embedding = embedding.to_vec();
        let exclude: Vec<String> = exclude_ids.iter().map(|s| s.to_string()).collect();
        let result = self
            .db_op(move |db| {
                let exclude_ref: Vec<&str> = exclude.iter().map(|s| s.as_str()).collect();
                db.find_similar(
                    &embedding,
                    config.similar_limit,
                    &exclude_ref,
                    content_max_length,
                )
            })
            .await?;

        Ok(self.filter_by_threshold(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
    use crate::embedding::MockEmbedder;

    /// Create a MemoryService with in-memory DB and mock embedder for tests.
    fn test_service() -> MemoryService {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let embedder = Arc::new(MockEmbedder::new(768));
        MemoryService::new(
            Arc::new(Mutex::new(db)),
            embedder,
            None,
            ServiceConfig::default(),
        )
    }

    /// Create a service with a tiny max_content_size for validation tests.
    fn test_service_small_content() -> MemoryService {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let embedder = Arc::new(MockEmbedder::new(768));
        MemoryService::new(
            Arc::new(Mutex::new(db)),
            embedder,
            None,
            ServiceConfig {
                max_content_size: 10, // tiny limit
                ..ServiceConfig::default()
            },
        )
    }

    #[tokio::test]
    async fn store_rejects_oversized_content_before_embedding() {
        let svc = test_service_small_content();

        let result = svc
            .store(StoreRequest {
                content: "this content is definitely longer than 10 bytes".into(),
                memory_type: None,
                projects: vec![],
                tags: vec![],
                links: vec![],
            })
            .await;

        let err = result.unwrap_err();
        assert!(err.is_user_facing());
        assert!(
            matches!(err, ServiceError::InvalidInput(_)),
            "expected InvalidInput, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("bytes"), "error should mention bytes: {msg}");
        assert!(msg.contains("10"), "error should mention the limit: {msg}");
    }

    #[tokio::test]
    async fn store_returns_id_and_threshold_filtered_similar() {
        let svc = test_service();

        // Store a memory, then store a similar one. The second store should
        // return the first as a similar memory (above default threshold).
        let first = svc
            .store(StoreRequest {
                content: "Rust error handling patterns".into(),
                memory_type: Some("fact".into()),
                projects: vec!["erinra".into()],
                tags: vec!["rust".into()],
                links: vec![],
            })
            .await
            .unwrap();

        assert!(!first.id.is_empty());
        // First store has no other memories to be similar to.
        assert!(first.similar.is_empty());

        // Store something with overlapping content.
        let second = svc
            .store(StoreRequest {
                content: "Rust error handling patterns and best practices".into(),
                memory_type: Some("fact".into()),
                projects: vec!["erinra".into()],
                tags: vec!["rust".into()],
                links: vec![],
            })
            .await
            .unwrap();

        assert!(!second.id.is_empty());
        assert_ne!(second.id, first.id);

        // The second store should find the first as similar — the mock embedder
        // produces deterministic vectors from content, so similar text produces
        // vectors with high cosine similarity. If threshold filters it out, the
        // list may be empty; verify the pipeline ran by checking the ID doesn't
        // appear as its own similar.
        for (mem, score) in &second.similar {
            assert_ne!(mem.id, second.id, "should not return self as similar");
            assert!(
                *score >= svc.config().similar_threshold,
                "similar score {score} should be >= threshold {}",
                svc.config().similar_threshold,
            );
        }
    }

    #[tokio::test]
    async fn search_applies_config_content_max_length() {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let embedder = Arc::new(MockEmbedder::new(768));
        let svc = MemoryService::new(
            Arc::new(Mutex::new(db)),
            embedder,
            None,
            ServiceConfig {
                content_max_length: 5, // truncate to 5 chars
                ..ServiceConfig::default()
            },
        );

        // Store a memory with long content.
        svc.store(StoreRequest {
            content: "This is a long piece of content for testing truncation".into(),
            memory_type: None,
            projects: vec![],
            tags: vec![],
            links: vec![],
        })
        .await
        .unwrap();

        // Search should apply content_max_length from config.
        let results = svc
            .search(SearchRequest {
                query: "long content".into(),
                projects: None,
                memory_type: None,
                tags: None,
                include_global: true,
                include_archived: false,
                time: ResolvedTimeFilters::default(),
                limit: 10,
                offset: 0,
                content_max_length: None,
            })
            .await
            .unwrap();

        assert!(
            !results.results.is_empty(),
            "should find at least one result"
        );
        for hit in &results.results {
            assert!(
                hit.memory.content.chars().count() <= 5,
                "content should be truncated to 5 chars, got {} chars: {:?}",
                hit.memory.content.chars().count(),
                hit.memory.content,
            );
        }
    }

    #[tokio::test]
    async fn search_per_request_content_max_length_overrides_config() {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let embedder = Arc::new(MockEmbedder::new(768));
        let svc = MemoryService::new(
            Arc::new(Mutex::new(db)),
            embedder,
            None,
            ServiceConfig {
                content_max_length: 1000, // large config default
                ..ServiceConfig::default()
            },
        );

        // Store a memory with long content.
        svc.store(StoreRequest {
            content: "This is a long piece of content for testing per-request truncation override"
                .into(),
            memory_type: None,
            projects: vec![],
            tags: vec![],
            links: vec![],
        })
        .await
        .unwrap();

        // Search with per-request content_max_length=10 should truncate to 10, not 1000.
        let results = svc
            .search(SearchRequest {
                query: "long content".into(),
                projects: None,
                memory_type: None,
                tags: None,
                include_global: true,
                include_archived: false,
                time: ResolvedTimeFilters::default(),
                limit: 10,
                offset: 0,
                content_max_length: Some(10),
            })
            .await
            .unwrap();

        assert!(
            !results.results.is_empty(),
            "should find at least one result"
        );
        for hit in &results.results {
            assert!(
                hit.memory.content.chars().count() <= 10,
                "content should be truncated to 10 chars (per-request override), got {} chars: {:?}",
                hit.memory.content.chars().count(),
                hit.memory.content,
            );
            assert!(hit.memory.truncated, "truncated flag should be set");
        }
    }

    #[tokio::test]
    async fn list_per_request_content_max_length_overrides_config() {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let embedder = Arc::new(MockEmbedder::new(768));
        let svc = MemoryService::new(
            Arc::new(Mutex::new(db)),
            embedder,
            None,
            ServiceConfig {
                content_max_length: 1000, // large config default
                ..ServiceConfig::default()
            },
        );

        // Store a memory with long content.
        svc.store(StoreRequest {
            content: "This is a long piece of content for testing list truncation override".into(),
            memory_type: None,
            projects: vec![],
            tags: vec![],
            links: vec![],
        })
        .await
        .unwrap();

        // List with per-request content_max_length=8 should truncate to 8, not 1000.
        let result = svc
            .list(ListRequest {
                projects: None,
                memory_type: None,
                tags: None,
                include_global: true,
                include_archived: false,
                time: ResolvedTimeFilters::default(),
                limit: 20,
                offset: 0,
                content_max_length: Some(8),
            })
            .await
            .unwrap();

        assert_eq!(result.total, 1);
        let mem = &result.memories[0];
        assert!(
            mem.content.chars().count() <= 8,
            "content should be truncated to 8 chars (per-request override), got {}: {:?}",
            mem.content.chars().count(),
            mem.content,
        );
        assert!(mem.truncated, "truncated flag should be set");
    }

    #[tokio::test]
    async fn update_with_content_triggers_reembedding() {
        let svc = test_service();

        // Store a memory.
        let stored = svc
            .store(StoreRequest {
                content: "original content".into(),
                memory_type: None,
                projects: vec![],
                tags: vec![],
                links: vec![],
            })
            .await
            .unwrap();

        // Update its content — this should re-embed.
        let result = svc
            .update(UpdateRequest {
                id: stored.id.clone(),
                content: Some("completely new and different content".into()),
                memory_type: FieldUpdate::NoChange,
                projects: None,
                tags: None,
            })
            .await
            .unwrap();

        assert_eq!(result.id, stored.id);
        assert!(!result.updated_at.is_empty());

        // Verify the content was actually updated by searching for it.
        let search = svc
            .search(SearchRequest {
                query: "completely new and different".into(),
                projects: None,
                memory_type: None,
                tags: None,
                include_global: true,
                include_archived: false,
                time: ResolvedTimeFilters::default(),
                limit: 10,
                offset: 0,
                content_max_length: None,
            })
            .await
            .unwrap();

        assert!(
            search.results.iter().any(|hit| hit.memory.id == stored.id),
            "updated memory should be findable by new content"
        );
    }

    #[tokio::test]
    async fn update_without_content_skips_embedding() {
        let svc = test_service();

        // Store a memory.
        let stored = svc
            .store(StoreRequest {
                content: "some content".into(),
                memory_type: Some("fact".into()),
                projects: vec![],
                tags: vec![],
                links: vec![],
            })
            .await
            .unwrap();

        // Update only metadata — no content change, no re-embedding needed.
        let result = svc
            .update(UpdateRequest {
                id: stored.id.clone(),
                content: None,
                memory_type: FieldUpdate::Set("pattern".into()),
                projects: None,
                tags: Some(vec!["new-tag".into()]),
            })
            .await
            .unwrap();

        assert_eq!(result.id, stored.id);
        assert!(!result.updated_at.is_empty());
    }

    #[tokio::test]
    async fn merge_archives_sources_and_returns_similar_excluding_archived() {
        let svc = test_service();

        // Store two source memories.
        let src1 = svc
            .store(StoreRequest {
                content: "first source memory about Rust".into(),
                memory_type: Some("fact".into()),
                projects: vec!["erinra".into()],
                tags: vec![],
                links: vec![],
            })
            .await
            .unwrap();

        let src2 = svc
            .store(StoreRequest {
                content: "second source memory about Rust".into(),
                memory_type: Some("fact".into()),
                projects: vec!["erinra".into()],
                tags: vec![],
                links: vec![],
            })
            .await
            .unwrap();

        // Merge them.
        let merged = svc
            .merge(MergeRequest {
                source_ids: vec![src1.id.clone(), src2.id.clone()],
                content: "combined memory about Rust patterns".into(),
                memory_type: Some("fact".into()),
                projects: vec!["erinra".into()],
                tags: vec!["rust".into()],
            })
            .await
            .unwrap();

        assert!(!merged.id.is_empty());
        assert_ne!(merged.id, src1.id);
        assert_ne!(merged.id, src2.id);

        // The archived list should contain both source IDs.
        assert!(merged.archived.contains(&src1.id));
        assert!(merged.archived.contains(&src2.id));

        // Similar results should not contain the merged ID or archived source IDs.
        for (mem, _) in &merged.similar {
            assert_ne!(mem.id, merged.id, "should not return merged ID as similar");
            assert_ne!(
                mem.id, src1.id,
                "should not return archived source as similar"
            );
            assert_ne!(
                mem.id, src2.id,
                "should not return archived source as similar"
            );
        }
    }

    #[tokio::test]
    async fn find_similar_standalone_with_exclusion_and_threshold() {
        let svc = test_service();

        // Store a few memories.
        let m1 = svc
            .store(StoreRequest {
                content: "Rust async patterns with tokio".into(),
                memory_type: None,
                projects: vec![],
                tags: vec![],
                links: vec![],
            })
            .await
            .unwrap();

        // Store a second memory (needed so find_similar has something to return).
        svc.store(StoreRequest {
            content: "Python async patterns with asyncio".into(),
            memory_type: None,
            projects: vec![],
            tags: vec![],
            links: vec![],
        })
        .await
        .unwrap();

        // Get an embedding for a query.
        let embedder = Arc::clone(&svc.embedder);
        let emb = tokio::task::spawn_blocking(move || embedder.embed_query("async patterns"))
            .await
            .unwrap()
            .unwrap();

        // Find similar, excluding m1.
        let results = svc.find_similar(&emb, &[&m1.id], None).await.unwrap();

        // m1 should be excluded.
        for (mem, score) in &results {
            assert_ne!(mem.id, m1.id, "excluded ID should not appear in results");
            assert!(
                *score >= svc.config().similar_threshold,
                "score {score} below threshold {}",
                svc.config().similar_threshold,
            );
        }
    }

    #[tokio::test]
    async fn archive_and_unarchive_pass_through() {
        let svc = test_service();

        // Store a memory.
        let stored = svc
            .store(StoreRequest {
                content: "memory to archive".into(),
                memory_type: None,
                projects: vec![],
                tags: vec![],
                links: vec![],
            })
            .await
            .unwrap();

        // Archive it.
        let archive_result = svc.archive(&stored.id).await.unwrap();
        assert_eq!(archive_result.id, stored.id);
        assert!(!archive_result.archived_at.is_empty());

        // Unarchive it.
        let unarchive_result = svc.unarchive(&stored.id).await.unwrap();
        assert_eq!(unarchive_result.id, stored.id);
        assert!(!unarchive_result.updated_at.is_empty());
    }

    #[tokio::test]
    async fn bulk_archive_skips_user_facing_errors() {
        let svc = test_service();

        // Store one memory.
        let stored = svc
            .store(StoreRequest {
                content: "bulk test memory".into(),
                memory_type: None,
                projects: vec![],
                tags: vec![],
                links: vec![],
            })
            .await
            .unwrap();

        // Bulk archive with a valid ID and a non-existent ID.
        // The non-existent one should be skipped (user-facing error).
        let results = svc
            .bulk_archive(&[stored.id.clone(), "nonexistent-id".into()])
            .await
            .unwrap();

        // Only the valid one should succeed.
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, stored.id);
    }

    #[tokio::test]
    async fn discover_returns_taxonomy() {
        let svc = test_service();

        // Store a memory with project and tags.
        svc.store(StoreRequest {
            content: "some memory".into(),
            memory_type: Some("fact".into()),
            projects: vec!["erinra".into()],
            tags: vec!["rust".into()],
            links: vec![],
        })
        .await
        .unwrap();

        let result = svc.discover().await.unwrap();

        assert_eq!(result.stats.total_memories, 1);
        assert!(
            result.projects.iter().any(|nc| nc.name == "erinra"),
            "should contain project 'erinra'"
        );
        assert!(
            result.types.iter().any(|nc| nc.name == "fact"),
            "should contain type 'fact'"
        );
        assert!(
            result.tags.iter().any(|nc| nc.name == "rust"),
            "should contain tag 'rust'"
        );
    }

    #[tokio::test]
    async fn get_retrieves_memories_with_links() {
        let svc = test_service();

        // Store two memories and link them.
        let m1 = svc
            .store(StoreRequest {
                content: "memory one".into(),
                memory_type: None,
                projects: vec![],
                tags: vec![],
                links: vec![],
            })
            .await
            .unwrap();

        let m2 = svc
            .store(StoreRequest {
                content: "memory two".into(),
                memory_type: None,
                projects: vec![],
                tags: vec![],
                links: vec![],
            })
            .await
            .unwrap();

        svc.link(&m1.id, &m2.id, "related_to").await.unwrap();

        // Get both memories.
        let results = svc.get(&[m1.id.clone(), m2.id.clone()]).await.unwrap();
        assert_eq!(results.len(), 2);

        // m1 should have an outgoing link to m2.
        let r1 = results.iter().find(|r| r.memory.id == m1.id).unwrap();
        assert_eq!(r1.outgoing_links.len(), 1);
        assert_eq!(r1.outgoing_links[0].target_id, m2.id);

        // m2 should have an incoming link from m1.
        let r2 = results.iter().find(|r| r.memory.id == m2.id).unwrap();
        assert_eq!(r2.incoming_links.len(), 1);
        assert_eq!(r2.incoming_links[0].source_id, m1.id);
    }

    #[tokio::test]
    async fn link_and_unlink_operations() {
        let svc = test_service();

        // Store two memories.
        let m1 = svc
            .store(StoreRequest {
                content: "first memory for linking".into(),
                memory_type: None,
                projects: vec![],
                tags: vec![],
                links: vec![],
            })
            .await
            .unwrap();

        let m2 = svc
            .store(StoreRequest {
                content: "second memory for linking".into(),
                memory_type: None,
                projects: vec![],
                tags: vec![],
                links: vec![],
            })
            .await
            .unwrap();

        // Create a link.
        let link = svc.link(&m1.id, &m2.id, "related_to").await.unwrap();

        assert!(!link.id.is_empty());
        assert_eq!(link.source_id, m1.id);
        assert_eq!(link.target_id, m2.id);
        assert_eq!(link.relation, "related_to");

        // Unlink by ID.
        let removed = svc.unlink_by_id(&link.id).await.unwrap();
        assert_eq!(removed, 1);

        // Create another link, then unlink by endpoints.
        let link2 = svc.link(&m1.id, &m2.id, "supersedes").await.unwrap();
        assert!(!link2.id.is_empty());

        let removed2 = svc
            .unlink_by_endpoints(&m1.id, &m2.id, "supersedes")
            .await
            .unwrap();
        assert_eq!(removed2, 1);
    }

    #[tokio::test]
    async fn list_applies_config_content_max_length() {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let embedder = Arc::new(MockEmbedder::new(768));
        let svc = MemoryService::new(
            Arc::new(Mutex::new(db)),
            embedder,
            None,
            ServiceConfig {
                content_max_length: 8,
                ..ServiceConfig::default()
            },
        );

        // Store a memory with long content.
        svc.store(StoreRequest {
            content: "This is long content that should be truncated by list".into(),
            memory_type: Some("fact".into()),
            projects: vec!["proj".into()],
            tags: vec![],
            links: vec![],
        })
        .await
        .unwrap();

        // List should apply content_max_length from config.
        let result = svc
            .list(ListRequest {
                projects: None,
                memory_type: None,
                tags: None,
                include_global: true,
                include_archived: false,
                time: ResolvedTimeFilters::default(),
                limit: 20,
                offset: 0,
                content_max_length: None,
            })
            .await
            .unwrap();

        assert_eq!(result.total, 1);
        assert_eq!(result.memories.len(), 1);
        let mem = &result.memories[0];
        assert!(
            mem.content.chars().count() <= 8,
            "content should be truncated to 8 chars, got {}: {:?}",
            mem.content.chars().count(),
            mem.content,
        );
        assert!(mem.truncated, "truncated flag should be set");
    }

    #[tokio::test]
    async fn context_multi_query_with_content_budget() {
        let svc = test_service();

        // Store several memories with distinct content.
        svc.store(StoreRequest {
            content: "Rust error handling with Result and anyhow".into(),
            memory_type: Some("fact".into()),
            projects: vec!["erinra".into()],
            tags: vec![],
            links: vec![],
        })
        .await
        .unwrap();

        svc.store(StoreRequest {
            content: "Python exception handling with try except".into(),
            memory_type: Some("fact".into()),
            projects: vec!["other".into()],
            tags: vec![],
            links: vec![],
        })
        .await
        .unwrap();

        svc.store(StoreRequest {
            content: "Go error handling with error interface".into(),
            memory_type: Some("fact".into()),
            projects: vec![],
            tags: vec![],
            links: vec![],
        })
        .await
        .unwrap();

        // Use a small content budget so not all hits fit.
        let result = svc
            .context(ContextRequest {
                queries: vec!["error handling".into(), "exception patterns".into()],
                projects: None,
                memory_type: None,
                tags: None,
                include_global: true,
                limit: 10,
                content_budget: 50, // small budget
                include_taxonomy: true,
            })
            .await
            .unwrap();

        // Should have some hits.
        assert!(!result.hits.is_empty(), "should have at least one hit");

        // Each hit should have a valid query_index.
        for hit in &result.hits {
            assert!(
                hit.query_index < 2,
                "query_index {} should be < 2",
                hit.query_index,
            );
            assert!(hit.score > 0.0, "score should be positive");
        }

        // Taxonomy should be present since we asked for it.
        assert!(result.taxonomy.is_some(), "taxonomy should be included");

        // Content budget: total chars should be reasonable.
        let total_chars: usize = result
            .hits
            .iter()
            .map(|h| h.memory.content.chars().count())
            .sum();
        // With a 50-char budget, at most ~50 chars worth of hits should be included.
        // The first hit always gets in even if it exceeds budget, so allow some slack.
        if result.hits.len() > 1 {
            assert!(
                total_chars <= 100,
                "total chars {total_chars} should be close to content_budget=50"
            );
        }
    }

    #[test]
    fn service_config_from_config_maps_all_fields() {
        let config = Config {
            store: crate::config::StoreConfig {
                similar_limit: 7,
                similar_threshold: 0.42,
                content_max_length: 999,
                max_content_size: 2048,
            },
            search: crate::config::SearchConfig { rrf_k: 99 },
            reranker: crate::config::RerankerConfig {
                threshold: -0.5,
                ..Default::default()
            },
            ..Default::default()
        };

        let sc = ServiceConfig::from(&config);

        assert_eq!(sc.similar_limit, 7);
        assert_eq!(sc.similar_threshold, 0.42);
        assert_eq!(sc.content_max_length, 999);
        assert_eq!(sc.rrf_k, 99);
        assert_eq!(sc.max_content_size, 2048);
        assert_eq!(sc.reranker_threshold, -0.5);
    }

    #[test]
    fn service_config_from_default_config_matches_default_service_config() {
        let from_config = ServiceConfig::from(&Config::default());
        let default = ServiceConfig::default();

        assert_eq!(from_config.similar_limit, default.similar_limit);
        assert_eq!(from_config.similar_threshold, default.similar_threshold);
        assert_eq!(from_config.content_max_length, default.content_max_length);
        assert_eq!(from_config.rrf_k, default.rrf_k);
        assert_eq!(from_config.max_content_size, default.max_content_size);
        assert_eq!(from_config.reranker_threshold, default.reranker_threshold);
    }
}
