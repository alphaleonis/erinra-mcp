//! CLI subcommand implementations.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use erinra::embedding::{Embedder, Reranker};
use erinra::service::{MemoryService, ServiceConfig};
use erinra::{config, db, embedding, mcp, relay, sync, web};

/// Load the reranker model if enabled in config. Returns `None` when disabled.
async fn load_reranker(
    config: &config::RerankerConfig,
    data_dir: &Path,
) -> Result<Option<Arc<dyn Reranker>>> {
    if !config.enabled {
        tracing::debug!("reranker disabled");
        return Ok(None);
    }
    tracing::info!("loading reranker model...");
    let rm = config.model.clone();
    let cache_dir = data_dir.join("models");
    let reranker =
        tokio::task::spawn_blocking(move || embedding::FastembedReranker::new(&rm, cache_dir))
            .await?
            .context("failed to initialize reranker model")?;
    tracing::info!("reranker model loaded");
    Ok(Some(Arc::new(reranker)))
}

/// Run the `serve` subcommand.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    data_dir: &Path,
    log_level: Option<String>,
    log_file: Option<PathBuf>,
    busy_timeout: Option<u32>,
    embedding_model: Option<String>,
    reranker_model: Option<String>,
    web: bool,
    port: Option<u16>,
    bind: Option<String>,
) -> Result<()> {
    // Ensure the data directory exists with restricted permissions.
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create data directory: {}", data_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to set permissions on {}", data_dir.display()))?;
    }

    // Load config before tracing init so log_level and log_file take effect.
    let cli_overrides = config::CliOverrides::new(
        log_level,
        log_file,
        busy_timeout,
        embedding_model,
        reranker_model,
    );
    let config = config::Config::load(data_dir, Some(&cli_overrides))
        .context("failed to load configuration")?;

    init_tracing(&config.logging)?;
    tracing::debug!(?config, "loaded configuration");

    // Try relay mode: if a daemon is running, bridge stdio to its /mcp endpoint
    // instead of loading models locally. This skips the ~137 MB model download.
    if let Ok(Some(daemon_state)) = web::daemon::read_state(data_dir)
        && web::daemon::is_pid_alive(daemon_state.daemon_pid)
    {
        tracing::info!(
            port = daemon_state.port,
            daemon_pid = daemon_state.daemon_pid,
            "daemon detected, attempting relay mode"
        );
        let base_url = format!("http://127.0.0.1:{}", daemon_state.port);
        let stdin = tokio::io::BufReader::new(tokio::io::stdin());
        let stdout = tokio::io::stdout();
        match relay::run_relay(stdin, stdout, &base_url, &daemon_state.auth_token).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                // Fallback only works if relay failed immediately (e.g., connection refused).
                // If it failed mid-session, the MCP client has already started communicating
                // and won't re-initialize with a new standalone server.
                tracing::warn!("relay mode failed, falling back to standalone: {e:#}");
            }
        }
    }

    tracing::info!("loading embedding model...");
    let model_cache_dir = data_dir.join("models");
    std::fs::create_dir_all(&model_cache_dir)?;
    let em = config.embedding.model.clone();
    let embedder = tokio::task::spawn_blocking(move || {
        embedding::FastembedEmbedder::new(&em, model_cache_dir)
    })
    .await?
    .context("failed to initialize embedding model")?;
    tracing::info!("embedding model loaded");

    let db_path = data_dir.join("db.sqlite");
    let db_config = db::DbConfig {
        busy_timeout_ms: config.database.busy_timeout,
        embedding_dimensions: embedder.dimensions(),
        embedding_model: config.embedding.model.clone(),
        max_content_size: config.store.max_content_size,
    };
    let db = db::Database::open(&db_path, &db_config)
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;

    let db = Arc::new(Mutex::new(db));
    let embedder: Arc<dyn Embedder> = Arc::new(embedder);

    let reranker = load_reranker(&config.reranker, data_dir).await?;

    // restore_on_start: import peer exports before MCP server starts
    if config.sync.enabled && config.sync.restore_on_start {
        if config.sync.sync_dir.exists() {
            let filename = sync::background::resolve_filename(&config.sync.filename);
            let own_filename = format!("{}.{}", filename, config.sync.format);
            let db_lock = db.lock().expect("db mutex poisoned");
            match sync::background::restore_from_peers(
                &db_lock,
                embedder.as_ref(),
                &config.sync.sync_dir,
                &own_filename,
            ) {
                Ok(stats) => {
                    tracing::info!(
                        inserted = stats.memories_inserted,
                        updated = stats.memories_updated,
                        "restore_on_start complete"
                    );
                }
                Err(e) => {
                    tracing::warn!("restore_on_start failed: {e:#}");
                }
            }
            drop(db_lock);
        } else {
            tracing::debug!(
                sync_dir = %config.sync.sync_dir.display(),
                "restore_on_start skipped: sync directory does not exist yet"
            );
        }
    }

    // Start background sync (only if sync is enabled)
    let sync_handle = if config.sync.enabled {
        Some(
            sync::background::SyncHandle::start(db.clone(), embedder.clone(), config.sync.clone())
                .await
                .context("failed to start background sync")?,
        )
    } else {
        tracing::debug!("background sync disabled");
        None
    };

    let service = MemoryService::new(db, embedder, reranker, ServiceConfig::from(&config));

    // Optionally start the web dashboard daemon.
    let mut daemon_started = false;
    if web {
        let web_port = port.unwrap_or(config.web.port);
        let web_bind = bind.unwrap_or_else(|| config.web.bind.clone());
        match web::daemon::ensure_daemon(data_dir, web_port, &web_bind) {
            Ok(action) => {
                tracing::info!(?action, "web dashboard daemon ready");
                daemon_started = true;
            }
            Err(e) => {
                tracing::warn!("failed to start web dashboard daemon: {e:#}");
                // Non-fatal: MCP server continues without web dashboard
            }
        }
    }

    // Run MCP server with signal handling for graceful shutdown
    let mcp_future = mcp::serve(service);

    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

        tokio::select! {
            result = mcp_future => { result?; }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received SIGINT, shutting down");
            }
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM, shutting down");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::select! {
            result = mcp_future => { result?; }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received SIGINT, shutting down");
            }
        }
    }

    // Deregister from web dashboard daemon (only if we successfully registered).
    if daemon_started {
        let _ = web::daemon::deregister_client(data_dir, std::process::id());
    }

    // Graceful shutdown: stop background sync
    if let Some(handle) = sync_handle {
        handle.shutdown().await.context("sync shutdown failed")?;
    }

    Ok(())
}

/// Run the `export` subcommand.
pub fn export(
    data_dir: &Path,
    config: &config::Config,
    output: &Path,
    gzip: bool,
    since: Option<String>,
) -> Result<()> {
    tracing::info!(?output, gzip, ?since, "exporting memories");

    let db_path = data_dir.join("db.sqlite");
    if !db_path.exists() {
        anyhow::bail!(
            "no database found at {}. Run `erinra serve` first.",
            db_path.display()
        );
    }

    let db_config = db::DbConfig {
        busy_timeout_ms: config.database.busy_timeout,
        ..db::DbConfig::default()
    };
    let db = db::Database::open_unverified(&db_path, &db_config)
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;

    let opts = sync::ExportOptions {
        since,
        tombstone_retention_days: config.sync.tombstone_retention_days,
        purge: false, // standalone export should not mutate DB
    };

    // Atomic write: write to temp file, then rename.
    let parent = output.parent().unwrap_or(Path::new("."));
    let tmp_path = parent.join(format!(".erinra-export-{}.tmp", std::process::id()));

    let result = (|| -> Result<usize> {
        let file = std::fs::File::create(&tmp_path)
            .with_context(|| format!("failed to create temp file: {}", tmp_path.display()))?;

        let count = if gzip {
            let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let count = sync::export(&db, &mut gz, &opts)?;
            gz.finish().context("failed to finalize gzip")?;
            count
        } else {
            let mut writer = std::io::BufWriter::new(file);
            let count = sync::export(&db, &mut writer, &opts)?;
            std::io::Write::flush(&mut writer)?;
            count
        };

        Ok(count)
    })();

    match result {
        Ok(count) => {
            std::fs::rename(&tmp_path, output).with_context(|| {
                format!(
                    "failed to rename {} to {}",
                    tmp_path.display(),
                    output.display()
                )
            })?;
            eprintln!("Exported {count} records to {}", output.display());
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
    }

    Ok(())
}

/// Run the `import` subcommand.
pub async fn import(data_dir: &Path, config: &config::Config, input: &Path) -> Result<()> {
    tracing::info!(?input, "importing memories");

    let db_path = data_dir.join("db.sqlite");
    if !db_path.exists() {
        anyhow::bail!(
            "no database found at {}. Run `erinra serve` first.",
            db_path.display()
        );
    }

    // Load embedder for re-embedding imported memories.
    eprintln!("Loading embedding model...");
    let model_name = config.embedding.model.clone();
    let model_cache_dir = data_dir.join("models");
    std::fs::create_dir_all(&model_cache_dir)?;
    let mn = model_name.clone();
    let embedder = tokio::task::spawn_blocking(move || {
        embedding::FastembedEmbedder::new(&mn, model_cache_dir)
    })
    .await?
    .context("failed to initialize embedding model")?;

    let db_config = db::DbConfig {
        busy_timeout_ms: config.database.busy_timeout,
        embedding_dimensions: embedder.dimensions(),
        embedding_model: model_name,
        max_content_size: config.store.max_content_size,
    };
    let db = db::Database::open(&db_path, &db_config)
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;

    let file = std::fs::File::open(input)
        .with_context(|| format!("failed to open {}", input.display()))?;

    // Auto-detect gzip by checking the magic number (0x1f 0x8b).
    let mut buf_reader = std::io::BufReader::new(file);
    let is_gzip = {
        use std::io::{Read, Seek, SeekFrom};
        let mut magic = [0u8; 2];
        let n = buf_reader.read(&mut magic)?;
        buf_reader.seek(SeekFrom::Start(0))?;
        n == 2 && magic[0] == 0x1f && magic[1] == 0x8b
    };

    let embed_batch = |texts: &[&str]| embedder.embed_documents(texts);

    let stats = if is_gzip {
        let mut gz_reader = flate2::read::GzDecoder::new(buf_reader);
        sync::import(&db, embed_batch, &mut gz_reader)?
    } else {
        sync::import(&db, embed_batch, &mut buf_reader)?
    };

    eprintln!(
        "Import complete: {} inserted, {} updated, {} skipped (memories); \
         {} inserted, {} skipped (links); {} applied, {} skipped (tombstones)",
        stats.memories_inserted,
        stats.memories_updated,
        stats.memories_skipped,
        stats.links_inserted,
        stats.links_skipped,
        stats.tombstones_applied,
        stats.tombstones_skipped,
    );

    Ok(())
}

/// Run the `sync` subcommand.
pub async fn run_sync(data_dir: &Path, config: &config::Config, force: bool) -> Result<()> {
    if !config.sync.enabled && !force {
        anyhow::bail!(
            "sync is not enabled. Set [sync] enabled = true in config.toml, \
             or use --force to run anyway."
        );
    }
    tracing::info!("running sync cycle");

    let db_path = data_dir.join("db.sqlite");
    if !db_path.exists() {
        anyhow::bail!(
            "no database found at {}. Run `erinra serve` first.",
            db_path.display()
        );
    }

    // Load embedder for re-embedding imported memories.
    eprintln!("Loading embedding model...");
    let model_name = config.embedding.model.clone();
    let model_cache_dir = data_dir.join("models");
    std::fs::create_dir_all(&model_cache_dir)?;
    let mn = model_name.clone();
    let embedder = tokio::task::spawn_blocking(move || {
        embedding::FastembedEmbedder::new(&mn, model_cache_dir)
    })
    .await?
    .context("failed to initialize embedding model")?;

    let db_config = db::DbConfig {
        busy_timeout_ms: config.database.busy_timeout,
        embedding_dimensions: embedder.dimensions(),
        embedding_model: model_name,
        max_content_size: config.store.max_content_size,
    };
    let db = db::Database::open(&db_path, &db_config)
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;

    // Ensure sync directory exists.
    std::fs::create_dir_all(&config.sync.sync_dir).with_context(|| {
        format!(
            "failed to create sync directory: {}",
            config.sync.sync_dir.display()
        )
    })?;

    // Export local memories.
    let filename = sync::background::resolve_filename(&config.sync.filename);
    let export_filename = format!("{}.{}", filename, config.sync.format);
    let export_path = config.sync.sync_dir.join(&export_filename);

    let opts = sync::ExportOptions {
        since: None,
        tombstone_retention_days: config.sync.tombstone_retention_days,
        purge: true,
    };

    // Serialize to bytes, optionally gzip, then write only if content changed.
    let raw_bytes = sync::background::export_to_bytes(&db, &opts)?;
    let export_bytes = if config.sync.format.to_string().ends_with(".gz") {
        use flate2::write::GzEncoder;
        let mut gz = GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, &raw_bytes)?;
        gz.finish().context("failed to finalize gzip")?
    } else {
        raw_bytes
    };
    let changed = sync::background::write_atomic_if_changed(&export_path, &export_bytes)?;
    if changed {
        eprintln!("Exported to {}", export_path.display());
    } else {
        eprintln!(
            "Export unchanged, skipping write to {}",
            export_path.display()
        );
    }

    // Import peer files.
    let import_stats = sync::background::restore_from_peers(
        &db,
        &embedder,
        &config.sync.sync_dir,
        &export_filename,
    )?;
    eprintln!(
        "Import complete: {} inserted, {} updated, {} skipped (memories); \
         {} inserted, {} skipped (links); {} applied, {} skipped (tombstones)",
        import_stats.memories_inserted,
        import_stats.memories_updated,
        import_stats.memories_skipped,
        import_stats.links_inserted,
        import_stats.links_skipped,
        import_stats.tombstones_applied,
        import_stats.tombstones_skipped,
    );

    Ok(())
}

/// Run the `reembed` subcommand.
pub async fn reembed(
    data_dir: &Path,
    config: &config::Config,
    model: Option<String>,
) -> Result<()> {
    let db_path = data_dir.join("db.sqlite");
    if !db_path.exists() {
        anyhow::bail!(
            "no database found at {}. Run `erinra serve` first.",
            db_path.display()
        );
    }

    // Determine model: CLI flag > config > default.
    let model_name = model.unwrap_or(config.embedding.model.clone());

    // Load the embedder.
    eprintln!("Loading embedding model '{model_name}'...");
    let model_cache_dir = data_dir.join("models");
    std::fs::create_dir_all(&model_cache_dir)?;
    let mn = model_name.clone();
    let embedder = tokio::task::spawn_blocking(move || {
        embedding::FastembedEmbedder::new(&mn, model_cache_dir)
    })
    .await?
    .context("failed to initialize embedding model")?;
    let dims = embedder.dimensions();

    // Open DB without embedding verification (we're changing it).
    let db_config = db::DbConfig {
        busy_timeout_ms: config.database.busy_timeout,
        max_content_size: config.store.max_content_size,
        ..db::DbConfig::default()
    };
    let db = db::Database::open_unverified(&db_path, &db_config)
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;

    // If dimensions changed, recreate the vec0 table.
    let old_dims: u32 = db
        .get_metadata("embedding_dimensions")?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if old_dims != dims {
        eprintln!("Dimension change ({old_dims} -> {dims}), recreating vector index...");
        db.recreate_vec_table(dims)?;
    }

    let total = db.count_active_memories()?;
    eprintln!("Re-embedding {total} memories with '{model_name}' ({dims} dims)...");

    const BATCH_SIZE: u32 = 100;
    let mut processed: u64 = 0;
    let mut offset: u32 = 0;

    loop {
        let batch = db.fetch_memory_batch(BATCH_SIZE, offset)?;
        if batch.is_empty() {
            break;
        }

        let texts: Vec<&str> = batch.iter().map(|(_, c)| c.as_str()).collect();
        let embeddings = embedder
            .embed_documents(&texts)
            .context("embedding failed")?;

        for ((id, _), embedding) in batch.iter().zip(embeddings.iter()) {
            db.update_embedding(id, embedding)?;
        }

        processed += batch.len() as u64;
        eprint!("\r  {processed}/{total} memories re-embedded");

        if (batch.len() as u32) < BATCH_SIZE {
            break;
        }
        offset += BATCH_SIZE;
    }
    eprintln!();

    // Update metadata to reflect the new model.
    db.set_metadata("embedding_model", &model_name)?;
    db.set_metadata("embedding_dimensions", &dims.to_string())?;

    eprintln!("Done. {processed} memories re-embedded.");
    Ok(())
}

/// Run the `status` subcommand.
pub fn status(data_dir: &Path, config: &config::Config) -> Result<()> {
    let db_path = data_dir.join("db.sqlite");
    if !db_path.exists() {
        println!("No database found at {}", db_path.display());
        println!("Run `erinra serve` to create the database.");
        return Ok(());
    }

    let db_config = db::DbConfig {
        busy_timeout_ms: config.database.busy_timeout,
        ..db::DbConfig::default()
    };
    let db = db::Database::open_unverified(&db_path, &db_config)
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;
    let info = db.status()?;

    let file_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    println!("Erinra Status");
    println!("─────────────────────────────");
    println!("Active memories:    {}", info.stats.total_memories);
    println!("Archived memories:  {}", info.stats.total_archived);
    println!("Total links:        {}", info.total_links);
    println!("─────────────────────────────");
    println!("Embedding model:    {}", info.stats.embedding_model);
    println!("Embedding dims:     {}", info.embedding_dimensions);
    println!("Schema version:     {}", info.schema_version);
    println!("Database size:      {}", format_bytes(file_size));
    println!("Database path:      {}", db_path.display());

    Ok(())
}

/// List available embedding and reranker models.
pub fn licenses() -> Result<()> {
    print!("{}", include_str!("../THIRD_PARTY_LICENSES.txt"));
    Ok(())
}

pub fn models() -> Result<()> {
    let output = format_models_listing();
    print!("{output}");
    Ok(())
}

/// Build the formatted models listing string (testable without stdout capture).
fn format_models_listing() -> String {
    use std::fmt::Write;
    let mut out = String::new();

    writeln!(out, "Available embedding models:").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "  {:<24} {:>4}  Description", "Name", "Dims").unwrap();
    writeln!(
        out,
        "  {:<24} {:>4}  ───────────────────────────────────",
        "────────────────────────", "────"
    )
    .unwrap();
    for &(name, dims, desc) in embedding::SUPPORTED_MODELS {
        writeln!(out, "  {:<24} {:>4}  {}", name, dims, desc).unwrap();
    }
    writeln!(out).unwrap();
    writeln!(out, "Default: NomicEmbedTextV15Q").unwrap();
    writeln!(
        out,
        "Set via config.toml [embedding] model, --embedding-model flag, or ERINRA_EMBEDDING_MODEL env var."
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(out, "Available reranker models:").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "  {:<32} Description", "Name").unwrap();
    writeln!(
        out,
        "  {:<32} ───────────────────────────────────────────",
        "────────────────────────────────"
    )
    .unwrap();
    for &(name, desc) in embedding::SUPPORTED_RERANKER_MODELS {
        writeln!(out, "  {:<32} {}", name, desc).unwrap();
    }
    writeln!(out).unwrap();
    writeln!(out, "Default: JINARerankerV1TurboEn (disabled by default)").unwrap();
    writeln!(
        out,
        "Enable via config.toml [reranker] enabled = true, or ERINRA_RERANKER_ENABLED=true env var."
    )
    .unwrap();

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_listing_includes_embedding_and_reranker() {
        let output = format_models_listing();
        // Should contain embedding section.
        assert!(
            output.contains("Available embedding models"),
            "should contain embedding models heading"
        );
        assert!(
            output.contains("NomicEmbedTextV15Q"),
            "should list default embedding model"
        );
        // Should contain reranker section.
        assert!(
            output.contains("Available reranker models"),
            "should contain reranker models heading"
        );
        assert!(
            output.contains("JINARerankerV1TurboEn"),
            "should list default reranker model"
        );
        assert!(
            output.contains("BGERerankerBase"),
            "should list BGERerankerBase"
        );
    }
}

/// Format a byte count as a human-readable string.
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    match bytes {
        b if b >= GIB => format!("{:.1} GiB", b as f64 / GIB as f64),
        b if b >= MIB => format!("{:.1} MiB", b as f64 / MIB as f64),
        b if b >= KIB => format!("{:.1} KiB", b as f64 / KIB as f64),
        b => format!("{b} B"),
    }
}

/// Run the `dash` subcommand: start the web dashboard via daemon.
pub async fn dash(
    data_dir: &Path,
    config: &config::Config,
    port: Option<u16>,
    bind: Option<String>,
    no_open: bool,
) -> Result<()> {
    let db_path = data_dir.join("db.sqlite");
    if !db_path.exists() {
        anyhow::bail!(
            "no database found at {}. Run `erinra serve` first.",
            db_path.display()
        );
    }

    let port = port.unwrap_or(config.web.port);
    let bind = bind.unwrap_or_else(|| config.web.bind.clone());

    let action = web::daemon::ensure_daemon(data_dir, port, &bind)?;
    let daemon_port = match &action {
        web::daemon::DaemonAction::Spawned { port } => *port,
        web::daemon::DaemonAction::Joined { port } => *port,
    };

    // Read the auth token from the daemon state file so we can pass it to the browser.
    let auth_token = web::daemon::read_state(data_dir)?
        .map(|s| s.auth_token)
        .unwrap_or_default();

    let url = if auth_token.is_empty() {
        format!("http://{bind}:{daemon_port}")
    } else {
        format!("http://{bind}:{daemon_port}?token={auth_token}")
    };

    eprintln!("Erinra dashboard: {url}");

    if !no_open && let Err(e) = open::that(&url) {
        tracing::warn!("failed to open browser: {e}");
    }

    // Block until Ctrl-C or SIGTERM.
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
    }

    eprintln!("\nShutting down...");
    let _ = web::daemon::deregister_client(data_dir, std::process::id());
    Ok(())
}

/// Run the web dashboard as a daemon process.
/// Writes state file on startup, periodically sweeps dead clients,
/// shuts down (with grace period) when client list empties.
pub async fn run_daemon(
    data_dir: &Path,
    config: &config::Config,
    port: u16,
    bind: &str,
) -> Result<()> {
    let db_path = data_dir.join("db.sqlite");
    if !db_path.exists() {
        anyhow::bail!(
            "no database found at {}. Run `erinra serve` first.",
            db_path.display()
        );
    }

    // Generate auth token and write state file early, before loading models.
    // This avoids a race condition where `dash` reads the state file before
    // the daemon finishes loading models and would get an empty token.
    let auth_token = web::auth::generate_auth_token();
    let our_pid = std::process::id();
    web::daemon::update_state(data_dir, |existing| {
        Some(web::daemon::DaemonState {
            daemon_pid: our_pid,
            port,
            clients: existing.map(|s| s.clients).unwrap_or_default(),
            auth_token: auth_token.clone(),
        })
    })?;

    // Load embedding model for search support.
    let model_cache_dir = data_dir.join("models");
    std::fs::create_dir_all(&model_cache_dir)?;
    let em = config.embedding.model.clone();
    let embedder = tokio::task::spawn_blocking(move || {
        embedding::FastembedEmbedder::new(&em, model_cache_dir)
    })
    .await?
    .context("failed to initialize embedding model")?;

    let db_config = db::DbConfig {
        busy_timeout_ms: config.database.busy_timeout,
        embedding_dimensions: embedder.dimensions(),
        embedding_model: config.embedding.model.clone(),
        max_content_size: config.store.max_content_size,
    };
    let db = db::Database::open(&db_path, &db_config)
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;

    let embedder: Arc<dyn Embedder> = Arc::new(embedder);

    let reranker = load_reranker(&config.reranker, data_dir).await?;

    let addr: std::net::SocketAddr = format!("{bind}:{port}")
        .parse()
        .with_context(|| format!("invalid bind address: {bind}:{port}"))?;

    let service = MemoryService::new(
        Arc::new(Mutex::new(db)),
        embedder,
        reranker,
        ServiceConfig::from(config),
    );

    let opts = web::ServeOptions {
        open_browser: false,
    };
    let server = web::serve(service, auth_token, addr, opts);

    let data_dir_owned = data_dir.to_path_buf();

    // Background task: periodically sweep dead clients and check for shutdown.
    let sweep_handle = tokio::spawn(async move {
        let mut grace_start: Option<std::time::Instant> = None;
        let grace_period = std::time::Duration::from_secs(60);

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;

            match web::daemon::cleanup_stale_state(&data_dir_owned) {
                Ok(Some(state)) => {
                    if web::daemon::should_shutdown(&state, &mut grace_start, grace_period) {
                        tracing::info!("no clients remaining after grace period, shutting down");
                        break;
                    }
                }
                Ok(None) => {
                    // State file gone (someone else cleaned up). Shut down.
                    tracing::info!("daemon state file removed, shutting down");
                    break;
                }
                Err(e) => {
                    tracing::warn!("failed to sweep daemon state: {e:#}");
                }
            }
        }
    });

    // Run server until SIGTERM or sweep decides to stop.
    // The daemon ignores SIGINT (Ctrl-C) — it's detached from the terminal's
    // process group, so Ctrl-C only affects the foreground dash/serve process.
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = server => { result?; }
            _ = sigterm.recv() => {
                tracing::info!("daemon received SIGTERM, shutting down");
            }
            _ = sweep_handle => {
                tracing::info!("daemon sweep triggered shutdown");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::select! {
            result = server => { result?; }
            _ = sweep_handle => {
                tracing::info!("daemon sweep triggered shutdown");
            }
        }
    }

    // Clean up state file on exit.
    let _ = web::daemon::update_state(data_dir, |_| None);

    Ok(())
}

/// Initialize tracing subscriber with stderr output and optional file logging.
pub fn init_tracing(config: &config::LoggingConfig) -> Result<()> {
    use tracing_subscriber::prelude::*;

    let env_filter = tracing_subscriber::EnvFilter::try_new(&config.log_level)
        .context("invalid log_level filter directive")?;

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer);

    if let Some(log_file) = &config.log_file {
        let filename = log_file.file_name().ok_or_else(|| {
            anyhow::anyhow!("log_file path has no filename: {}", log_file.display())
        })?;
        let parent = log_file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed to create log file directory: {}", parent.display())
        })?;
        let file_appender = tracing_appender::rolling::never(parent, filename);
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(file_appender)
            .with_ansi(false);
        registry.with(file_layer).init();
    } else {
        registry.init();
    }

    Ok(())
}
