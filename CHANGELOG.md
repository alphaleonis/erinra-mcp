# Changelog

All notable changes to this project will be documented in this file.

## Unreleased

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
