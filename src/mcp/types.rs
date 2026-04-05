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

// ── Service response conversions ──────────────────────────────────────

impl From<crate::service::StoredMemory> for StoreResponse {
    fn from(sm: crate::service::StoredMemory) -> Self {
        Self {
            id: sm.id,
            similar: sm
                .similar
                .into_iter()
                .map(|(mem, score)| SimilarMemoryResponse {
                    id: mem.id,
                    content: mem.content,
                    projects: mem.projects,
                    memory_type: mem.memory_type,
                    tags: mem.tags,
                    similarity: score,
                    created_at: mem.created_at,
                    truncated: mem.truncated,
                })
                .collect(),
        }
    }
}

impl From<crate::service::MergedMemory> for MergeResponse {
    fn from(mm: crate::service::MergedMemory) -> Self {
        Self {
            id: mm.id,
            archived: mm.archived,
            similar: mm
                .similar
                .into_iter()
                .map(|(mem, score)| SimilarMemoryResponse {
                    id: mem.id,
                    content: mem.content,
                    projects: mem.projects,
                    memory_type: mem.memory_type,
                    tags: mem.tags,
                    similarity: score,
                    created_at: mem.created_at,
                    truncated: mem.truncated,
                })
                .collect(),
        }
    }
}

impl From<crate::service::ContextResult> for ContextResponse {
    fn from(cr: crate::service::ContextResult) -> Self {
        Self {
            memories: cr
                .hits
                .into_iter()
                .map(|hit| ContextHit {
                    id: hit.memory.id,
                    content: hit.memory.content,
                    projects: hit.memory.projects,
                    memory_type: hit.memory.memory_type,
                    tags: hit.memory.tags,
                    score: hit.score,
                    query_index: hit.query_index,
                    created_at: hit.memory.created_at,
                    truncated: hit.memory.truncated,
                })
                .collect(),
            taxonomy: cr.taxonomy,
            truncated: cr.truncated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{ContextHitInner, ContextResult, MergedMemory, StoredMemory};

    fn sample_memory(id: &str, content: &str) -> Memory {
        Memory {
            id: id.to_string(),
            content: content.to_string(),
            memory_type: Some("pattern".to_string()),
            projects: vec!["proj-a".to_string()],
            tags: vec!["rust".to_string()],
            created_at: "2025-06-15T00:00:00.000Z".to_string(),
            updated_at: "2025-06-15T00:00:00.000Z".to_string(),
            archived_at: None,
            last_accessed_at: None,
            access_count: 0,
            truncated: false,
        }
    }

    // ── Behavior 1: StoreInput → StoreRequest + StoredMemory → StoreResponse ──

    #[test]
    fn store_input_converts_to_store_request() {
        use crate::service::StoreRequest;

        let input = StoreInput {
            content: "some content".to_string(),
            projects: vec!["proj-a".to_string()],
            memory_type: Some("pattern".to_string()),
            tags: vec!["rust".to_string()],
            links: vec![
                StoreLinkInput {
                    target_id: "target-1".to_string(),
                    relation: "related_to".to_string(),
                },
                StoreLinkInput {
                    target_id: "target-2".to_string(),
                    relation: "caused_by".to_string(),
                },
            ],
        };

        let req = StoreRequest::from(input);
        assert_eq!(req.content, "some content");
        assert_eq!(req.projects, vec!["proj-a"]);
        assert_eq!(req.memory_type, Some("pattern".to_string()));
        assert_eq!(req.tags, vec!["rust"]);
        assert_eq!(
            req.links,
            vec![
                ("target-1".to_string(), "related_to".to_string()),
                ("target-2".to_string(), "caused_by".to_string()),
            ]
        );
    }

    #[test]
    fn stored_memory_converts_to_store_response() {
        let sm = StoredMemory {
            id: "new-id".to_string(),
            similar: vec![
                (sample_memory("sim-1", "similar content 1"), 0.92),
                (sample_memory("sim-2", "similar content 2"), 0.85),
            ],
        };

        let resp = StoreResponse::from(sm);
        assert_eq!(resp.id, "new-id");
        assert_eq!(resp.similar.len(), 2);
        assert_eq!(resp.similar[0].id, "sim-1");
        assert_eq!(resp.similar[0].content, "similar content 1");
        assert_eq!(resp.similar[0].similarity, 0.92);
        assert_eq!(resp.similar[0].projects, vec!["proj-a"]);
        assert_eq!(resp.similar[0].memory_type, Some("pattern".to_string()));
        assert_eq!(resp.similar[0].tags, vec!["rust"]);
        assert_eq!(resp.similar[0].created_at, "2025-06-15T00:00:00.000Z");
        assert!(!resp.similar[0].truncated);
    }

    #[test]
    fn stored_memory_with_truncated_similar() {
        let mut mem = sample_memory("sim-1", "truncated");
        mem.truncated = true;

        let sm = StoredMemory {
            id: "new-id".to_string(),
            similar: vec![(mem, 0.90)],
        };

        let resp = StoreResponse::from(sm);
        assert!(resp.similar[0].truncated);
    }

    // ── Behavior 4: UpdateInput → UpdateRequest ──

    #[test]
    fn update_input_converts_to_update_request() {
        use crate::db::types::FieldUpdate;
        use crate::service::UpdateRequest;

        // Test: memory_type Some(Some("decision")) → Set("decision")
        let input = UpdateInput {
            id: "mem-1".to_string(),
            content: Some("new content".to_string()),
            memory_type: Some(Some("decision".to_string())),
            projects: Some(vec!["proj-b".to_string()]),
            tags: Some(vec!["go".to_string()]),
        };

        let req = UpdateRequest::from(input);
        assert_eq!(req.id, "mem-1");
        assert_eq!(req.content, Some("new content".to_string()));
        assert_eq!(req.memory_type, FieldUpdate::Set("decision".to_string()));
        assert_eq!(req.projects, Some(vec!["proj-b".to_string()]));
        assert_eq!(req.tags, Some(vec!["go".to_string()]));
    }

    #[test]
    fn update_input_none_type_is_no_change() {
        use crate::db::types::FieldUpdate;
        use crate::service::UpdateRequest;

        let input = UpdateInput {
            id: "mem-1".to_string(),
            content: None,
            memory_type: None, // absent → NoChange
            projects: None,
            tags: None,
        };

        let req = UpdateRequest::from(input);
        assert_eq!(req.memory_type, FieldUpdate::NoChange);
    }

    #[test]
    fn update_input_some_none_type_is_clear() {
        use crate::db::types::FieldUpdate;
        use crate::service::UpdateRequest;

        let input = UpdateInput {
            id: "mem-1".to_string(),
            content: None,
            memory_type: Some(None), // explicit null → Clear
            projects: None,
            tags: None,
        };

        let req = UpdateRequest::from(input);
        assert_eq!(req.memory_type, FieldUpdate::Clear);
    }

    // ── Behavior 5: MergeInput → MergeRequest + MergedMemory → MergeResponse ──

    #[test]
    fn merge_input_converts_to_merge_request() {
        use crate::service::MergeRequest;

        let input = MergeInput {
            source_ids: vec!["id-1".to_string(), "id-2".to_string()],
            content: "merged content".to_string(),
            projects: vec!["proj-a".to_string()],
            memory_type: Some("decision".to_string()),
            tags: vec!["rust".to_string()],
        };

        let req = MergeRequest::from(input);
        assert_eq!(req.source_ids, vec!["id-1", "id-2"]);
        assert_eq!(req.content, "merged content");
        assert_eq!(req.projects, vec!["proj-a"]);
        assert_eq!(req.memory_type, Some("decision".to_string()));
        assert_eq!(req.tags, vec!["rust"]);
    }

    #[test]
    fn merged_memory_converts_to_merge_response() {
        let mm = MergedMemory {
            id: "merged-id".to_string(),
            archived: vec!["src-1".to_string(), "src-2".to_string()],
            similar: vec![(sample_memory("sim-1", "similar"), 0.88)],
        };

        let resp = MergeResponse::from(mm);
        assert_eq!(resp.id, "merged-id");
        assert_eq!(resp.archived, vec!["src-1", "src-2"]);
        assert_eq!(resp.similar.len(), 1);
        assert_eq!(resp.similar[0].id, "sim-1");
        assert_eq!(resp.similar[0].similarity, 0.88);
    }

    // ── Behavior 7: ContextInput → ContextRequest ──

    #[test]
    fn context_input_converts_to_context_request() {
        use crate::service::ContextRequest;

        let input = ContextInput {
            queries: vec!["q1".to_string(), "q2".to_string()],
            projects: Some(vec!["proj-a".to_string()]),
            memory_type: Some("pattern".to_string()),
            tags: Some(vec!["rust".to_string()]),
            include_global: Some(false),
            include_taxonomy: Some(true),
            content_budget: Some(5000),
            limit: Some(20),
        };

        let req = ContextRequest::from(input);
        assert_eq!(req.queries, vec!["q1", "q2"]);
        assert_eq!(req.projects, Some(vec!["proj-a".to_string()]));
        assert_eq!(req.memory_type, Some("pattern".to_string()));
        assert_eq!(req.tags, Some(vec!["rust".to_string()]));
        assert!(!req.include_global);
        assert!(req.include_taxonomy);
        assert_eq!(req.content_budget, 5000);
        assert_eq!(req.limit, 20);
    }

    #[test]
    fn context_input_defaults() {
        use crate::service::ContextRequest;

        let input = ContextInput {
            queries: vec!["q1".to_string()],
            projects: None,
            memory_type: None,
            tags: None,
            include_global: None,   // defaults to true
            include_taxonomy: None, // defaults to false
            content_budget: None,   // defaults to 2000
            limit: None,            // defaults to 10
        };

        let req = ContextRequest::from(input);
        assert!(req.include_global);
        assert!(!req.include_taxonomy);
        assert_eq!(req.content_budget, 2000);
        assert_eq!(req.limit, 10);
    }

    #[test]
    fn context_result_converts_to_context_response() {
        let cr = ContextResult {
            hits: vec![ContextHitInner {
                memory: sample_memory("hit-1", "context content"),
                score: 0.95,
                query_index: 0,
            }],
            taxonomy: None,
            truncated: true,
        };

        let resp = ContextResponse::from(cr);
        assert_eq!(resp.memories.len(), 1);
        assert_eq!(resp.memories[0].id, "hit-1");
        assert_eq!(resp.memories[0].content, "context content");
        assert_eq!(resp.memories[0].score, 0.95);
        assert_eq!(resp.memories[0].query_index, 0);
        assert!(resp.truncated);
        assert!(resp.taxonomy.is_none());
    }
}
