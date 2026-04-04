//! MCP input types, response types, and From conversions.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::db::types::*;

// ── Serde helpers ───────────────────────────────────────────────────────

/// Deserialize a JSON field that can be absent, null, or a value.
///
/// - Absent → `None` (field not present in JSON)
/// - `null` → `Some(None)` (explicitly set to null)
/// - `"value"` → `Some(Some("value"))`
///
/// Requires `#[serde(default)]` on the field so absent maps to `None`.
pub(crate) fn deserialize_nullable_field<'de, D, T>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    // If the deserializer is called, the field was present in JSON.
    // Deserialize the inner Option<T> to distinguish null vs value.
    let value: Option<T> = Option::deserialize(deserializer)?;
    Ok(Some(value))
}

/// JSON Schema for a nullable string field (string | null).
pub(crate) fn nullable_string_schema(
    _generator: &mut schemars::SchemaGenerator,
) -> schemars::Schema {
    let mut map = serde_json::Map::new();
    map.insert("type".to_string(), serde_json::json!(["string", "null"]));
    map.insert(
        "description".to_string(),
        serde_json::json!("New memory type. Omit to leave unchanged, set to null to clear, or provide a string to replace."),
    );
    map.into()
}

pub(crate) fn is_false(b: &bool) -> bool {
    !b
}

// ── Configuration ───────────────────────────────────────────────────────

/// Server-level configuration for MCP tool behavior.
#[derive(Clone)]
pub struct ServerConfig {
    /// Number of similar memories returned by `store` and `merge`.
    pub similar_limit: usize,
    /// Minimum cosine similarity to include in similar results.
    pub similar_threshold: f64,
    /// Default content truncation for store/search/list responses.
    pub content_max_length: u32,
    /// RRF constant for hybrid search result merging.
    pub rrf_k: u32,
    /// Maximum content size in bytes (enforced on store/update).
    pub max_content_size: usize,
    /// Minimum reranker score to include in search results.
    pub reranker_threshold: f64,
}

impl Default for ServerConfig {
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

// ── Tool input types ────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub(crate) struct StoreInput {
    /// The memory content to store.
    pub content: String,
    /// Project names this memory belongs to.
    #[serde(default)]
    pub projects: Vec<String>,
    /// Memory type (e.g. pattern, decision, preference, bug-fix).
    #[serde(rename = "type")]
    pub memory_type: Option<String>,
    /// Freeform tags for categorization.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Links to create from this memory to existing memories.
    #[serde(default)]
    pub links: Vec<StoreLinkInput>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct StoreLinkInput {
    /// UUID of the target memory.
    pub target_id: String,
    /// Relationship type (e.g. related_to, caused_by, context_for).
    pub relation: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct UpdateInput {
    /// UUID of the memory to update.
    pub id: String,
    /// New content (triggers re-embedding).
    pub content: Option<String>,
    /// New memory type. Omit to leave unchanged, set to null to clear, or
    /// provide a string to replace.
    #[serde(
        rename = "type",
        default,
        deserialize_with = "deserialize_nullable_field"
    )]
    #[schemars(schema_with = "nullable_string_schema")]
    pub memory_type: Option<Option<String>>,
    /// New project list. Replaces all existing projects if provided.
    pub projects: Option<Vec<String>>,
    /// New tag list. Replaces all existing tags if provided.
    pub tags: Option<Vec<String>>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct ArchiveInput {
    /// UUID of the memory to archive.
    pub id: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct MergeInput {
    /// UUIDs of source memories to merge. All will be archived.
    pub source_ids: Vec<String>,
    /// The merged content (you provide the combined text).
    pub content: String,
    /// Project names for the merged memory.
    #[serde(default)]
    pub projects: Vec<String>,
    /// Memory type for the merged memory.
    #[serde(rename = "type")]
    pub memory_type: Option<String>,
    /// Tags for the merged memory.
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct LinkInput {
    /// UUID of the source memory.
    pub source_id: String,
    /// UUID of the target memory.
    pub target_id: String,
    /// Relationship type (e.g. supersedes, related_to, caused_by, context_for).
    pub relation: String,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct UnlinkInput {
    /// Link UUID (option A: remove by link ID).
    pub id: Option<String>,
    /// Source memory UUID (option B: remove by endpoints).
    pub source_id: Option<String>,
    /// Target memory UUID (option B: remove by endpoints).
    pub target_id: Option<String>,
    /// Relationship type (option B: remove by endpoints).
    pub relation: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct SearchInput {
    /// Search query text (natural language).
    pub query: String,
    /// Filter to memories in any of these projects (OR). Omit for no project filter.
    pub projects: Option<Vec<String>>,
    /// Filter to memories of this type (exact match).
    #[serde(rename = "type")]
    pub memory_type: Option<String>,
    /// Filter to memories with all of these tags (AND).
    pub tags: Option<Vec<String>>,
    /// Include memories with no projects. Default: true.
    pub include_global: Option<bool>,
    /// Include archived memories. Default: false.
    pub include_archived: Option<bool>,
    /// Only include memories created on or after this ISO 8601 timestamp.
    pub created_after: Option<String>,
    /// Only include memories created before this ISO 8601 timestamp.
    pub created_before: Option<String>,
    /// Only include memories updated on or after this ISO 8601 timestamp.
    pub updated_after: Option<String>,
    /// Only include memories updated before this ISO 8601 timestamp.
    pub updated_before: Option<String>,
    /// Only include memories created at most this many days ago.
    pub created_max_age_days: Option<u32>,
    /// Only include memories created at least this many days ago.
    pub created_min_age_days: Option<u32>,
    /// Only include memories updated at most this many days ago.
    pub updated_max_age_days: Option<u32>,
    /// Only include memories updated at least this many days ago.
    pub updated_min_age_days: Option<u32>,
    /// Maximum number of results. Default: 10.
    pub limit: Option<u32>,
    /// Number of results to skip. Default: 0.
    pub offset: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct GetInput {
    /// UUIDs of memories to fetch.
    pub ids: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct ListInput {
    /// Filter to memories in any of these projects (OR).
    pub projects: Option<Vec<String>>,
    /// Filter to memories of this type (exact match).
    #[serde(rename = "type")]
    pub memory_type: Option<String>,
    /// Filter to memories with all of these tags (AND).
    pub tags: Option<Vec<String>>,
    /// Only include memories created on or after this ISO 8601 timestamp.
    pub created_after: Option<String>,
    /// Only include memories created before this ISO 8601 timestamp.
    pub created_before: Option<String>,
    /// Only include memories updated on or after this ISO 8601 timestamp.
    pub updated_after: Option<String>,
    /// Only include memories updated before this ISO 8601 timestamp.
    pub updated_before: Option<String>,
    /// Only include memories created at most this many days ago.
    pub created_max_age_days: Option<u32>,
    /// Only include memories created at least this many days ago.
    pub created_min_age_days: Option<u32>,
    /// Only include memories updated at most this many days ago.
    pub updated_max_age_days: Option<u32>,
    /// Only include memories updated at least this many days ago.
    pub updated_min_age_days: Option<u32>,
    /// Include memories with no projects. Default: true.
    pub include_global: Option<bool>,
    /// Include archived memories. Default: false.
    pub include_archived: Option<bool>,
    /// Maximum number of results. Default: 20.
    pub limit: Option<u32>,
    /// Number of results to skip. Default: 0.
    pub offset: Option<u32>,
}

// ── Tool response types ─────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub(crate) struct StoreResponse {
    pub id: String,
    pub similar: Vec<SimilarMemoryResponse>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SimilarMemoryResponse {
    pub id: String,
    pub content: String,
    pub projects: Vec<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
    pub tags: Vec<String>,
    pub similarity: f64,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

#[derive(Serialize)]
pub(crate) struct MergeResponse {
    pub id: String,
    pub archived: Vec<String>,
    pub similar: Vec<SimilarMemoryResponse>,
}

#[derive(Serialize)]
pub(crate) struct UnlinkResponse {
    pub removed: usize,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SearchHitResponse {
    pub id: String,
    pub content: String,
    pub projects: Vec<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
    pub tags: Vec<String>,
    pub links: LinksResponse,
    pub score: f64,
    pub created_at: String,
    pub access_count: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct LinksResponse {
    pub outgoing: Vec<Link>,
    pub incoming: Vec<Link>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct MemoryFullResponse {
    pub id: String,
    pub content: String,
    pub projects: Vec<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
    pub tags: Vec<String>,
    pub links: LinksResponse,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
    pub access_count: i64,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ListResponse {
    pub memories: Vec<MemoryResponseSerde>,
    pub total: i64,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct MemoryResponseSerde {
    pub id: String,
    pub content: String,
    pub projects: Vec<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
    pub tags: Vec<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

// ── From conversions ────────────────────────────────────────────────────

impl From<MemoryWithLinks> for MemoryFullResponse {
    fn from(mwl: MemoryWithLinks) -> Self {
        Self {
            id: mwl.memory.id,
            content: mwl.memory.content,
            projects: mwl.memory.projects,
            memory_type: mwl.memory.memory_type,
            tags: mwl.memory.tags,
            links: LinksResponse {
                outgoing: mwl.outgoing_links,
                incoming: mwl.incoming_links,
            },
            created_at: mwl.memory.created_at,
            updated_at: mwl.memory.updated_at,
            archived_at: mwl.memory.archived_at,
            access_count: mwl.memory.access_count,
        }
    }
}

impl From<Memory> for MemoryResponseSerde {
    fn from(mem: Memory) -> Self {
        Self {
            id: mem.id,
            content: mem.content,
            projects: mem.projects,
            memory_type: mem.memory_type,
            tags: mem.tags,
            created_at: mem.created_at,
            truncated: mem.truncated,
        }
    }
}

impl From<SearchHit> for SearchHitResponse {
    fn from(hit: SearchHit) -> Self {
        Self {
            id: hit.memory.id,
            content: hit.memory.content,
            projects: hit.memory.projects,
            memory_type: hit.memory.memory_type,
            tags: hit.memory.tags,
            links: LinksResponse {
                outgoing: hit.outgoing_links,
                incoming: hit.incoming_links,
            },
            score: hit.score,
            created_at: hit.memory.created_at,
            access_count: hit.memory.access_count,
            truncated: hit.memory.truncated,
        }
    }
}

// ── Context tool types ─────────────────────────────────────────────────

/// Input for the `context` tool — batched session-start search.
#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ContextInput {
    /// 1-5 search queries to run in parallel.
    pub queries: Vec<String>,
    /// Filter: include memories in these projects (OR).
    pub projects: Option<Vec<String>>,
    /// Filter: exact memory type match.
    #[serde(rename = "type")]
    pub memory_type: Option<String>,
    /// Filter: memories with all these tags (AND).
    pub tags: Option<Vec<String>>,
    /// Include memories without a project. Default: true.
    pub include_global: Option<bool>,
    /// Include taxonomy (projects, types, tags, relations, stats) in response. Default: false.
    pub include_taxonomy: Option<bool>,
    /// Max total characters across all returned content. Default: 2000.
    pub content_budget: Option<u32>,
    /// Max total results across all queries. Default: 10.
    pub limit: Option<u32>,
}

/// A single result in the context response.
/// Compact format: omits links and access_count (available via `get`) to reduce payload size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ContextHit {
    pub id: String,
    pub content: String,
    pub projects: Vec<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
    pub tags: Vec<String>,
    pub score: f64,
    pub query_index: usize,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// Response from the `context` tool.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ContextResponse {
    pub memories: Vec<ContextHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taxonomy: Option<DiscoverResult>,
    /// True if some results were omitted due to content_budget or limit.
    /// The first result is always included even if it alone exceeds the budget.
    pub truncated: bool,
}
