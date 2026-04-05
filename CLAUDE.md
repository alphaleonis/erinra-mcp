# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Erinra is a memory MCP server for LLM coding assistants — a single Rust binary that stores, indexes, and retrieves memories via stdio MCP transport. It uses SQLite for storage, fastembed for local embeddings, and sqlite-vec for vector search. The core design principle: the server provides storage and retrieval, the calling LLM makes all semantic decisions (dedup, merge, corrections).

**Status**: Alpha implementation in progress. Core modules implemented: MCP server (`src/mcp/`), database with CRUD + hybrid search + FTS5 escaping + RRF merge (`src/db/`), embeddings + reranker (`src/embedding/`), sync with background export/import (`src/sync/`), web dashboard + shared daemon + Bearer token auth (`src/web/`), stdio-to-HTTP relay (`src/relay.rs`), CLI with `serve`, `export`, `import`, `sync`, `reembed`, `status`, `models`, and `dash` commands (`src/main.rs`), configuration (`src/config.rs`).

## Commands

```bash
mise run build                       # Build everything (frontend + Rust, debug)
mise run build:release               # Build for distribution (frontend + licenses + release binary)
mise run ci                          # Full CI pipeline: install, check, test, build release
mise run build:rust                  # Build Rust binary only (debug)
mise run build:web                   # Build frontend SPA only
mise run check                       # Type-check and lint (cargo fmt + clippy + svelte-check)
mise run test                        # Run fast tests (Rust + frontend unit)
mise run test:e2e                    # Run Playwright e2e tests
mise run test:all                    # Run all tests (unit + e2e)
mise run licenses                    # Regenerate THIRD_PARTY_LICENSES.txt
mise run dev                         # Build frontend + run dashboard in debug mode
mise run run -- <args>               # Build frontend + run binary with given arguments
cargo test                           # Run Rust unit tests directly (~350 tests, in-memory SQLite)
cargo test <test_name>               # Run a single Rust test by name
cd web && npx vitest run             # Run frontend tests directly (~90 tests)
cd web && npx playwright test        # Run E2E tests (starts daemon, seeds DB, runs Chromium)
```

Note: First `serve` run downloads the Nomic embedding model (~137 MB) to `~/.erinra/models/`.

**After any code changes**, always run `mise run check` to verify that nothing is broken. This applies after every edit, not just before committing.

## Architecture (from design doc)

```
erinra/
  src/
    main.rs              # CLI entry point (clap), subcommands: serve, export, import, sync, reembed, status, dash, models
    lib.rs               # Crate root, re-exports public modules
    config.rs            # TOML config with env var overrides and validation
    relay.rs             # Stdio-to-HTTP relay for shared daemon mode (bridges stdin/stdout to daemon's /mcp endpoint)
    mcp/                 # MCP server, stdio JSON-RPC, tool handlers (11 tools including context)
    db/                  # SQLite schema, queries, migrations, hybrid search + RRF merge (rusqlite + FTS5 + sqlite-vec)
    embedding/           # fastembed wrapper, embedding + reranker model management, async startup loading
    sync/                # JSONL export/import, filesystem watching (notify), per-machine sync
    web/                 # Axum web server, dashboard SPA, daemon coordination, MCP HTTP endpoint
      mod.rs             # AppState, app_router (API + MCP + SPA), serve()
      auth.rs            # Bearer token middleware, token generation
      daemon.rs          # DaemonState, state file locking, spawn/join/cleanup
      routes.rs          # REST API handlers for dashboard
  web/                   # SvelteKit 2 + Svelte 5 + Tailwind CSS 4 frontend SPA
```

Rust 2024 edition. Key dependencies: `rusqlite`, `fastembed`, `sqlite-vec` (static), `rmcp` + `schemars` (MCP server, stdio + HTTP transports), `axum` + `tower-http` (web server), `reqwest` (relay HTTP client), `uuid`, `serde`/`serde_json`, `flate2`, `clap`, `tracing`, `anyhow`/`thiserror` (error handling), `rand` (auth token generation).

## Key Design Decisions

- **No server-side reasoning** — no dedup, contradiction detection, or merge logic. The LLM decides.
- **Non-destructive** — `store` always creates, `archive` instead of delete. `update` is the only destructive operation (overwrites content).
- **sqlite-vec over USearch** — chosen for SQLite WAL concurrency (multiple Claude Code sessions sharing one DB). USearch has no multi-process support.
- **Hybrid search** — vector search + FTS5 merged via RRF. FTS5 queries are escaped (literal text, no operators).
- **Fat responses** — `store` returns top-3 similar memories with full content so the LLM can react without follow-up calls.
- **Sync via JSONL** — each machine exports to a per-machine file, imports others. No SQLite file-level sync.

## Code Style

- **Error handling**: `anyhow::Result` for application-level functions, `thiserror` for typed errors in library modules. MCP tool handlers convert errors to JSON-RPC error responses.
- **Module structure**: Each module is a directory with `mod.rs`. `db/` splits into `mod.rs` (schema, Database struct), `types.rs` (data types), `helpers.rs` (shared query helpers), `ops_core.rs` (CRUD operations), `ops_search.rs` (hybrid search), `ops_sync.rs` (export/import), `search.rs` (FTS5 escaping + RRF merge, `pub(super)` visibility). `mcp/` splits into `mod.rs` (server struct, ServerHandler, startup), `types.rs` (input/output types, From conversions), `handlers.rs` (tool implementations). `web/` splits into `mod.rs` (AppState, app_router, serve), `auth.rs` (Bearer token middleware), `daemon.rs` (state file, process coordination), `routes.rs` (REST API handlers).
- **Relay mode**: `serve` checks for a running daemon before loading models. If daemon is alive, `relay::run_relay()` bridges stdin/stdout to the daemon's `/mcp` HTTP endpoint. Falls back to standalone on connection failure. Tests in `relay.rs` use real Axum servers via `tokio::net::TcpListener`.
- **Testing**: Tests use `#[cfg(test)]` modules within each source file. DB tests use `Connection::open_in_memory()` with a mock `Embedder`.
- **Nullable update fields**: `FieldUpdate<T>` (in `db/types.rs`) for nullable columns in `UpdateParams` where `Option<T>` can't distinguish "no change" from "clear". Non-nullable fields and collections still use `Option<T>`.

## Issue Tracking

This project uses **nibs** for issue tracking. Use `nibs list`, `nibs show <id>`, `nibs create`. See `.nibs/` directory. Always use nibs instead of TodoWrite for work tracking.

**Important**: `.nibs/` is a **separate git repository** (gitignored from this repo). Commit and push nibs changes from within `.nibs/`, not from the project root.

**Milestone requirement**: Every new nib must have a milestone as its parent. Use `--parent <milestone-id>` when creating. The current milestone is **1.0.0 Alpha** (`erin-jal4`) — use it as the default parent unless a different milestone is specified.

## When Executing Plans

When working from a plan or work item:

- **Auto-fix without asking:** bug fixes, type errors, missing imports, broken references, missing null checks, obvious error handling gaps
- **Auto-add without asking:** necessary validation, missing error handling on external calls, required trait implementations, derive macros needed for compilation
- **Ask before doing:** new dependencies/packages, schema or data model changes, architectural decisions not covered in the plan, changes to public APIs or MCP tool contracts, anything that affects sync/export format
- **Never without explicit approval:** delete or restructure files not mentioned in the plan, change build/CI configuration, modify the MCP protocol surface, change database migration behavior

