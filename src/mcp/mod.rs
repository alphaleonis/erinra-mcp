//! MCP server and tool handlers (stdio JSON-RPC via rmcp).

mod handlers;
mod types;

use std::sync::{Arc, Mutex};

use rmcp::handler::server::tool::{ToolCallContext, ToolRouter};
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde::Serialize;

use crate::db::Database;
use crate::embedding::{Embedder, Reranker};

pub use types::ServerConfig;

// ── Server struct ───────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ErinraServer {
    db: Arc<Mutex<Database>>,
    embedder: Arc<dyn Embedder>,
    reranker: Option<Arc<dyn Reranker>>,
    config: Arc<ServerConfig>,
    /// Cached MCP instructions (taxonomy snapshot). Updated after mutations
    /// so `get_info()` can read without acquiring the DB mutex.
    instructions: Arc<std::sync::RwLock<String>>,
    tool_router: ToolRouter<Self>,
}

impl ErinraServer {
    pub fn new(
        db: Arc<Mutex<Database>>,
        embedder: Arc<dyn Embedder>,
        reranker: Option<Arc<dyn Reranker>>,
        config: ServerConfig,
    ) -> Self {
        let instructions = {
            let db = db.lock().expect("db mutex poisoned");
            Self::build_instructions(&db).unwrap_or_default()
        };
        Self {
            db,
            embedder,
            reranker,
            config: Arc::new(config),
            instructions: Arc::new(std::sync::RwLock::new(instructions)),
            tool_router: Self::create_tool_router(),
        }
    }

    /// Refresh the cached instructions after a mutation.
    /// Spawns a background task so the handler can return immediately.
    pub(crate) fn refresh_instructions(&self) {
        let db = Arc::clone(&self.db);
        let instructions = Arc::clone(&self.instructions);
        tokio::task::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || {
                let db = db.lock().expect("db mutex poisoned");
                if let Ok(s) = ErinraServer::build_instructions(&db) {
                    *instructions.write().expect("instructions lock poisoned") = s;
                }
            })
            .await;
        });
    }

    fn build_instructions(db: &Database) -> anyhow::Result<String> {
        let discover = db.discover()?;
        let mut parts = vec![
            "Erinra is a memory store for LLM coding assistants.".to_string(),
            String::new(),
            "Quick start:".to_string(),
            "- `store` to save knowledge (returns similar existing memories for dedup)".to_string(),
            "- `search` to find relevant memories by text".to_string(),
            "- `get` to fetch full details by ID".to_string(),
            "- `list` to browse/filter without a search query".to_string(),
            "- `discover` to refresh the full taxonomy".to_string(),
        ];

        let has_data = !discover.projects.is_empty()
            || !discover.types.is_empty()
            || !discover.tags.is_empty();

        if has_data {
            parts.push(String::new());
            parts.push("Current taxonomy:".to_string());
            if !discover.projects.is_empty() {
                let items: Vec<String> = discover
                    .projects
                    .iter()
                    .map(|nc| format!("{} ({})", nc.name, nc.count))
                    .collect();
                parts.push(format!("Projects: {}", items.join(", ")));
            }
            if !discover.types.is_empty() {
                let items: Vec<String> = discover
                    .types
                    .iter()
                    .map(|nc| format!("{} ({})", nc.name, nc.count))
                    .collect();
                parts.push(format!("Types: {}", items.join(", ")));
            }
            if !discover.tags.is_empty() {
                let items: Vec<String> = discover
                    .tags
                    .iter()
                    .map(|nc| format!("{} ({})", nc.name, nc.count))
                    .collect();
                parts.push(format!("Tags: {}", items.join(", ")));
            }
        }

        Ok(parts.join("\n"))
    }
}

// ── ServerHandler impl ──────────────────────────────────────────────────

impl ServerHandler for ErinraServer {
    fn get_info(&self) -> ServerInfo {
        // Read cached instructions (no DB mutex needed — updated asynchronously
        // after mutations via refresh_instructions).
        let instructions = self
            .instructions
            .read()
            .expect("instructions lock poisoned")
            .clone();
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("erinra", env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions)
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult {
            tools: self.tool_router.list_all(),
            ..Default::default()
        }))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, ErrorData>> + Send + '_ {
        let tool_context = ToolCallContext::new(self, request, context);
        async move { self.tool_router.call(tool_context).await }
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
    }
}

// ── Server startup ──────────────────────────────────────────────────────

/// Start the MCP server on stdio transport.
pub async fn serve(
    db: Arc<Mutex<Database>>,
    embedder: Arc<dyn Embedder>,
    reranker: Option<Arc<dyn Reranker>>,
    config: ServerConfig,
) -> anyhow::Result<()> {
    let server = ErinraServer::new(db, embedder, reranker, config);
    let transport = rmcp::transport::io::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Build a successful tool result with JSON content.
pub(crate) fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let json = serde_json::to_string(value)
        .map_err(|e| ErrorData::internal_error(format!("JSON serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Build a tool-level error result (isError: true, visible to LLM).
pub(crate) fn tool_error(msg: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![Content::text(msg.into())]))
}

/// Map an internal error to ErrorData.
pub(crate) fn internal_error(e: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(format!("internal error: {e}"), None)
}

/// Convert Option<Vec<String>> to Option<Vec<&str>>.
pub(crate) fn strs(v: &Option<Vec<String>>) -> Option<Vec<&str>> {
    v.as_ref().map(|v| v.iter().map(|s| s.as_str()).collect())
}

/// Convert &[String] to Vec<&str>.
pub(crate) fn strs_owned(v: &[String]) -> Vec<&str> {
    v.iter().map(|s| s.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::types::*;
    use super::*;
    use crate::db::DbConfig;
    use crate::db::types::*;
    use crate::embedding::MockEmbedder;
    use rmcp::handler::server::wrapper::Parameters;

    fn test_server() -> ErinraServer {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let embedder = Arc::new(MockEmbedder::new(768));
        ErinraServer::new(
            Arc::new(Mutex::new(db)),
            embedder,
            None,
            ServerConfig::default(),
        )
    }

    fn extract_text(result: &CallToolResult) -> &str {
        match &result.content[0].raw {
            RawContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn server_builds_instructions() {
        let server = test_server();
        let info = server.get_info();
        let instructions = info.instructions.unwrap_or_default();
        assert!(instructions.contains("Erinra is a memory store"));
    }

    #[tokio::test]
    async fn tool_discover_empty_db() {
        let server = test_server();
        let result = server.tool_discover().await.unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let parsed: serde_json::Value = serde_json::from_str(extract_text(&result)).unwrap();
        assert_eq!(parsed["stats"]["total_memories"], 0);
    }

    #[tokio::test]
    async fn tool_store_and_get() {
        let server = test_server();

        // Store a memory.
        let store_result = server
            .tool_store(Parameters(StoreInput {
                content: "Rust pattern: use Result for recoverable errors".to_string(),
                projects: vec!["erinra".to_string()],
                memory_type: Some("pattern".to_string()),
                tags: vec!["rust".to_string(), "error-handling".to_string()],
                links: vec![],
            }))
            .await
            .unwrap();
        assert!(!store_result.is_error.unwrap_or(false));
        let store_resp: StoreResponse = serde_json::from_str(extract_text(&store_result)).unwrap();
        assert!(!store_resp.id.is_empty());

        // Get it back.
        let get_result = server
            .tool_get(Parameters(GetInput {
                ids: vec![store_resp.id.clone()],
            }))
            .await
            .unwrap();
        let get_resp: Vec<MemoryFullResponse> =
            serde_json::from_str(extract_text(&get_result)).unwrap();
        assert_eq!(get_resp.len(), 1);
        assert_eq!(get_resp[0].id, store_resp.id);
        assert_eq!(
            get_resp[0].content,
            "Rust pattern: use Result for recoverable errors"
        );
        assert_eq!(get_resp[0].projects, vec!["erinra"]);
    }

    #[tokio::test]
    async fn tool_store_content_too_large() {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let embedder = Arc::new(MockEmbedder::new(768));
        let config = ServerConfig {
            max_content_size: 10,
            ..Default::default()
        };
        let server = ErinraServer::new(Arc::new(Mutex::new(db)), embedder, None, config);

        let result = server
            .tool_store(Parameters(StoreInput {
                content: "this content is way too long".to_string(),
                projects: vec![],
                memory_type: None,
                tags: vec![],
                links: vec![],
            }))
            .await
            .unwrap();
        assert!(result.is_error.unwrap_or(false));
    }

    #[tokio::test]
    async fn tool_archive_not_found() {
        let server = test_server();
        let result = server
            .tool_archive(Parameters(ArchiveInput {
                id: "nonexistent-uuid".to_string(),
            }))
            .await
            .unwrap();
        // User-facing errors are returned as tool errors (isError: true),
        // not protocol errors, so the LLM can reason about them.
        assert!(result.is_error.unwrap_or(false));
        let text = extract_text(&result);
        // Coupled to DbError::NotFound's Display impl.
        assert!(
            text.contains("does not exist"),
            "expected not_found error, got: {text}"
        );
    }

    #[tokio::test]
    async fn tool_search_returns_results() {
        let server = test_server();

        // Store a memory.
        server
            .tool_store(Parameters(StoreInput {
                content: "Always use snake_case for Rust function names".to_string(),
                projects: vec!["coding".to_string()],
                memory_type: Some("preference".to_string()),
                tags: vec!["rust".to_string()],
                links: vec![],
            }))
            .await
            .unwrap();

        // Search for it.
        let result = server
            .tool_search(Parameters(SearchInput {
                query: "rust naming conventions".to_string(),
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
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let hits: Vec<SearchHitResponse> = serde_json::from_str(extract_text(&result)).unwrap();
        assert!(!hits.is_empty());
    }

    #[tokio::test]
    async fn tool_list_with_filters() {
        let server = test_server();

        // Store two memories in different projects.
        server
            .tool_store(Parameters(StoreInput {
                content: "Memory in project A".to_string(),
                projects: vec!["project-a".to_string()],
                memory_type: None,
                tags: vec![],
                links: vec![],
            }))
            .await
            .unwrap();
        server
            .tool_store(Parameters(StoreInput {
                content: "Memory in project B".to_string(),
                projects: vec!["project-b".to_string()],
                memory_type: None,
                tags: vec![],
                links: vec![],
            }))
            .await
            .unwrap();

        // List only project-a.
        let result = server
            .tool_list(Parameters(ListInput {
                projects: Some(vec!["project-a".to_string()]),
                memory_type: None,
                tags: None,
                created_after: None,
                created_before: None,
                updated_after: None,
                updated_before: None,
                created_max_age_days: None,
                created_min_age_days: None,
                updated_max_age_days: None,
                updated_min_age_days: None,
                include_global: Some(false),
                include_archived: None,
                limit: None,
                offset: None,
            }))
            .await
            .unwrap();
        let list_resp: ListResponse = serde_json::from_str(extract_text(&result)).unwrap();
        assert_eq!(list_resp.total, 1);
        assert_eq!(list_resp.memories[0].projects, vec!["project-a"]);
    }

    #[tokio::test]
    async fn tool_update_clears_type() {
        let server = test_server();

        // Store a memory with a type.
        let store_result = server
            .tool_store(Parameters(StoreInput {
                content: "Remember to use FieldUpdate for nullable fields".to_string(),
                projects: vec![],
                memory_type: Some("pattern".to_string()),
                tags: vec![],
                links: vec![],
            }))
            .await
            .unwrap();
        assert!(!store_result.is_error.unwrap_or(false));
        let store_resp: StoreResponse = serde_json::from_str(extract_text(&store_result)).unwrap();

        // Update: clear the type by setting it to null.
        let update_result = server
            .tool_update(Parameters(UpdateInput {
                id: store_resp.id.clone(),
                content: None,
                memory_type: Some(None), // null → Clear
                projects: None,
                tags: None,
            }))
            .await
            .unwrap();
        assert!(!update_result.is_error.unwrap_or(false));

        // Verify the type was cleared.
        let get_result = server
            .tool_get(Parameters(GetInput {
                ids: vec![store_resp.id.clone()],
            }))
            .await
            .unwrap();
        let get_resp: Vec<MemoryFullResponse> =
            serde_json::from_str(extract_text(&get_result)).unwrap();
        assert_eq!(get_resp[0].memory_type, None);
    }

    #[tokio::test]
    async fn embed_content_returns_correct_dimensionality() {
        let server = test_server();
        let vec = server.embed_content("test content").await.unwrap();
        assert_eq!(vec.len(), 768);
    }

    #[tokio::test]
    async fn embed_query_text_returns_correct_dimensionality() {
        let server = test_server();
        let vec = server.embed_query_text("test query").await.unwrap();
        assert_eq!(vec.len(), 768);
    }

    #[test]
    fn strs_converts_some() {
        let input = Some(vec!["a".to_string(), "b".to_string()]);
        let result = strs(&input);
        assert_eq!(result, Some(vec!["a", "b"]));
    }

    #[test]
    fn strs_converts_none() {
        let input: Option<Vec<String>> = None;
        assert_eq!(strs(&input), None);
    }

    #[test]
    fn strs_converts_empty_vec() {
        let input = Some(vec![]);
        let result = strs(&input);
        assert_eq!(result, Some(vec![] as Vec<&str>));
    }

    #[test]
    fn strs_owned_converts_non_empty() {
        let input = vec!["a".to_string(), "b".to_string()];
        let result = strs_owned(&input);
        assert_eq!(result, vec!["a", "b"]);
    }

    #[test]
    fn strs_owned_converts_empty() {
        let input: Vec<String> = vec![];
        let result = strs_owned(&input);
        assert!(result.is_empty());
    }

    #[test]
    fn update_input_tristate_deserialization() {
        // Field absent → NoChange
        let input: UpdateInput = serde_json::from_str(r#"{"id": "abc"}"#).unwrap();
        assert_eq!(FieldUpdate::from(input.memory_type), FieldUpdate::NoChange);

        // Field explicitly null → Clear
        let input: UpdateInput = serde_json::from_str(r#"{"id": "abc", "type": null}"#).unwrap();
        assert_eq!(FieldUpdate::from(input.memory_type), FieldUpdate::Clear);

        // Field set to value → Set
        let input: UpdateInput =
            serde_json::from_str(r#"{"id": "abc", "type": "pattern"}"#).unwrap();
        assert_eq!(
            FieldUpdate::from(input.memory_type),
            FieldUpdate::Set("pattern".to_string())
        );
    }

    #[tokio::test]
    async fn tool_context_single_query() {
        let server = test_server();

        // Store a memory.
        server
            .tool_store(Parameters(StoreInput {
                content: "Always use snake_case for Rust function names".to_string(),
                projects: vec!["coding".to_string()],
                memory_type: Some("preference".to_string()),
                tags: vec!["rust".to_string()],
                links: vec![],
            }))
            .await
            .unwrap();

        // Call context with a single query.
        let result = server
            .tool_context(Parameters(ContextInput {
                queries: vec!["rust naming conventions".to_string()],
                projects: None,
                memory_type: None,
                tags: None,
                include_global: None,
                include_taxonomy: None,
                content_budget: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let resp: ContextResponse = serde_json::from_str(extract_text(&result)).unwrap();
        assert!(!resp.memories.is_empty(), "should find the stored memory");

        let hit = &resp.memories[0];
        assert_eq!(hit.content, "Always use snake_case for Rust function names");
        assert_eq!(hit.projects, vec!["coding"]);
        assert_eq!(hit.memory_type, Some("preference".to_string()));
        assert_eq!(hit.tags, vec!["rust"]);
        assert_eq!(hit.query_index, 0);
        assert!(hit.score > 0.0);
    }

    #[tokio::test]
    async fn tool_context_multiple_queries_dedup() {
        let server = test_server();

        // Store three memories with distinct content.
        server
            .tool_store(Parameters(StoreInput {
                content: "Memory A about Rust error handling".to_string(),
                projects: vec!["proj".to_string()],
                memory_type: None,
                tags: vec!["rust".to_string()],
                links: vec![],
            }))
            .await
            .unwrap();
        server
            .tool_store(Parameters(StoreInput {
                content: "Memory B about Rust testing and error handling".to_string(),
                projects: vec!["proj".to_string()],
                memory_type: None,
                tags: vec!["rust".to_string(), "testing".to_string()],
                links: vec![],
            }))
            .await
            .unwrap();
        server
            .tool_store(Parameters(StoreInput {
                content: "Memory C about Rust testing strategies".to_string(),
                projects: vec!["proj".to_string()],
                memory_type: None,
                tags: vec!["testing".to_string()],
                links: vec![],
            }))
            .await
            .unwrap();

        // Two queries that may return overlapping results.
        let result = server
            .tool_context(Parameters(ContextInput {
                queries: vec![
                    "error handling".to_string(),
                    "testing strategies".to_string(),
                ],
                projects: None,
                memory_type: None,
                tags: None,
                include_global: None,
                include_taxonomy: None,
                content_budget: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let resp: ContextResponse = serde_json::from_str(extract_text(&result)).unwrap();

        // Should have no duplicate IDs.
        let ids: Vec<&str> = resp.memories.iter().map(|h| h.id.as_str()).collect();
        let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(
            ids.len(),
            unique.len(),
            "results should have no duplicate IDs"
        );

        // All three memories should appear in the results.
        assert_eq!(
            resp.memories.len(),
            3,
            "all three memories should be returned"
        );
    }

    #[tokio::test]
    async fn tool_context_query_index_reflects_best_query() {
        let server = test_server();

        // Store a memory that both queries will find.
        let store_result = server
            .tool_store(Parameters(StoreInput {
                content: "Rust error handling patterns and testing".to_string(),
                projects: vec![],
                memory_type: None,
                tags: vec![],
                links: vec![],
            }))
            .await
            .unwrap();
        let store_resp: StoreResponse = serde_json::from_str(extract_text(&store_result)).unwrap();

        // Two queries — one will score higher than the other.
        let result = server
            .tool_context(Parameters(ContextInput {
                queries: vec![
                    "completely unrelated topic xyz".to_string(),
                    "Rust error handling patterns and testing".to_string(),
                ],
                projects: None,
                memory_type: None,
                tags: None,
                include_global: None,
                include_taxonomy: None,
                content_budget: None,
                limit: None,
            }))
            .await
            .unwrap();
        let resp: ContextResponse = serde_json::from_str(extract_text(&result)).unwrap();

        // Find our memory in results.
        let hit = resp.memories.iter().find(|h| h.id == store_resp.id);
        assert!(hit.is_some(), "stored memory should appear in results");
        let hit = hit.unwrap();

        // query_index should point to a valid query.
        assert!(
            hit.query_index < 2,
            "query_index should be 0 or 1, got {}",
            hit.query_index
        );
    }

    #[tokio::test]
    async fn tool_context_content_budget_enforcement() {
        let server = test_server();

        // Store several memories with known content lengths.
        // Each ~50 chars.
        for i in 0..5 {
            server
                .tool_store(Parameters(StoreInput {
                    content: format!("Memory number {} with some content padding here!!", i),
                    projects: vec![],
                    memory_type: None,
                    tags: vec![],
                    links: vec![],
                }))
                .await
                .unwrap();
        }

        // Set a very small content budget that should only fit ~1-2 memories.
        let result = server
            .tool_context(Parameters(ContextInput {
                queries: vec!["memory content padding".to_string()],
                projects: None,
                memory_type: None,
                tags: None,
                include_global: None,
                include_taxonomy: None,
                content_budget: Some(60),
                limit: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let resp: ContextResponse = serde_json::from_str(extract_text(&result)).unwrap();

        // With budget of 60 chars, at most 1-2 memories should be returned
        // (each is ~50 chars).
        assert!(
            resp.memories.len() < 5,
            "content budget should limit results, got {} memories",
            resp.memories.len()
        );

        // The response should be marked truncated since not all results fit.
        assert!(
            resp.truncated,
            "response should be truncated when budget is exceeded"
        );
    }

    #[tokio::test]
    async fn tool_context_limit_caps_results() {
        let server = test_server();

        // Store 5 memories.
        for i in 0..5 {
            server
                .tool_store(Parameters(StoreInput {
                    content: format!("Limit test memory number {i}"),
                    projects: vec![],
                    memory_type: None,
                    tags: vec![],
                    links: vec![],
                }))
                .await
                .unwrap();
        }

        // Set limit=2.
        let result = server
            .tool_context(Parameters(ContextInput {
                queries: vec!["limit test memory".to_string()],
                projects: None,
                memory_type: None,
                tags: None,
                include_global: None,
                include_taxonomy: None,
                content_budget: Some(100_000), // large budget so it doesn't interfere
                limit: Some(2),
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let resp: ContextResponse = serde_json::from_str(extract_text(&result)).unwrap();
        assert_eq!(
            resp.memories.len(),
            2,
            "limit=2 should return exactly 2 results, got {}",
            resp.memories.len()
        );
    }

    #[tokio::test]
    async fn tool_context_include_taxonomy() {
        let server = test_server();

        // Store a memory so the DB isn't empty.
        server
            .tool_store(Parameters(StoreInput {
                content: "Taxonomy test memory".to_string(),
                projects: vec!["my-project".to_string()],
                memory_type: Some("decision".to_string()),
                tags: vec!["important".to_string()],
                links: vec![],
            }))
            .await
            .unwrap();

        // Call context with include_taxonomy: true.
        let result = server
            .tool_context(Parameters(ContextInput {
                queries: vec!["taxonomy test".to_string()],
                projects: None,
                memory_type: None,
                tags: None,
                include_global: None,
                include_taxonomy: Some(true),
                content_budget: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let resp: ContextResponse = serde_json::from_str(extract_text(&result)).unwrap();

        // Taxonomy should be populated.
        assert!(resp.taxonomy.is_some(), "taxonomy should be present");
        let taxonomy = resp.taxonomy.unwrap();
        assert!(
            !taxonomy.projects.is_empty(),
            "projects should be populated"
        );
        assert_eq!(taxonomy.projects[0].name, "my-project");
        assert_eq!(taxonomy.stats.total_memories, 1);
    }

    #[tokio::test]
    async fn tool_context_taxonomy_omitted_by_default() {
        let server = test_server();

        server
            .tool_store(Parameters(StoreInput {
                content: "Some memory for no-taxonomy test".to_string(),
                projects: vec![],
                memory_type: None,
                tags: vec![],
                links: vec![],
            }))
            .await
            .unwrap();

        // Call context without include_taxonomy (defaults to false).
        let result = server
            .tool_context(Parameters(ContextInput {
                queries: vec!["some memory".to_string()],
                projects: None,
                memory_type: None,
                tags: None,
                include_global: None,
                include_taxonomy: None,
                content_budget: None,
                limit: None,
            }))
            .await
            .unwrap();
        let resp: ContextResponse = serde_json::from_str(extract_text(&result)).unwrap();
        assert!(
            resp.taxonomy.is_none(),
            "taxonomy should be omitted by default"
        );
    }

    #[tokio::test]
    async fn tool_context_empty_queries_returns_error() {
        let server = test_server();

        let result = server
            .tool_context(Parameters(ContextInput {
                queries: vec![],
                projects: None,
                memory_type: None,
                tags: None,
                include_global: None,
                include_taxonomy: None,
                content_budget: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert!(
            result.is_error.unwrap_or(false),
            "empty queries should return an error"
        );
        let text = extract_text(&result);
        assert!(
            text.contains("queries must not be empty"),
            "error should mention empty queries, got: {text}"
        );
    }

    #[tokio::test]
    async fn tool_context_too_many_queries_returns_error() {
        let server = test_server();

        let result = server
            .tool_context(Parameters(ContextInput {
                queries: vec![
                    "q1".into(),
                    "q2".into(),
                    "q3".into(),
                    "q4".into(),
                    "q5".into(),
                    "q6".into(),
                ],
                projects: None,
                memory_type: None,
                tags: None,
                include_global: None,
                include_taxonomy: None,
                content_budget: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert!(
            result.is_error.unwrap_or(false),
            ">5 queries should return an error"
        );
        let text = extract_text(&result);
        assert!(
            text.contains("too many queries"),
            "error should mention too many queries, got: {text}"
        );
    }

    #[tokio::test]
    async fn tool_context_filters_apply_to_all_queries() {
        let server = test_server();

        // Store memories in different projects.
        server
            .tool_store(Parameters(StoreInput {
                content: "Frontend React component patterns".to_string(),
                projects: vec!["frontend".to_string()],
                memory_type: None,
                tags: vec![],
                links: vec![],
            }))
            .await
            .unwrap();
        server
            .tool_store(Parameters(StoreInput {
                content: "Backend API design patterns".to_string(),
                projects: vec!["backend".to_string()],
                memory_type: None,
                tags: vec![],
                links: vec![],
            }))
            .await
            .unwrap();

        // Filter by project=frontend with include_global=false.
        let result = server
            .tool_context(Parameters(ContextInput {
                queries: vec!["component patterns".to_string(), "API design".to_string()],
                projects: Some(vec!["frontend".to_string()]),
                memory_type: None,
                tags: None,
                include_global: Some(false),
                include_taxonomy: None,
                content_budget: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let resp: ContextResponse = serde_json::from_str(extract_text(&result)).unwrap();

        // Only the frontend memory should appear.
        assert_eq!(
            resp.memories.len(),
            1,
            "only frontend memory should be returned, got {}",
            resp.memories.len()
        );
        assert!(
            resp.memories[0].content.contains("Frontend"),
            "returned memory should be from frontend project"
        );
        assert_eq!(resp.memories[0].projects, vec!["frontend"]);
    }

    #[test]
    fn from_memory_to_memory_response_serde() {
        let mem = Memory {
            id: "mem-lr1".into(),
            content: "list response content".into(),
            memory_type: None,
            projects: vec!["proj-z".into()],
            tags: vec![],
            created_at: "2025-04-01T00:00:00Z".into(),
            updated_at: "2025-04-02T00:00:00Z".into(),
            archived_at: None,
            last_accessed_at: None,
            access_count: 0,
            truncated: false,
        };

        let resp = MemoryResponseSerde::from(mem);
        assert_eq!(resp.id, "mem-lr1");
        assert_eq!(resp.content, "list response content");
        assert_eq!(resp.memory_type, None);
        assert_eq!(resp.projects, vec!["proj-z"]);
        assert!(resp.tags.is_empty());
        assert!(!resp.truncated);
    }

    #[test]
    fn from_search_hit_to_search_hit_response() {
        let hit = SearchHit {
            memory: Memory {
                id: "mem-s01".into(),
                content: "search result content".into(),
                memory_type: Some("decision".into()),
                projects: vec!["proj-x".into()],
                tags: vec!["important".into()],
                created_at: "2025-03-01T00:00:00Z".into(),
                updated_at: "2025-03-02T00:00:00Z".into(),
                archived_at: None,
                last_accessed_at: None,
                access_count: 3,
                truncated: true,
            },
            outgoing_links: vec![],
            incoming_links: vec![Link {
                id: "link-in".into(),
                source_id: "mem-other".into(),
                target_id: "mem-s01".into(),
                relation: "caused_by".into(),
                created_at: "2025-03-01T00:00:00Z".into(),
                content: None,
            }],
            score: 0.85,
        };

        let resp = SearchHitResponse::from(hit);
        assert_eq!(resp.id, "mem-s01");
        assert_eq!(resp.content, "search result content");
        assert_eq!(resp.memory_type, Some("decision".into()));
        assert_eq!(resp.score, 0.85);
        assert!(resp.truncated);
        assert!(resp.links.outgoing.is_empty());
        assert_eq!(resp.links.incoming.len(), 1);
        assert_eq!(resp.access_count, 3);
    }

    #[test]
    fn from_memory_with_links_to_full_response() {
        let mwl = MemoryWithLinks {
            memory: Memory {
                id: "mem-001".into(),
                content: "test content".into(),
                memory_type: Some("pattern".into()),
                projects: vec!["proj-a".into()],
                tags: vec!["rust".into()],
                created_at: "2025-01-01T00:00:00Z".into(),
                updated_at: "2025-01-02T00:00:00Z".into(),
                archived_at: None,
                last_accessed_at: None,
                access_count: 5,
                truncated: false,
            },
            outgoing_links: vec![Link {
                id: "link-1".into(),
                source_id: "mem-001".into(),
                target_id: "mem-002".into(),
                relation: "related_to".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                content: None,
            }],
            incoming_links: vec![],
        };

        let resp = MemoryFullResponse::from(mwl);
        assert_eq!(resp.id, "mem-001");
        assert_eq!(resp.content, "test content");
        assert_eq!(resp.memory_type, Some("pattern".into()));
        assert_eq!(resp.projects, vec!["proj-a"]);
        assert_eq!(resp.tags, vec!["rust"]);
        assert_eq!(resp.links.outgoing.len(), 1);
        assert_eq!(resp.links.outgoing[0].id, "link-1");
        assert!(resp.links.incoming.is_empty());
        assert_eq!(resp.access_count, 5);
    }
}
