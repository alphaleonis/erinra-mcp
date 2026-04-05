//! MCP server and tool handlers (stdio JSON-RPC via rmcp).

mod handlers;
mod types;

use std::sync::Arc;

use rmcp::handler::server::tool::{ToolCallContext, ToolRouter};
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde::Serialize;

use crate::db::Database;
use crate::service::MemoryService;

// ── Server struct ───────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ErinraServer {
    service: MemoryService,
    /// Cached MCP instructions (taxonomy snapshot). Updated after mutations
    /// so `get_info()` can read without acquiring the DB mutex.
    instructions: Arc<std::sync::RwLock<String>>,
    tool_router: ToolRouter<Self>,
}

impl ErinraServer {
    pub fn new(service: MemoryService) -> Self {
        let instructions = {
            let db_guard = service.db().lock().expect("db mutex poisoned");
            Self::build_instructions(&db_guard).unwrap_or_default()
        };
        Self {
            service,
            instructions: Arc::new(std::sync::RwLock::new(instructions)),
            tool_router: Self::create_tool_router(),
        }
    }

    /// Refresh the cached instructions after a mutation.
    /// Spawns a background task so the handler can return immediately.
    pub(crate) fn refresh_instructions(&self) {
        let service = self.service.clone();
        let instructions = Arc::clone(&self.instructions);
        tokio::task::spawn(async move {
            match service.discover().await {
                Ok(discover) => {
                    let s = Self::build_instructions_from_discover(&discover);
                    *instructions.write().expect("instructions lock poisoned") = s;
                }
                Err(e) => {
                    tracing::warn!("failed to refresh instructions: {e}");
                }
            }
        });
    }

    fn build_instructions(db: &Database) -> anyhow::Result<String> {
        let discover = db.discover()?;
        Ok(Self::build_instructions_from_discover(&discover))
    }

    fn build_instructions_from_discover(discover: &crate::db::types::DiscoverResult) -> String {
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

        parts.join("\n")
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
pub async fn serve(service: MemoryService) -> anyhow::Result<()> {
    let server = ErinraServer::new(service);
    let transport = rmcp::transport::io::stdio();
    let mcp_service = server.serve(transport).await?;
    mcp_service.waiting().await?;
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::types::*;
    use super::*;
    use crate::db::DbConfig;
    use crate::db::types::*;
    use crate::embedding::MockEmbedder;
    use crate::service::ServiceConfig;

    fn test_service() -> crate::service::MemoryService {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let embedder = Arc::new(MockEmbedder::new(768));
        crate::service::MemoryService::new(
            Arc::new(Mutex::new(db)),
            embedder,
            None,
            ServiceConfig::default(),
        )
    }

    fn test_server() -> ErinraServer {
        ErinraServer::new(test_service())
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
    async fn refresh_instructions_after_store() {
        let service = test_service();
        let server = ErinraServer::new(service.clone());

        // Empty DB → no taxonomy section.
        let instructions = server.get_info().instructions.unwrap_or_default();
        assert!(
            !instructions.contains("Current taxonomy"),
            "empty DB should have no taxonomy section"
        );

        // Store a memory with a project and type.
        service
            .store(crate::service::StoreRequest {
                content: "Rust uses ownership for memory safety".into(),
                memory_type: Some("fact".into()),
                projects: vec!["erinra".into()],
                tags: vec![],
                links: vec![],
            })
            .await
            .unwrap();

        // Refresh the cached instructions and wait for the background task.
        server.refresh_instructions();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let instructions = server.get_info().instructions.unwrap_or_default();
        assert!(
            instructions.contains("Current taxonomy"),
            "instructions should contain taxonomy after store + refresh"
        );
        assert!(
            instructions.contains("erinra (1)"),
            "instructions should list the project with count"
        );
        assert!(
            instructions.contains("fact (1)"),
            "instructions should list the type with count"
        );
    }

    #[tokio::test]
    async fn refresh_instructions_reflects_multiple_mutations() {
        let service = test_service();
        let server = ErinraServer::new(service.clone());

        // Store memories spanning different projects and types.
        for (content, mem_type, project) in [
            ("Ownership rules", "fact", "erinra"),
            ("Borrow checker tips", "fact", "erinra"),
            ("Use small PRs", "pattern", "vestige"),
            ("Error handling style", "pattern", "erinra"),
        ] {
            service
                .store(crate::service::StoreRequest {
                    content: content.into(),
                    memory_type: Some(mem_type.into()),
                    projects: vec![project.into()],
                    tags: vec![],
                    links: vec![],
                })
                .await
                .unwrap();
        }

        server.refresh_instructions();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let instructions = server.get_info().instructions.unwrap_or_default();

        // Verify projects with correct counts (3 erinra, 1 vestige).
        assert!(
            instructions.contains("erinra (3)"),
            "should show erinra with count 3, got:\n{instructions}"
        );
        assert!(
            instructions.contains("vestige (1)"),
            "should show vestige with count 1, got:\n{instructions}"
        );

        // Verify types with correct counts (2 fact, 2 pattern).
        assert!(
            instructions.contains("fact (2)"),
            "should show fact with count 2, got:\n{instructions}"
        );
        assert!(
            instructions.contains("pattern (2)"),
            "should show pattern with count 2, got:\n{instructions}"
        );
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
    async fn tool_context_empty_queries_returns_error() {
        let server = test_server();

        let result = server
            .tool_context(rmcp::handler::server::wrapper::Parameters(ContextInput {
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
            .tool_context(rmcp::handler::server::wrapper::Parameters(ContextInput {
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
