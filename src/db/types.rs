//! Data types for database operations.

use serde::{Deserialize, Serialize};

use crate::embedding::Reranker;

/// Tri-state update type for nullable fields.
///
/// `Option<T>` can't distinguish "don't change" from "set to NULL" — both map
/// to `None`. This enum makes the caller's intent explicit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FieldUpdate<T> {
    /// Leave the field unchanged.
    #[default]
    NoChange,
    /// Clear the field (set to NULL).
    Clear,
    /// Set the field to a new value.
    Set(T),
}

impl<T> FieldUpdate<T> {
    /// Returns `true` if this is a change (Clear or Set), i.e. not NoChange.
    pub fn is_change(&self) -> bool {
        !matches!(self, FieldUpdate::NoChange)
    }
}

impl<T> From<Option<Option<T>>> for FieldUpdate<T> {
    fn from(opt: Option<Option<T>>) -> Self {
        match opt {
            None => FieldUpdate::NoChange,
            Some(None) => FieldUpdate::Clear,
            Some(Some(v)) => FieldUpdate::Set(v),
        }
    }
}

impl FieldUpdate<String> {
    /// Convert `FieldUpdate<String>` to `FieldUpdate<&str>`.
    pub fn as_deref(&self) -> FieldUpdate<&str> {
        match self {
            FieldUpdate::NoChange => FieldUpdate::NoChange,
            FieldUpdate::Clear => FieldUpdate::Clear,
            FieldUpdate::Set(v) => FieldUpdate::Set(v.as_str()),
        }
    }
}

/// A memory with all its associated data.
#[derive(Debug, Clone, Serialize)]
pub struct Memory {
    pub id: String,
    pub content: String,
    pub memory_type: Option<String>,
    pub projects: Vec<String>,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub archived_at: Option<String>,
    #[allow(dead_code)] // populated from DB, read by consumers
    pub last_accessed_at: Option<String>,
    pub access_count: i64,
    /// True when content was truncated by `content_max_length`.
    pub truncated: bool,
}

impl Memory {
    /// Truncate content to at most `max_length` Unicode characters.
    /// Sets `self.truncated = true` if truncation occurs.
    pub fn truncate(&mut self, max_length: u32) {
        let max = max_length as usize;
        if let Some((byte_offset, _)) = self.content.char_indices().nth(max) {
            self.content.truncate(byte_offset);
            self.truncated = true;
        }
    }
}

/// A memory with link information in both directions (returned by `get`).
#[derive(Debug, Clone, Serialize)]
pub struct MemoryWithLinks {
    pub memory: Memory,
    pub outgoing_links: Vec<Link>,
    pub incoming_links: Vec<Link>,
}

/// A directional link between two memories.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub id: String,
    pub source_id: String,
    pub target_id: String,
    pub relation: String,
    pub created_at: String,
    /// Content snippet (first 100 chars) of the linked memory.
    /// Only populated by `get()` for display purposes; `None` in all other contexts
    /// (search results, sync exports, link creation responses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// A name with its usage count (for `discover`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NameCount {
    pub name: String,
    pub count: i64,
}

/// Database statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbStats {
    pub total_memories: i64,
    pub total_archived: i64,
    pub storage_size_bytes: i64,
    pub embedding_model: String,
}

/// Full status information for the CLI `status` command.
///
/// Composes `DbStats` with additional fields not needed by `discover`.
#[derive(Debug, Clone)]
pub struct StatusInfo {
    pub stats: DbStats,
    pub total_links: i64,
    pub embedding_dimensions: u32,
    pub schema_version: u32,
}

/// Result of the `discover` operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverResult {
    pub projects: Vec<NameCount>,
    pub types: Vec<NameCount>,
    pub tags: Vec<NameCount>,
    pub relations: Vec<NameCount>,
    pub stats: DbStats,
}

/// Result of the `list` operation.
#[derive(Debug, Clone, Serialize)]
pub struct ListResult {
    pub memories: Vec<Memory>,
    pub total: i64,
}

/// Parameters for storing a new memory.
pub struct StoreParams<'a> {
    pub content: &'a str,
    pub memory_type: Option<&'a str>,
    pub projects: &'a [&'a str],
    pub tags: &'a [&'a str],
    pub links: &'a [(&'a str, &'a str)], // (target_id, relation)
    pub embedding: &'a [f32],
}

/// Parameters for updating an existing memory.
///
/// - `Option<T>`: `None` = no change. Used for non-nullable fields and collection fields
///   (where an empty collection can represent "clear all").
/// - `FieldUpdate<T>`: Used for nullable scalar fields where "no change" and "clear" must
///   be distinguished.
pub struct UpdateParams<'a> {
    pub content: Option<&'a str>,
    pub memory_type: FieldUpdate<&'a str>,
    pub projects: Option<&'a [&'a str]>,
    pub tags: Option<&'a [&'a str]>,
    /// New embedding — required when content changes.
    pub embedding: Option<&'a [f32]>,
}

/// Result of an update operation.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateResult {
    pub id: String,
    pub updated_at: String,
}

/// Result of an archive operation.
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveResult {
    pub id: String,
    pub archived_at: String,
}

/// Returned by `Database::unarchive` with the memory ID and the new `updated_at` timestamp.
#[derive(Debug, Clone, Serialize)]
pub struct UnarchiveResult {
    pub id: String,
    pub updated_at: String,
}

/// Temporal filters for constraining queries by creation/update time.
/// All fields are optional; when provided, they are AND-combined.
#[derive(Default)]
pub struct TimeFilters<'a> {
    pub created_after: Option<&'a str>, // ISO 8601 lower bound on created_at
    pub created_before: Option<&'a str>, // ISO 8601 upper bound on created_at
    pub updated_after: Option<&'a str>, // ISO 8601 lower bound on updated_at
    pub updated_before: Option<&'a str>, // ISO 8601 upper bound on updated_at
}

/// Common filter fields shared by `ListParams` and `SearchParams`.
pub struct FilterParams<'a> {
    pub projects: Option<&'a [&'a str]>,
    pub memory_type: Option<&'a str>,
    pub tags: Option<&'a [&'a str]>,
    pub include_global: bool,
    pub include_archived: bool,
    pub time: TimeFilters<'a>,
}

impl Default for FilterParams<'_> {
    fn default() -> Self {
        Self {
            projects: None,
            memory_type: None,
            tags: None,
            include_global: true,
            include_archived: false,
            time: TimeFilters::default(),
        }
    }
}

/// Parameters for listing memories.
pub struct ListParams<'a> {
    pub filter: FilterParams<'a>,
    pub limit: u32,
    pub offset: u32,
    pub content_max_length: Option<u32>,
}

impl Default for ListParams<'_> {
    fn default() -> Self {
        Self {
            filter: FilterParams::default(),
            limit: 20,
            offset: 0,
            content_max_length: None,
        }
    }
}

/// Parameters for the search operation.
pub struct SearchParams<'a> {
    /// The search query text (used for FTS5 search).
    pub query: &'a str,
    /// Pre-computed embedding of the query (used for vector search).
    pub query_embedding: &'a [f32],
    pub filter: FilterParams<'a>,
    pub limit: u32,
    pub offset: u32,
    pub content_max_length: Option<u32>,
    /// RRF constant for result merging (default: 60).
    pub rrf_k: u32,
    /// Optional reranker to rescore candidates after RRF merge.
    /// When `None`, RRF scores are used directly.
    pub reranker: Option<&'a dyn Reranker>,
    /// Minimum reranker score to include in results. Only used when `reranker` is `Some`.
    pub reranker_threshold: f64,
}

/// NOTE: Default produces a no-op search (empty query + empty embedding
/// skips both FTS5 and vector backends). Callers must set `query` and
/// `query_embedding` for meaningful results. Intended for struct-update
/// syntax in tests: `SearchParams { query: "...", query_embedding: &emb, ..Default::default() }`.
impl Default for SearchParams<'_> {
    fn default() -> Self {
        Self {
            query: "",
            query_embedding: &[],
            filter: FilterParams::default(),
            limit: 10,
            offset: 0,
            content_max_length: None,
            rrf_k: 60,
            reranker: None,
            reranker_threshold: 0.0,
        }
    }
}

/// A single search result with relevance score and links.
#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub memory: Memory,
    pub outgoing_links: Vec<Link>,
    pub incoming_links: Vec<Link>,
    /// RRF relevance score (higher is more relevant).
    pub score: f64,
}

/// Result of the `search` operation (hits + total match count for pagination).
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub results: Vec<SearchHit>,
    pub total: i64,
}

/// Parameters for the merge operation.
pub struct MergeParams<'a> {
    pub source_ids: &'a [&'a str],
    pub content: &'a str,
    pub memory_type: Option<&'a str>,
    pub projects: &'a [&'a str],
    pub tags: &'a [&'a str],
    pub embedding: &'a [f32],
}

/// Result of a merge operation.
#[derive(Debug, Clone)]
pub struct MergeResult {
    pub id: String,
    pub archived: Vec<String>,
}

/// The type of entity referenced by a tombstone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Memory,
    Link,
}

impl EntityType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntityType::Memory => "memory",
            EntityType::Link => "link",
        }
    }
}

impl std::fmt::Display for EntityType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for EntityType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "memory" => Ok(EntityType::Memory),
            "link" => Ok(EntityType::Link),
            other => Err(format!("unknown entity_type: '{other}'")),
        }
    }
}

impl rusqlite::types::FromSql for EntityType {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse()
            .map_err(|e: String| rusqlite::types::FromSqlError::Other(e.into()))
    }
}

/// The action recorded in a tombstone (what happened to the entity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TombstoneAction {
    Archived,
    Deleted,
    Unarchived,
}

impl TombstoneAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            TombstoneAction::Archived => "archived",
            TombstoneAction::Deleted => "deleted",
            TombstoneAction::Unarchived => "unarchived",
        }
    }
}

impl std::fmt::Display for TombstoneAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TombstoneAction {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "archived" => Ok(TombstoneAction::Archived),
            "deleted" => Ok(TombstoneAction::Deleted),
            "unarchived" => Ok(TombstoneAction::Unarchived),
            other => Err(format!("unknown tombstone action: '{other}'")),
        }
    }
}

impl rusqlite::types::FromSql for TombstoneAction {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse()
            .map_err(|e: String| rusqlite::types::FromSqlError::Other(e.into()))
    }
}

/// A tombstone record for sync: tracks archived/deleted entities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub entity_type: EntityType,
    pub entity_id: String,
    pub action: TombstoneAction,
    pub timestamp: String,
}

/// Parameters for importing a memory from a remote machine during sync.
pub struct ImportMemoryParams<'a> {
    pub id: &'a str,
    pub content: &'a str,
    pub memory_type: Option<&'a str>,
    pub projects: &'a [&'a str],
    pub tags: &'a [&'a str],
    pub created_at: &'a str,
    pub updated_at: &'a str,
    pub archived_at: Option<&'a str>,
    pub embedding: &'a [f32],
}

/// Result of an import upsert operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportAction {
    Inserted,
    Updated,
    Skipped,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_serializes_to_expected_json() {
        let link = Link {
            id: "link-001".into(),
            source_id: "mem-aaa".into(),
            target_id: "mem-bbb".into(),
            relation: "related_to".into(),
            created_at: "2025-01-15T10:00:00Z".into(),
            content: None,
        };
        let json: serde_json::Value = serde_json::to_value(&link).unwrap();
        assert_eq!(json["id"], "link-001");
        assert_eq!(json["source_id"], "mem-aaa");
        assert_eq!(json["target_id"], "mem-bbb");
        assert_eq!(json["relation"], "related_to");
        assert_eq!(json["created_at"], "2025-01-15T10:00:00Z");
        // content: None is skipped by skip_serializing_if.
        assert!(!json.as_object().unwrap().contains_key("content"));
        // Exactly 5 fields when content is None.
        assert_eq!(json.as_object().unwrap().len(), 5);
    }

    #[test]
    fn link_serializes_with_content_snippet() {
        let link = Link {
            id: "link-002".into(),
            source_id: "mem-aaa".into(),
            target_id: "mem-bbb".into(),
            relation: "supersedes".into(),
            created_at: "2025-01-15T10:00:00Z".into(),
            content: Some("Rust error handling patterns".into()),
        };
        let json: serde_json::Value = serde_json::to_value(&link).unwrap();
        assert_eq!(json["content"], "Rust error handling patterns");
        assert_eq!(json.as_object().unwrap().len(), 6);
    }

    #[test]
    fn link_deserialization_handles_missing_content_field() {
        // JSON without the "content" field — simulates old sync export format.
        let json = r#"{
            "id": "link-001",
            "source_id": "mem-aaa",
            "target_id": "mem-bbb",
            "relation": "related_to",
            "created_at": "2025-01-15T10:00:00Z"
        }"#;
        let link: Link = serde_json::from_str(json).unwrap();
        assert_eq!(link.id, "link-001");
        assert_eq!(link.source_id, "mem-aaa");
        assert_eq!(link.target_id, "mem-bbb");
        assert_eq!(link.relation, "related_to");
        assert_eq!(link.created_at, "2025-01-15T10:00:00Z");
        assert!(
            link.content.is_none(),
            "missing content field should default to None for backward compatibility"
        );
    }

    #[test]
    fn name_count_serializes_correctly() {
        let nc = NameCount {
            name: "rust".into(),
            count: 42,
        };
        let json: serde_json::Value = serde_json::to_value(&nc).unwrap();
        assert_eq!(json["name"], "rust");
        assert_eq!(json["count"], 42);
        assert_eq!(json.as_object().unwrap().len(), 2);
    }

    #[test]
    fn db_stats_serializes_correctly() {
        let stats = DbStats {
            total_memories: 100,
            total_archived: 5,
            storage_size_bytes: 1024,
            embedding_model: "nomic-embed-text-v1.5".into(),
        };
        let json: serde_json::Value = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["total_memories"], 100);
        assert_eq!(json["total_archived"], 5);
        assert_eq!(json["storage_size_bytes"], 1024);
        assert_eq!(json["embedding_model"], "nomic-embed-text-v1.5");
        assert_eq!(json.as_object().unwrap().len(), 4);
    }

    #[test]
    fn discover_result_serializes_correctly() {
        let result = DiscoverResult {
            projects: vec![NameCount {
                name: "erinra".into(),
                count: 3,
            }],
            types: vec![],
            tags: vec![NameCount {
                name: "rust".into(),
                count: 7,
            }],
            relations: vec![],
            stats: DbStats {
                total_memories: 10,
                total_archived: 2,
                storage_size_bytes: 2048,
                embedding_model: "test-model".into(),
            },
        };
        let json: serde_json::Value = serde_json::to_value(&result).unwrap();
        assert_eq!(json["projects"][0]["name"], "erinra");
        assert_eq!(json["projects"][0]["count"], 3);
        assert!(json["types"].as_array().unwrap().is_empty());
        assert_eq!(json["tags"][0]["name"], "rust");
        assert!(json["relations"].as_array().unwrap().is_empty());
        assert_eq!(json["stats"]["total_memories"], 10);
        assert_eq!(json["stats"]["embedding_model"], "test-model");
    }

    #[test]
    fn update_result_serializes_correctly() {
        let result = UpdateResult {
            id: "mem-123".into(),
            updated_at: "2025-06-01T12:00:00Z".into(),
        };
        let json: serde_json::Value = serde_json::to_value(&result).unwrap();
        assert_eq!(json["id"], "mem-123");
        assert_eq!(json["updated_at"], "2025-06-01T12:00:00Z");
        assert_eq!(json.as_object().unwrap().len(), 2);
    }

    #[test]
    fn archive_result_serializes_correctly() {
        let result = ArchiveResult {
            id: "mem-456".into(),
            archived_at: "2025-06-01T15:00:00Z".into(),
        };
        let json: serde_json::Value = serde_json::to_value(&result).unwrap();
        assert_eq!(json["id"], "mem-456");
        assert_eq!(json["archived_at"], "2025-06-01T15:00:00Z");
        assert_eq!(json.as_object().unwrap().len(), 2);
    }

    #[test]
    fn truncate_ascii_content_sets_truncated_flag() {
        let mut mem = Memory {
            id: "test-id".into(),
            content: "hello world".into(), // 11 chars
            memory_type: None,
            projects: vec![],
            tags: vec![],
            created_at: String::new(),
            updated_at: String::new(),
            archived_at: None,
            last_accessed_at: None,
            access_count: 0,
            truncated: false,
        };

        mem.truncate(5);

        assert_eq!(mem.content, "hello");
        assert!(
            mem.truncated,
            "truncated flag should be set when content is shortened"
        );
    }

    #[test]
    fn truncate_noop_when_content_within_limit() {
        let mut mem = Memory {
            id: "test-id".into(),
            content: "hello".into(), // 5 chars
            memory_type: None,
            projects: vec![],
            tags: vec![],
            created_at: String::new(),
            updated_at: String::new(),
            archived_at: None,
            last_accessed_at: None,
            access_count: 0,
            truncated: false,
        };

        // Exactly at limit.
        mem.truncate(5);
        assert_eq!(mem.content, "hello");
        assert!(
            !mem.truncated,
            "should not be truncated when content equals limit"
        );

        // Under limit.
        mem.truncate(100);
        assert_eq!(mem.content, "hello");
        assert!(
            !mem.truncated,
            "should not be truncated when content is under limit"
        );
    }

    #[test]
    fn truncate_handles_multibyte_utf8() {
        // CJK characters are 3 bytes each in UTF-8.
        let mut mem = Memory {
            id: "test-id".into(),
            content: "rust 错误处理模式".into(), // "rust " (5) + 5 CJK = 10 chars
            memory_type: None,
            projects: vec![],
            tags: vec![],
            created_at: String::new(),
            updated_at: String::new(),
            archived_at: None,
            last_accessed_at: None,
            access_count: 0,
            truncated: false,
        };

        mem.truncate(7); // Should keep "rust " + 2 CJK chars = "rust 错误"

        assert_eq!(mem.content, "rust 错误");
        assert_eq!(mem.content.chars().count(), 7);
        assert!(mem.truncated);

        // Also test with emoji (4 bytes each).
        let mut mem2 = Memory {
            id: "test-id".into(),
            content: "hi 😀😁😂".into(), // "hi " (3) + 3 emoji = 6 chars
            memory_type: None,
            projects: vec![],
            tags: vec![],
            created_at: String::new(),
            updated_at: String::new(),
            archived_at: None,
            last_accessed_at: None,
            access_count: 0,
            truncated: false,
        };

        mem2.truncate(4); // "hi " + 1 emoji = "hi 😀"

        assert_eq!(mem2.content, "hi 😀");
        assert_eq!(mem2.content.chars().count(), 4);
        assert!(mem2.truncated);
    }

    #[test]
    fn truncate_to_zero_empties_content() {
        let mut mem = Memory {
            id: "test-id".into(),
            content: "hello".into(),
            memory_type: None,
            projects: vec![],
            tags: vec![],
            created_at: String::new(),
            updated_at: String::new(),
            archived_at: None,
            last_accessed_at: None,
            access_count: 0,
            truncated: false,
        };

        mem.truncate(0);

        assert_eq!(mem.content, "");
        assert!(mem.truncated);
    }

    #[test]
    fn truncate_twice_narrows_correctly() {
        let mut mem = Memory {
            id: "test-id".into(),
            content: "hello world, this is a test".into(), // 27 chars
            memory_type: None,
            projects: vec![],
            tags: vec![],
            created_at: String::new(),
            updated_at: String::new(),
            archived_at: None,
            last_accessed_at: None,
            access_count: 0,
            truncated: false,
        };

        mem.truncate(15);
        assert_eq!(mem.content.chars().count(), 15);
        assert!(mem.truncated);

        // Second truncation narrows further.
        mem.truncate(5);
        assert_eq!(mem.content, "hello");
        assert!(mem.truncated);
    }

    #[test]
    fn tombstone_serializes_to_expected_json() {
        let tombstone = Tombstone {
            entity_type: EntityType::Memory,
            entity_id: "mem-123".into(),
            action: TombstoneAction::Archived,
            timestamp: "2025-06-01T12:00:00Z".into(),
        };
        let json: serde_json::Value = serde_json::to_value(&tombstone).unwrap();
        assert_eq!(json["entity_type"], "memory");
        assert_eq!(json["entity_id"], "mem-123");
        assert_eq!(json["action"], "archived");
        assert_eq!(json["timestamp"], "2025-06-01T12:00:00Z");
        // Exactly 4 fields, no extras.
        assert_eq!(json.as_object().unwrap().len(), 4);
    }

    #[test]
    fn tombstone_round_trips_through_json() {
        let original = Tombstone {
            entity_type: EntityType::Link,
            entity_id: "link-456".into(),
            action: TombstoneAction::Deleted,
            timestamp: "2025-07-01T08:30:00Z".into(),
        };
        let json_str = serde_json::to_string(&original).unwrap();
        let deserialized: Tombstone = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deserialized.entity_type, EntityType::Link);
        assert_eq!(deserialized.entity_id, "link-456");
        assert_eq!(deserialized.action, TombstoneAction::Deleted);
        assert_eq!(deserialized.timestamp, "2025-07-01T08:30:00Z");
    }

    #[test]
    fn entity_type_serializes_and_round_trips() {
        // Memory variant serializes to "memory".
        let json = serde_json::to_value(EntityType::Memory).unwrap();
        assert_eq!(json, "memory");

        // Link variant serializes to "link".
        let json = serde_json::to_value(EntityType::Link).unwrap();
        assert_eq!(json, "link");

        // Round-trip through JSON string.
        let serialized = serde_json::to_string(&EntityType::Memory).unwrap();
        let deserialized: EntityType = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, EntityType::Memory);

        let serialized = serde_json::to_string(&EntityType::Link).unwrap();
        let deserialized: EntityType = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, EntityType::Link);

        // Display / as_str produce lowercase strings.
        assert_eq!(EntityType::Memory.as_str(), "memory");
        assert_eq!(EntityType::Link.as_str(), "link");
        assert_eq!(EntityType::Memory.to_string(), "memory");
        assert_eq!(EntityType::Link.to_string(), "link");
    }

    #[test]
    fn tombstone_action_serializes_and_round_trips() {
        // Archived variant serializes to "archived".
        let json = serde_json::to_value(TombstoneAction::Archived).unwrap();
        assert_eq!(json, "archived");

        // Deleted variant serializes to "deleted".
        let json = serde_json::to_value(TombstoneAction::Deleted).unwrap();
        assert_eq!(json, "deleted");

        // Round-trip through JSON string.
        let serialized = serde_json::to_string(&TombstoneAction::Archived).unwrap();
        let deserialized: TombstoneAction = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, TombstoneAction::Archived);

        let serialized = serde_json::to_string(&TombstoneAction::Deleted).unwrap();
        let deserialized: TombstoneAction = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, TombstoneAction::Deleted);

        // Display / as_str produce lowercase strings.
        assert_eq!(TombstoneAction::Archived.as_str(), "archived");
        assert_eq!(TombstoneAction::Deleted.as_str(), "deleted");
        assert_eq!(TombstoneAction::Archived.to_string(), "archived");
        assert_eq!(TombstoneAction::Deleted.to_string(), "deleted");
    }

    #[test]
    fn memory_serializes_to_expected_json() {
        let mem = Memory {
            id: "mem-001".into(),
            content: "Rust error handling patterns".into(),
            memory_type: Some("knowledge".into()),
            projects: vec!["erinra".into(), "tooling".into()],
            tags: vec!["rust".into(), "error-handling".into()],
            created_at: "2025-01-15T10:00:00Z".into(),
            updated_at: "2025-01-16T08:30:00Z".into(),
            archived_at: Some("2025-02-01T12:00:00Z".into()),
            last_accessed_at: Some("2025-01-20T14:00:00Z".into()),
            access_count: 5,
            truncated: false,
        };
        let json: serde_json::Value = serde_json::to_value(&mem).unwrap();
        assert_eq!(json["id"], "mem-001");
        assert_eq!(json["content"], "Rust error handling patterns");
        assert_eq!(json["memory_type"], "knowledge");
        assert_eq!(json["projects"], serde_json::json!(["erinra", "tooling"]));
        assert_eq!(json["tags"], serde_json::json!(["rust", "error-handling"]));
        assert_eq!(json["created_at"], "2025-01-15T10:00:00Z");
        assert_eq!(json["updated_at"], "2025-01-16T08:30:00Z");
        assert_eq!(json["archived_at"], "2025-02-01T12:00:00Z");
        assert_eq!(json["last_accessed_at"], "2025-01-20T14:00:00Z");
        assert_eq!(json["access_count"], 5);
        assert_eq!(json["truncated"], false);
        // Exactly 11 fields, no extras.
        assert_eq!(json.as_object().unwrap().len(), 11);
    }

    #[test]
    fn memory_minimal_serialization() {
        let mem = Memory {
            id: "mem-002".into(),
            content: "Minimal memory".into(),
            memory_type: None,
            projects: vec![],
            tags: vec![],
            created_at: "2025-01-15T10:00:00Z".into(),
            updated_at: "2025-01-15T10:00:00Z".into(),
            archived_at: None,
            last_accessed_at: None,
            access_count: 0,
            truncated: false,
        };
        let json: serde_json::Value = serde_json::to_value(&mem).unwrap();
        assert_eq!(json["id"], "mem-002");
        assert_eq!(json["content"], "Minimal memory");
        assert!(json["memory_type"].is_null());
        assert!(json["projects"].as_array().unwrap().is_empty());
        assert!(json["tags"].as_array().unwrap().is_empty());
        assert_eq!(json["created_at"], "2025-01-15T10:00:00Z");
        assert_eq!(json["updated_at"], "2025-01-15T10:00:00Z");
        assert!(json["archived_at"].is_null());
        assert!(json["last_accessed_at"].is_null());
        assert_eq!(json["access_count"], 0);
        assert_eq!(json["truncated"], false);
        // Exactly 11 fields, no extras.
        assert_eq!(json.as_object().unwrap().len(), 11);
    }

    #[test]
    fn memory_with_links_serializes_to_expected_json() {
        let mem = Memory {
            id: "mem-003".into(),
            content: "Memory with links".into(),
            memory_type: Some("decision".into()),
            projects: vec!["erinra".into()],
            tags: vec![],
            created_at: "2025-01-15T10:00:00Z".into(),
            updated_at: "2025-01-15T10:00:00Z".into(),
            archived_at: None,
            last_accessed_at: None,
            access_count: 1,
            truncated: false,
        };
        let mwl = MemoryWithLinks {
            memory: mem,
            outgoing_links: vec![Link {
                id: "link-001".into(),
                source_id: "mem-003".into(),
                target_id: "mem-010".into(),
                relation: "supersedes".into(),
                created_at: "2025-01-15T11:00:00Z".into(),
                content: None,
            }],
            incoming_links: vec![Link {
                id: "link-002".into(),
                source_id: "mem-020".into(),
                target_id: "mem-003".into(),
                relation: "related_to".into(),
                created_at: "2025-01-15T12:00:00Z".into(),
                content: None,
            }],
        };
        let json: serde_json::Value = serde_json::to_value(&mwl).unwrap();
        // Top-level structure has 3 fields.
        assert_eq!(json.as_object().unwrap().len(), 3);
        // Memory is nested.
        assert_eq!(json["memory"]["id"], "mem-003");
        assert_eq!(json["memory"]["content"], "Memory with links");
        assert_eq!(json["memory"]["memory_type"], "decision");
        // Outgoing links.
        assert_eq!(json["outgoing_links"].as_array().unwrap().len(), 1);
        assert_eq!(json["outgoing_links"][0]["id"], "link-001");
        assert_eq!(json["outgoing_links"][0]["source_id"], "mem-003");
        assert_eq!(json["outgoing_links"][0]["target_id"], "mem-010");
        assert_eq!(json["outgoing_links"][0]["relation"], "supersedes");
        assert_eq!(
            json["outgoing_links"][0]["created_at"],
            "2025-01-15T11:00:00Z"
        );
        // Incoming links.
        assert_eq!(json["incoming_links"].as_array().unwrap().len(), 1);
        assert_eq!(json["incoming_links"][0]["id"], "link-002");
        assert_eq!(json["incoming_links"][0]["source_id"], "mem-020");
        assert_eq!(json["incoming_links"][0]["target_id"], "mem-003");
        assert_eq!(json["incoming_links"][0]["relation"], "related_to");
        assert_eq!(
            json["incoming_links"][0]["created_at"],
            "2025-01-15T12:00:00Z"
        );
    }

    #[test]
    fn list_result_serializes_to_expected_json() {
        let result = ListResult {
            memories: vec![
                Memory {
                    id: "mem-100".into(),
                    content: "First memory".into(),
                    memory_type: None,
                    projects: vec![],
                    tags: vec![],
                    created_at: "2025-01-15T10:00:00Z".into(),
                    updated_at: "2025-01-15T10:00:00Z".into(),
                    archived_at: None,
                    last_accessed_at: None,
                    access_count: 0,
                    truncated: false,
                },
                Memory {
                    id: "mem-101".into(),
                    content: "Second memory".into(),
                    memory_type: Some("note".into()),
                    projects: vec!["proj".into()],
                    tags: vec!["tag1".into()],
                    created_at: "2025-01-16T10:00:00Z".into(),
                    updated_at: "2025-01-16T10:00:00Z".into(),
                    archived_at: None,
                    last_accessed_at: None,
                    access_count: 3,
                    truncated: false,
                },
            ],
            total: 42,
        };
        let json: serde_json::Value = serde_json::to_value(&result).unwrap();
        // Exactly 2 fields: memories and total.
        assert_eq!(json.as_object().unwrap().len(), 2);
        assert_eq!(json["total"], 42);
        assert_eq!(json["memories"].as_array().unwrap().len(), 2);
        assert_eq!(json["memories"][0]["id"], "mem-100");
        assert_eq!(json["memories"][0]["content"], "First memory");
        assert_eq!(json["memories"][1]["id"], "mem-101");
        assert_eq!(json["memories"][1]["content"], "Second memory");
        assert_eq!(json["memories"][1]["memory_type"], "note");
        assert_eq!(json["memories"][1]["access_count"], 3);
    }

    #[test]
    fn truncate_empty_content_is_noop() {
        let mut mem = Memory {
            id: "test-id".into(),
            content: String::new(),
            memory_type: None,
            projects: vec![],
            tags: vec![],
            created_at: String::new(),
            updated_at: String::new(),
            archived_at: None,
            last_accessed_at: None,
            access_count: 0,
            truncated: false,
        };

        mem.truncate(10);

        assert_eq!(mem.content, "");
        assert!(!mem.truncated);
    }
}
