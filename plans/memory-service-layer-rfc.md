# RFC: Extract MemoryService Layer

## Problem

The MCP tool handlers (`src/mcp/handlers.rs`, ~1,000 lines) and web route handlers (`src/web/routes.rs`, ~1,300 lines) independently implement the same multi-step orchestration for every operation:

1. Validate input (content size check)
2. Embed content/query on a blocking thread (`spawn_blocking` around `dyn Embedder`)
3. Acquire DB mutex and execute operation (`spawn_blocking { db.lock(); db.op() }`)
4. Apply config values (`similar_limit`, `rrf_k`, `reranker_threshold`, `content_max_length`)
5. Post-process results (filter similar by threshold, truncate content)
6. Convert to wire format

This creates several concrete problems:

- **Duplicated orchestration**: The `Arc::clone → spawn_blocking → db.lock() → db.op()` pattern appears verbatim in every handler. The `strs()` / `strs_owned()` borrow-to-slice conversion is repeated in every handler body.
- **Config inconsistency**: The web search route hardcodes `rrf_k: 60` instead of reading from `ServerConfig`. The web layer holds `reranker_threshold` as a loose field on `AppState` while MCP reads it from `Arc<ServerConfig>`.
- **Untestable orchestration**: Testing "store + find_similar with threshold filtering" requires either a full MCP JSON-RPC round-trip or manually constructing `StoreParams` with pre-computed embeddings (bypassing the embed→store→similar pipeline).
- **Behavioral divergence**: Time filter resolution (relative age → absolute timestamps) lives only in the MCP layer. The web layer passes raw timestamps, skipping validation. Adding relative-age support to the web layer would require re-implementing the resolution logic.

## Proposed Interface

A concrete `MemoryService` struct in `src/service.rs` that owns all shared state and exposes one async method per logical operation. Both transport layers become thin shims that parse wire format, call the service, and convert the result.

### Core struct

```rust
#[derive(Clone)]
pub struct MemoryService {
    db: Arc<Mutex<Database>>,
    embedder: Arc<dyn Embedder>,
    reranker: Option<Arc<dyn Reranker>>,
    config: Arc<ServiceConfig>,
}
```

### Error type

```rust
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("embedding failed: {0}")]
    Embedding(#[from] anyhow::Error),
    #[error("{0}")]
    InvalidInput(String),
}

impl ServiceError {
    pub fn is_user_facing(&self) -> bool {
        match self {
            Self::Db(e) => e.is_user_facing(),
            Self::Embedding(_) => false,
            Self::InvalidInput(_) => true,
        }
    }
}
```

### Request types (owned, no lifetimes)

```rust
pub struct StoreRequest {
    pub content: String,
    pub memory_type: Option<String>,
    pub projects: Vec<String>,
    pub tags: Vec<String>,
    pub links: Vec<(String, String)>,  // (target_id, relation)
}

pub struct UpdateRequest {
    pub id: String,
    pub content: Option<String>,
    pub memory_type: FieldUpdate<String>,
    pub projects: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
}

pub struct MergeRequest {
    pub source_ids: Vec<String>,
    pub content: String,
    pub memory_type: Option<String>,
    pub projects: Vec<String>,
    pub tags: Vec<String>,
}

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
}

pub struct ListRequest {
    pub projects: Option<Vec<String>>,
    pub memory_type: Option<String>,
    pub tags: Option<Vec<String>>,
    pub include_global: bool,
    pub include_archived: bool,
    pub time: ResolvedTimeFilters,
    pub limit: u32,
    pub offset: u32,
}

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
```

### Response types

```rust
pub struct StoredMemory {
    pub id: String,
    pub similar: Vec<(Memory, f64)>,  // threshold already applied
}

pub struct MergedMemory {
    pub id: String,
    pub archived: Vec<String>,
    pub similar: Vec<(Memory, f64)>,
}

pub struct ContextResult {
    pub hits: Vec<ContextHitInner>,
    pub taxonomy: Option<DiscoverResult>,
    pub truncated: bool,
}

pub struct ContextHitInner {
    pub memory: Memory,
    pub score: f64,
    pub query_index: usize,
}
```

### Service methods (15 total)

```rust
impl MemoryService {
    pub fn new(db, embedder, reranker, config) -> Self;

    // Write operations (embed + DB + post-process)
    pub async fn store(&self, req: StoreRequest) -> ServiceResult<StoredMemory>;
    pub async fn update(&self, req: UpdateRequest) -> ServiceResult<UpdateResult>;
    pub async fn merge(&self, req: MergeRequest) -> ServiceResult<MergedMemory>;

    // Mutation pass-throughs (DB only)
    pub async fn archive(&self, id: &str) -> ServiceResult<ArchiveResult>;
    pub async fn unarchive(&self, id: &str) -> ServiceResult<UnarchiveResult>;
    pub async fn bulk_archive(&self, ids: &[String]) -> ServiceResult<Vec<ArchiveResult>>;
    pub async fn bulk_unarchive(&self, ids: &[String]) -> ServiceResult<Vec<UnarchiveResult>>;
    pub async fn link(&self, source: &str, target: &str, relation: &str) -> ServiceResult<Link>;
    pub async fn unlink_by_id(&self, id: &str) -> ServiceResult<usize>;
    pub async fn unlink_by_endpoints(&self, source: &str, target: &str, relation: &str) -> ServiceResult<usize>;

    // Read operations (embed query + DB + config applied)
    pub async fn search(&self, req: SearchRequest) -> ServiceResult<SearchResult>;
    pub async fn list(&self, req: ListRequest) -> ServiceResult<ListResult>;
    pub async fn get(&self, ids: &[String]) -> ServiceResult<Vec<MemoryWithLinks>>;
    pub async fn discover(&self) -> ServiceResult<DiscoverResult>;

    // Compound (MCP context tool)
    pub async fn context(&self, req: ContextRequest) -> ServiceResult<ContextResult>;

    // Composable stage (from Design B hybrid)
    pub async fn find_similar(&self, embedding: &[f32], exclude_ids: &[&str], content_max_length: Option<u32>) -> ServiceResult<Vec<(Memory, f64)>>;

    // Config accessor
    pub fn config(&self) -> &ServiceConfig;
}
```

### Conversion helpers on request types

```rust
impl StoreRequest {
    pub fn from_mcp(input: StoreInput) -> Self { ... }
}
impl SearchRequest {
    pub fn from_mcp(input: SearchInput) -> Result<Self, String> { ... } // resolves time filters
    pub fn from_web(query: &SearchMemoriesQuery) -> Result<Self, String> { ... }
}
impl ListRequest {
    pub fn from_mcp(input: ListInput) -> Result<Self, String> { ... }
    pub fn from_web(query: &ListMemoriesQuery) -> Self { ... }
}
```

### Usage: MCP handler shrinks to ~5 lines

```rust
async fn tool_store(&self, params: Parameters<StoreInput>) -> Result<CallToolResult, ErrorData> {
    let req = StoreRequest::from_mcp(params.0);
    let result = svc_result!(self.service.store(req).await)?;
    self.refresh_instructions();
    json_result(&StoreResponse::from(result))
}
```

### Usage: Web route becomes ~10 lines

```rust
async fn search_memories(State(state): State<AppState>, uri: axum::http::Uri) -> Response {
    let q = SearchMemoriesQuery::from_query(uri.query().unwrap_or(""));
    let req = match SearchRequest::from_web(&q) {
        Ok(r) => r,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    match state.service.search(req).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => service_error_to_response(e),
    }
}
```

### What the service hides internally

- `spawn_blocking` + mutex acquisition (the `Arc::clone → db.lock()` dance)
- Embedding invocation (`embed_documents` / `embed_query` on blocking thread)
- Config application (`similar_limit`, `similar_threshold`, `rrf_k`, `reranker_threshold`, `content_max_length`)
- Reranker threading (`Option<Arc<dyn Reranker>>` → `Option<&dyn Reranker>` into `SearchParams`)
- `find_similar` follow-up after store/merge with threshold filtering and ID exclusion
- Owned-to-borrowed conversion (`String` → `&str`, `Vec<String>` → `&[&str]`) inside blocking closures
- JoinError handling from `spawn_blocking`

## Dependency Strategy

**Category: In-process.** All dependencies (Database, Embedder, Reranker) are passed at construction time as `Arc`-wrapped values — identical to the current `ErinraServer` construction pattern. No network boundaries, no I/O adapters needed.

`MemoryService` is a concrete struct, not a trait. There is one implementation. Tests use `MockEmbedder` + `Database::open_in_memory()` directly, exactly like the existing DB tests. A trait would add async-in-trait complexity and force callers to parameterize over the trait, complicating `ErinraServer` and `AppState` types for no benefit.

`ErinraServer` and `AppState` hold `Arc<MemoryService>` (or `MemoryService` directly since it's `Clone`-cheap). They drop their direct `db`/`embedder`/`reranker` fields for service-covered operations. `ErinraServer` retains direct `db` access only for `build_instructions()` (taxonomy cache, runs during construction).

## Testing Strategy

### New boundary tests to write

Test the service methods directly with `MockEmbedder` + in-memory `Database`:

- **store → similar**: Store a memory, verify returned `similar` list is filtered by threshold and excludes the stored memory's own ID
- **update with content change**: Verify re-embedding occurs (content hash changes vector)
- **update without content change**: Verify embedding is NOT recomputed
- **merge → similar**: Merge N memories, verify archived IDs excluded from similar results
- **search with config**: Verify `rrf_k`, `reranker_threshold`, `content_max_length` are applied from config (not caller)
- **search with time filters**: Pass `ResolvedTimeFilters`, verify filtering
- **content size validation**: Verify `InvalidInput` error for oversized content before embedding runs
- **bulk_archive skip semantics**: Verify user-facing errors are skipped, internal errors propagate
- **find_similar standalone**: Verify the composable stage returns threshold-filtered results independently

### Old tests that become partially redundant

- MCP handler tests in `handlers.rs` that test orchestration (embed + DB + similar) — these shrink to testing wire-format conversion only
- Web route time-filter tests in `routes.rs` — the service tests cover the filtering; route tests only verify HTTP parsing
- The `filter_similar` tests in `handlers.rs` — absorbed into service store/merge tests

### Test environment needs

No new infrastructure. Same `MockEmbedder::new(768)` + `Database::open_in_memory()` pattern used throughout the codebase. The service tests are synchronous-over-async using `#[tokio::test]`.

## Implementation Recommendations

### What the service module should own

- All orchestration logic: embed → DB op → post-process
- Config values: the `ServiceConfig` struct (renamed from `ServerConfig`) with all tunable constants
- Time filter resolution: `resolve_time_filters`, `validate_time_filter_input`, `validate_resolved_ranges` — moved from `mcp/handlers.rs`
- `filter_similar` helper — moved from `mcp/handlers.rs`
- `ServiceError` type that unifies `DbError`, embedding errors, and validation errors
- Owned request types (`StoreRequest`, `SearchRequest`, etc.) that insulate callers from the lifetime-bearing DB param types

### What it should hide

- `tokio::task::spawn_blocking` and mutex acquisition — callers never see these
- `Vec<f32>` embeddings — callers pass `String`, service manages the vectors
- `strs()` / `strs_owned()` borrow conversion — happens inside blocking closures
- `similar_limit`, `similar_threshold`, `rrf_k`, `reranker_threshold` — applied internally
- The fact that store/merge do a second `find_similar` query after the write

### What it should expose

- One async method per operation with owned request/response types
- `find_similar` as a standalone composable stage (hybrid from Design B)
- `config()` accessor for transport-specific needs (MCP instruction caching reads `max_content_size` for validation before calling the service)
- `ServiceError` with `is_user_facing()` for transport-layer error mapping

### How callers should migrate

1. Create `src/service.rs` with `MemoryService`, `ServiceConfig`, `ServiceError`, request/response types
2. Move time resolution helpers from `mcp/handlers.rs` to `service.rs` (or `src/service/time.rs`)
3. Move `filter_similar` from `mcp/handlers.rs` to `service.rs`
4. Implement service methods by extracting orchestration logic from handlers
5. Update `ErinraServer` to hold `MemoryService` instead of raw `db`/`embedder`/`reranker`
6. Rewrite MCP handlers as thin shims: parse input → `from_mcp()` → service call → convert response
7. Update `AppState` to hold `MemoryService`, remove `reranker_threshold` loose field
8. Rewrite web routes to use service (fixes `rrf_k: 60` hardcode)
9. Add `from_mcp()` / `from_web()` conversion methods on request types
10. Write service-level boundary tests
11. Slim down MCP/web tests to only verify wire-format translation

### Notable side effects

- Fixes latent bug: web route `search_memories` hardcodes `rrf_k: 60` instead of reading from config
- Fixes config inconsistency: `reranker_threshold` moves from loose `AppState` field to `ServiceConfig`
- Enables future web CRUD: if the dashboard later needs store/update/merge, the service already supports it
- `find_similar` as a public method enables the sync module to use it for dedup during import (currently not possible without going through MCP)
