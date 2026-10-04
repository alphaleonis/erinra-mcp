# Changelog

All notable changes to this project will be documented in this file.

## Unreleased

## v0.3.0 - 2026-10-04

### Fixed
- Relay sessions now register as daemon clients, so the daemon no longer shuts down under active relays once the process that spawned it exits (e.g. Claude Code's version-negotiation probe).
- The daemon binds its port before advertising itself as ready, so a `serve` started while the daemon is loading models now relays to it instead of falling back to standalone. A port conflict or invalid bind address is reported in about half a second instead of after a 20-second wait.
- A relay that falls back to standalone no longer hangs the MCP session: input the relay had already read (including `initialize`) is replayed to the standalone server.
- The dashboard daemon now shuts down when its data directory is deleted, instead of running indefinitely and holding its port.

### Added
- Support for MCP protocol version 2026-07-28 clients, which skip `initialize` and send per-request metadata: the standalone server now serves them, and relay mode forwards the `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name` headers the daemon requires.

### Changed
- Minimum supported Rust version is now 1.95 (declared in `Cargo.toml`).
- Dependency updates: fastembed 7, rusqlite 0.40 (bundled SQLite 3.53), sysinfo 0.39, dirs 7, tower-http 0.7, rmcp 3.5, plus compatible updates across the Rust and dashboard dependency trees. Embeddings are unchanged, so existing databases need no `reembed`.
- The daemon's MCP HTTP endpoint now rejects requests whose `Host` header is not loopback (DNS-rebinding protection from rmcp). Relay mode is unaffected.

## v0.2.0 - 2026-06-21

### Added
- MCP server with 10 tools: store, search, get, list, discover, update, archive, merge, link, unlink
- Hybrid search: vector similarity (sqlite-vec) + full-text search (FTS5) with RRF merge
- Local embeddings via fastembed (Nomic Embed Text v1.5, 13 models supported)
- JSONL sync: export/import with gzip, conflict resolution, tombstones
- Background sync in serve mode: periodic export, filesystem watching, polling fallback
- Multi-machine sync via shared filesystem (e.g., Syncthing)
- CLI commands: serve, export, import, sync, reembed, status, models
- Configuration via TOML with env var overrides (ERINRA_* prefix)
- Graceful shutdown with SIGINT/SIGTERM handling and optional export_on_exit

### Changed
- Daemon coordination (`web.state`) now uses a typed `Claiming`/`Ready` format: the daemon PID and auth token can no longer be observed half-initialized, eliminating a startup race. A legacy or unparseable state file is reset automatically on upgrade.
- Relay mode no longer silently falls back to standalone after a mid-session failure; it now exits with a clear error so the MCP client can reconnect (a pre-handshake connection failure still falls back to standalone safely).

### Internal
- The vector index (`memory_embeddings`) is now derived from the `memories.embedding` column via SQL triggers (schema migration v3), keeping the stored embedding and the vector index in sync automatically instead of by hand at every write site.
- Startup-mode resolution (relay vs. standalone vs. daemon) refactored into a pure, fully-tested decision function with filesystem/process/relay effects behind injectable ports; daemon PID and auth token are modeled as newtypes that cannot hold sentinel values.
