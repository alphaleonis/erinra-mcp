# Erinra

[![CI](https://github.com/alphaleonis/erinra-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/alphaleonis/erinra-mcp/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/erinra)](https://crates.io/crates/erinra)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A memory MCP server for LLM coding assistants. Single Rust binary, local SQLite storage, local embeddings, zero external dependencies.

Erinra stores, indexes, and retrieves memories via the [Model Context Protocol](https://modelcontextprotocol.io/) (MCP) over stdio transport. The server handles storage and retrieval; the calling LLM makes all semantic decisions (deduplication, merging, corrections).

## Features

- **Hybrid search** -- vector similarity (sqlite-vec) + full-text search (FTS5), merged via Reciprocal Rank Fusion
- **Cross-encoder reranking** *(experimental)* -- optional second-stage reranking with JINA Reranker for improved search precision
- **Local embeddings** -- fastembed with Nomic Embed Text v1.5, no external API calls
- **Non-destructive** -- `store` always creates, `archive` instead of delete, `update` is the only destructive operation
- **Fat responses** -- `store` returns top-3 similar memories so the LLM can react without follow-up calls
- **Web dashboard** -- browser-based UI for browsing, searching, and inspecting memories
- **Shared daemon** -- multiple MCP sessions share one embedding model and database via a background daemon, saving ~288 MB per additional session
- **Multi-machine sync** -- JSONL export/import with background sync via filesystem watching or polling
- **Single file** -- all data in one SQLite database with WAL mode for concurrent access

## Quick Start

### Install from crates.io

```bash
cargo install erinra
```

### Install from GitHub

```bash
cargo install --git https://github.com/alphaleonis/erinra-mcp.git
```

### Or build from source

Requires [Node.js](https://nodejs.org/) (for building the web dashboard frontend).

```bash
git clone https://github.com/alphaleonis/erinra-mcp.git
cd erinra-mcp
cargo build --release
```

### Add to Claude Code

```bash
claude mcp add erinra -s user -- erinra serve
```

The first run downloads the Nomic embedding model (~137 MB) to `~/.erinra/models/`. Subsequent starts are fast.

### Verify it works

```bash
erinra status
```

### Open the web dashboard

```bash
erinra dash
```

## MCP Tools

| Tool | Description |
|------|-------------|
| **store** | Save a memory. Returns similar existing memories for dedup detection. |
| **search** | Find memories by semantic similarity and keyword matching. |
| **get** | Fetch full details of specific memories by ID. |
| **list** | Browse and filter memories without a search query. |
| **discover** | View taxonomy (projects, types, tags) with usage counts and stats. |
| **context** | Batched session-start search: run multiple queries in one call with a content budget. |
| **update** | Modify content or metadata. Re-embeds automatically on content change. |
| **archive** | Soft-delete a memory. Excluded from search/list by default. |
| **merge** | Combine multiple memories into one. Archives sources with `supersedes` links. |
| **link** | Create a directed relationship between memories. |
| **unlink** | Remove a relationship. |

## CLI Commands

### Global options

```
erinra [OPTIONS] <COMMAND>

Options:
    --data-dir <PATH>    Data directory [default: ~/.erinra] [env: ERINRA_DATA_DIR]
    -h, --help           Print help
    -V, --version        Print version
```

### `serve` -- Start MCP server

```
erinra serve [OPTIONS]
```

Starts the MCP server on stdio transport. When a web dashboard daemon is running, `serve` automatically uses **relay mode** -- bridging stdio to the daemon's HTTP endpoint instead of loading models locally. This saves ~288 MB of memory per additional session. Falls back to standalone mode if the daemon is unavailable.

| Option | Description |
|--------|-------------|
| `--log-level <LEVEL>` | Override log level (e.g. `debug`, `info`, `erinra=debug`) |
| `--log-file <PATH>` | Override log file path |
| `--busy-timeout <MS>` | Override SQLite busy timeout in milliseconds |
| `--embedding-model <NAME>` | Override embedding model name |
| `--reranker-model <NAME>` | Override reranker model name (also enables reranking) |
| `--web` | Also start the web dashboard (background daemon) |
| `--port <PORT>` | Override web server port (requires `--web`) |
| `--bind <ADDR>` | Override web server bind address (requires `--web`) |

### `dash` -- Open web dashboard

```
erinra dash [OPTIONS]
```

Starts (or joins) the background daemon and opens the dashboard in a browser. The daemon process is ref-counted -- it stays alive while any `serve --web` or `dash` process is running, and shuts down automatically when all clients exit.

| Option | Description |
|--------|-------------|
| `--port <PORT>` | Override web server port |
| `--bind <ADDR>` | Override web server bind address |
| `--no-open` | Don't open the browser automatically |
| `--open-only` | Open the browser and exit immediately (requires a running daemon) |

### `export` -- Export memories

```
erinra export <OUTPUT> [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `--gzip` | Compress output with gzip |
| `--since <TIMESTAMP>` | Only export memories created/updated after this timestamp |

### `import` -- Import memories

```
erinra import <INPUT>
```

Imports from JSONL file. Auto-detects gzip compression.

### `sync` -- Run sync cycle

```
erinra sync [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `--force` | Run even if sync is not enabled in config |

### `reembed` -- Regenerate embeddings

```
erinra reembed [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `--model <NAME>` | Override the embedding model |

### `status` -- Print database stats

```
erinra status
```

### `models` -- List available models

```
erinra models
```

## Configuration

Erinra reads configuration from `~/.erinra/config.toml`. All settings have sensible defaults.

**Precedence**: CLI args > environment variables > config file > defaults.

### Embedding

```toml
[embedding]
model = "NomicEmbedTextV15Q"        # Embedding model name (run `erinra models` to list)
```

| Env var | Config key |
|---------|------------|
| `ERINRA_EMBEDDING_MODEL` | `embedding.model` |

### Reranker *(experimental)*

> **Note:** Reranking is experimental and subject to change. Short or single-keyword queries may receive unexpectedly low scores from the cross-encoder, causing results to be filtered out. Consider setting a low threshold (e.g. `-5`) or disabling threshold filtering if you encounter missing results.

Optional cross-encoder reranking for improved search precision. Downloads the reranker model (~151 MB) on first use.

```toml
[reranker]
enabled = false                      # Enable cross-encoder reranking
model = "JINARerankerV1TurboEn"      # Reranker model name
threshold = 0.0                      # Minimum score to include (can be negative)
```

| Env var | Config key |
|---------|------------|
| `ERINRA_RERANKER_ENABLED` | `reranker.enabled` |
| `ERINRA_RERANKER_MODEL` | `reranker.model` |
| `ERINRA_RERANKER_THRESHOLD` | `reranker.threshold` |

### Store

```toml
[store]
similar_limit = 3                    # Similar memories returned by store/merge
similar_threshold = 0.5              # Minimum cosine similarity for similar results (0.0-1.0)
content_max_length = 500             # Content truncation for search/list responses (chars)
max_content_size = 10240             # Maximum content size on store/update (bytes)
```

| Env var | Config key |
|---------|------------|
| `ERINRA_STORE_SIMILAR_LIMIT` | `store.similar_limit` |
| `ERINRA_STORE_SIMILAR_THRESHOLD` | `store.similar_threshold` |
| `ERINRA_STORE_CONTENT_MAX_LENGTH` | `store.content_max_length` |
| `ERINRA_STORE_MAX_CONTENT_SIZE` | `store.max_content_size` |

### Search

```toml
[search]
rrf_k = 60                          # Reciprocal Rank Fusion constant
```

| Env var | Config key |
|---------|------------|
| `ERINRA_SEARCH_RRF_K` | `search.rrf_k` |

### Database

```toml
[database]
busy_timeout = 5000                  # SQLite busy timeout (ms)
```

| Env var | Config key |
|---------|------------|
| `ERINRA_DATABASE_BUSY_TIMEOUT` | `database.busy_timeout` |

### Logging

```toml
[logging]
log_level = "info"                   # Tracing filter directive
# log_file = "/path/to/erinra.log"  # Optional file logging (must be absolute path)
```

| Env var | Config key |
|---------|------------|
| `ERINRA_LOG_LEVEL` | `logging.log_level` |
| `ERINRA_LOG_FILE` | `logging.log_file` |

### Web Dashboard

```toml
[web]
port = 9898                          # Dashboard + MCP HTTP port
bind = "127.0.0.1"                   # Bind address
```

| Env var | Config key |
|---------|------------|
| `ERINRA_WEB_PORT` | `web.port` |
| `ERINRA_WEB_BIND` | `web.bind` |

### Sync

```toml
[sync]
enabled = false                      # Enable background sync in serve mode
sync_dir = "~/.erinra/sync"         # Directory for sync exports/imports
filename = "{hostname}"              # Export filename template
format = "jsonl.gz"                  # Export format: jsonl, jsonl.gz, json, json.gz
export_interval = 900                # Seconds between periodic exports
poll_interval = 0                    # 0 = filesystem watching, >0 = polling interval (seconds)
restore_on_start = false             # Import peer exports before MCP server starts
export_on_exit = false               # Run final export on shutdown
tombstone_retention_days = 90        # Purge tombstones older than this during export (days)
```

| Env var | Config key |
|---------|------------|
| `ERINRA_SYNC_ENABLED` | `sync.enabled` |
| `ERINRA_SYNC_DIR` | `sync.sync_dir` |
| `ERINRA_SYNC_FILENAME` | `sync.filename` |
| `ERINRA_SYNC_FORMAT` | `sync.format` |
| `ERINRA_SYNC_EXPORT_INTERVAL` | `sync.export_interval` |
| `ERINRA_SYNC_POLL_INTERVAL` | `sync.poll_interval` |
| `ERINRA_SYNC_RESTORE_ON_START` | `sync.restore_on_start` |
| `ERINRA_SYNC_EXPORT_ON_EXIT` | `sync.export_on_exit` |
| `ERINRA_SYNC_TOMBSTONE_RETENTION_DAYS` | `sync.tombstone_retention_days` |

The `filename` template supports the following placeholders:

| Placeholder | Resolves to | Example |
|-------------|-------------|---------|
| `{hostname}` | Machine hostname | `my-desktop` |
| `{os}` | Operating system (`linux`, `macos`, `windows`) | `linux` |
| `{platform}` | Like `{os}` but detects WSL as `wsl` | `wsl` |
| `{distro}` | Linux distro ID from `/etc/os-release` (falls back to OS) | `fedora` |
| `{user}` | Current username | `alice` |

Placeholders can be combined (e.g. `{hostname}-{os}`). The resolved filename is sanitized to remove characters unsafe for filenames. The format extension (e.g. `.jsonl.gz`) is appended automatically.

## Multi-Machine Sync

Erinra supports syncing memories between machines via a shared folder (e.g., Syncthing).

1. **Enable sync** in `config.toml`:
   ```toml
   [sync]
   enabled = true
   sync_dir = "~/.erinra/sync"
   export_on_exit = true
   restore_on_start = true
   ```

2. **Share the sync directory** between machines using Syncthing, Dropbox, or similar.

3. Each machine exports to a machine-specific file (named by hostname) and imports others' exports. Conflict resolution uses last-write-wins (newer `updated_at` wins, local wins on tie). Tombstones propagate archives and deletes across machines.

## Shared Daemon

When the web dashboard daemon is running, `erinra serve` automatically connects to it via HTTP relay instead of loading its own embedding model and reranker. This saves ~288 MB of memory per additional concurrent session.

The daemon holds the embedding model, reranker, and database connection. MCP `serve` instances become thin stdio-to-HTTP bridges. Claude Code sees no difference.

```bash
# Start the daemon (first session, or standalone)
erinra serve --web

# Subsequent sessions automatically use relay mode
erinra serve          # detects daemon, skips model loading
```

If the daemon is unavailable, `serve` falls back to standalone mode transparently.

## Data Storage

All data is stored in `~/.erinra/db.sqlite`. The schema uses:

- **memories** -- content, type, projects, tags, timestamps, access counts
- **links** -- directed relationships between memories (e.g., `supersedes`, `related_to`, `caused_by`)
- **tombstones** -- tracks archived/deleted entities for sync convergence
- **FTS5 index** -- full-text search on memory content
- **sqlite-vec index** -- vector similarity search on embeddings

Embeddings are generated locally using fastembed (ONNX runtime) and are never sent to external services.

## Architecture

```
erinra/
  src/
    main.rs         # CLI entry point (clap): serve, export, import, sync, reembed, status, dash, models
    config.rs       # TOML config with env var overrides and validation
    relay.rs        # Stdio-to-HTTP relay for shared daemon mode
    mcp/            # MCP server, stdio JSON-RPC, tool handlers (11 tools)
    db/             # SQLite schema, queries, migrations, hybrid search + RRF merge
    embedding/      # fastembed wrapper, embedding + reranker model management
    sync/           # JSONL export/import, background sync (filesystem watching + polling)
    web/            # Axum web server, dashboard SPA, daemon coordination, auth, MCP HTTP endpoint
  web/              # SvelteKit 2 + Svelte 5 + Tailwind CSS 4 frontend SPA
```

## Design Principles

- **No server-side reasoning** -- the LLM decides what to store, merge, or archive. The server never deduplicates, detects contradictions, or merges automatically.
- **Non-destructive** -- `store` always creates a new memory. `archive` soft-deletes. Only `update` overwrites content.
- **Offline-first** -- local embeddings, local storage, no network calls. Sync is opt-in via shared filesystem.
- **Concurrency-safe** -- SQLite WAL mode supports multiple concurrent readers. Background sync and MCP tool handlers share a mutex-protected database connection.

## License

MIT
