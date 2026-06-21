# Changelog

All notable changes to this project will be documented in this file.

## Unreleased

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
