//! Background sync: periodic export, filesystem watching/polling for import,
//! restore_on_start, export_on_exit, and graceful shutdown integration.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::config::SyncConfig;
use crate::db::Database;
use crate::embedding::Embedder;
use crate::sync::ExportOptions;

/// Resolve filename template: replaces {hostname}, {os}, {platform}, {distro}, {user}
/// with system values. Sanitizes result for safe filenames.
pub fn resolve_filename(template: &str) -> String {
    let mut result = template.to_string();

    if result.contains("{hostname}") {
        let hostname = gethostname::gethostname().to_string_lossy().to_string();
        result = result.replace("{hostname}", &hostname);
    }

    if result.contains("{os}") {
        result = result.replace("{os}", std::env::consts::OS);
    }

    if result.contains("{platform}") {
        let platform = detect_platform();
        result = result.replace("{platform}", &platform);
    }

    if result.contains("{distro}") {
        let distro = detect_distro();
        result = result.replace("{distro}", &distro);
    }

    if result.contains("{user}") {
        let user = whoami::username().unwrap_or_else(|_| "unknown".into());
        result = result.replace("{user}", &user);
    }

    sanitize_filename(&result)
}

/// Replace characters that are unsafe for filenames across all platforms.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | ':' | '<' | '>' | '"' | '|' | '?' | '*' => '_',
            _ => c,
        })
        .collect()
}

/// Serialize the database to bytes (DB read only, no file I/O).
pub fn export_to_bytes(db: &Database, options: &ExportOptions) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    super::export(db, &mut buf, options)?;
    Ok(buf)
}

/// Write bytes to a file atomically (temp write + rename), but only if content changed.
/// Returns true if the file was actually updated (new or content changed).
/// Cleans up the temp file on failure.
pub fn write_atomic_if_changed(export_path: &Path, new_bytes: &[u8]) -> Result<bool> {
    use anyhow::Context;

    let parent = export_path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create sync directory: {}", parent.display()))?;

    // Compare with existing file to avoid unnecessary writes.
    if export_path.exists()
        && let Ok(existing) = std::fs::read(export_path)
        && existing == new_bytes
    {
        return Ok(false);
    }

    // Write to a temp dotfile in the same directory (same filesystem for atomic rename).
    let tmp_path = parent.join(format!(".erinra-export-{}.tmp", std::process::id()));

    std::fs::write(&tmp_path, new_bytes)
        .with_context(|| format!("failed to write temp export: {}", tmp_path.display()))?;

    let result = std::fs::rename(&tmp_path, export_path);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result.with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp_path.display(),
            export_path.display()
        )
    })?;

    Ok(true)
}

/// Atomic export: serialize DB to bytes, then write atomically if changed.
/// Returns true if the file was actually updated (new or content changed).
#[cfg(test)]
pub fn atomic_export(db: &Database, export_path: &Path, options: &ExportOptions) -> Result<bool> {
    let new_bytes = export_to_bytes(db, options)?;
    write_atomic_if_changed(export_path, &new_bytes)
}

/// List peer export files in sync_dir (excludes own_filename and dotfiles).
pub fn list_peer_files(sync_dir: &Path, own_filename: &str) -> Result<Vec<std::path::PathBuf>> {
    use anyhow::Context;

    let mut peers = Vec::new();
    let entries = std::fs::read_dir(sync_dir)
        .with_context(|| format!("failed to read sync directory: {}", sync_dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Skip directories
        if path.is_dir() {
            continue;
        }

        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        // Skip dotfiles (temp files, hidden files)
        if file_name.starts_with('.') {
            continue;
        }

        // Skip own export file
        if file_name == own_filename {
            continue;
        }

        peers.push(path);
    }

    Ok(peers)
}

/// Import all peer exports (for restore_on_start). Returns combined stats.
pub fn restore_from_peers(
    db: &Database,
    embedder: &dyn Embedder,
    sync_dir: &Path,
    own_filename: &str,
) -> Result<super::ImportStats> {
    use anyhow::Context;

    let peers = list_peer_files(sync_dir, own_filename)?;
    let mut combined = super::ImportStats::default();

    for peer_path in &peers {
        let file = std::fs::File::open(peer_path)
            .with_context(|| format!("failed to open peer export: {}", peer_path.display()))?;

        let stats = import_from_file(db, embedder, file)?;

        combined.memories_inserted += stats.memories_inserted;
        combined.memories_updated += stats.memories_updated;
        combined.memories_skipped += stats.memories_skipped;
        combined.links_inserted += stats.links_inserted;
        combined.links_skipped += stats.links_skipped;
        combined.tombstones_applied += stats.tombstones_applied;
        combined.tombstones_skipped += stats.tombstones_skipped;
    }

    Ok(combined)
}

/// Handle for the background sync task. Drop triggers shutdown.
pub struct SyncHandle {
    shutdown_tx: broadcast::Sender<()>,
    export_handle: JoinHandle<()>,
    import_handle: Option<JoinHandle<()>>,
    config: SyncConfig,
    db: Arc<Mutex<Database>>,
}

impl SyncHandle {
    /// Start background sync (periodic export + watcher/poller for import).
    /// First export fires immediately.
    pub async fn start(
        db: Arc<Mutex<Database>>,
        embedder: Arc<dyn Embedder>,
        config: SyncConfig,
    ) -> Result<Self> {
        let (shutdown_tx, _) = broadcast::channel(1);

        let filename = resolve_filename(&config.filename);
        let export_filename = format!("{}.{}", filename, config.format);
        let sync_dir = config.sync_dir.clone();
        let export_path = sync_dir.join(&export_filename);
        let tombstone_retention_days = config.tombstone_retention_days;

        // -- Export task --
        let export_db = db.clone();
        let export_interval = config.export_interval;
        let export_path_clone = export_path.clone();
        let mut export_shutdown_rx = shutdown_tx.subscribe();

        let export_handle = tokio::spawn(async move {
            let opts = ExportOptions {
                since: None,
                tombstone_retention_days,
                purge: true,
            };

            // Immediate first export: serialize under lock, write outside lock
            {
                let bytes_result = {
                    let db = export_db.lock().expect("db mutex poisoned");
                    export_to_bytes(&db, &opts)
                };
                match bytes_result {
                    Ok(bytes) => {
                        if let Err(e) = write_atomic_if_changed(&export_path_clone, &bytes) {
                            tracing::error!("sync export failed: {e:#}");
                        }
                    }
                    Err(e) => tracing::error!("sync export failed: {e:#}"),
                }
            }

            // Periodic export loop
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(export_interval));
            // Consume the first immediate tick (we already exported above).
            interval.tick().await;

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let bytes_result = {
                            let db = export_db.lock().expect("db mutex poisoned");
                            export_to_bytes(&db, &opts)
                        };
                        match bytes_result {
                            Ok(bytes) => {
                                if let Err(e) = write_atomic_if_changed(&export_path_clone, &bytes) {
                                    tracing::error!("sync export failed: {e:#}");
                                }
                            }
                            Err(e) => tracing::error!("sync export failed: {e:#}"),
                        }
                    }
                    _ = export_shutdown_rx.recv() => {
                        break;
                    }
                }
            }
        });

        // -- Import task (notify watcher or polling fallback) --
        let import_handle = if config.poll_interval > 0 {
            // Polling mode: check for changes at regular intervals
            Some(Self::start_poll_import(
                db.clone(),
                embedder.clone(),
                sync_dir.clone(),
                export_filename.clone(),
                config.poll_interval,
                shutdown_tx.subscribe(),
            ))
        } else {
            // Watcher mode: use notify for filesystem events with debounce
            match Self::start_watch_import(
                db.clone(),
                embedder.clone(),
                sync_dir.clone(),
                export_filename.clone(),
                shutdown_tx.subscribe(),
            ) {
                Ok(handle) => Some(handle),
                Err(e) => {
                    tracing::warn!("filesystem watcher failed, falling back to 60s polling: {e:#}");
                    Some(Self::start_poll_import(
                        db.clone(),
                        embedder.clone(),
                        sync_dir.clone(),
                        export_filename.clone(),
                        60,
                        shutdown_tx.subscribe(),
                    ))
                }
            }
        };

        Ok(Self {
            shutdown_tx,
            export_handle,
            import_handle,
            config,
            db,
        })
    }

    /// Graceful shutdown: stop tasks, run final export if export_on_exit=true.
    pub async fn shutdown(self) -> Result<()> {
        // Signal shutdown to all tasks
        let _ = self.shutdown_tx.send(());

        // Wait for background tasks to finish
        let _ = self.export_handle.await;
        if let Some(handle) = self.import_handle {
            let _ = handle.await;
        }

        // Final export if configured (best-effort): serialize under lock, write outside lock
        if self.config.export_on_exit {
            let filename = resolve_filename(&self.config.filename);
            let export_filename = format!("{}.{}", filename, self.config.format);
            let export_path = self.config.sync_dir.join(&export_filename);
            let opts = ExportOptions {
                since: None,
                tombstone_retention_days: self.config.tombstone_retention_days,
                purge: true,
            };
            let result = (|| -> Result<()> {
                let bytes = {
                    let db = self.db.lock().expect("db mutex poisoned");
                    export_to_bytes(&db, &opts)?
                };
                write_atomic_if_changed(&export_path, &bytes)?;
                Ok(())
            })();
            if let Err(e) = result {
                tracing::warn!("export_on_exit failed: {e:#}");
            }
        }

        Ok(())
    }

    /// Start polling-based import task.
    fn start_poll_import(
        db: Arc<Mutex<Database>>,
        embedder: Arc<dyn Embedder>,
        sync_dir: PathBuf,
        own_filename: String,
        poll_interval: u64,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> JoinHandle<()> {
        let mtimes: Arc<
            std::sync::Mutex<std::collections::HashMap<PathBuf, std::time::SystemTime>>,
        > = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_interval));

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let peers = match list_peer_files(&sync_dir, &own_filename) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::error!("failed to list peer files: {e:#}");
                                continue;
                            }
                        };

                        for peer_path in peers {
                            let mtime = match std::fs::metadata(&peer_path)
                                .and_then(|m| m.modified())
                            {
                                Ok(t) => t,
                                Err(_) => continue,
                            };

                            let needs_import = {
                                let mut map = mtimes.lock().unwrap();
                                match map.get(&peer_path) {
                                    Some(prev) if *prev >= mtime => false,
                                    _ => {
                                        map.insert(peer_path.clone(), mtime);
                                        true
                                    }
                                }
                            };

                            if needs_import {
                                let db = db.lock().expect("db mutex poisoned");
                                import_peer_file(&db, embedder.as_ref(), &peer_path);
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                }
            }
        })
    }

    /// Start notify-based watcher import task.
    fn start_watch_import(
        db: Arc<Mutex<Database>>,
        embedder: Arc<dyn Embedder>,
        sync_dir: PathBuf,
        own_filename: String,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> Result<JoinHandle<()>> {
        use notify::Watcher;

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

        // Create a debounced watcher (2-second debounce)
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    // Only react to create/modify events
                    match event.kind {
                        notify::EventKind::Create(_) | notify::EventKind::Modify(_) => {
                            let _ = tx.blocking_send(event);
                        }
                        _ => {}
                    }
                }
            })?;

        watcher.watch(&sync_dir, notify::RecursiveMode::NonRecursive)?;

        let handle = tokio::spawn(async move {
            // Keep watcher alive for the duration of this task
            let _watcher = watcher;
            let mut debounce_timer: Option<tokio::time::Instant> = None;
            let debounce_duration = std::time::Duration::from_secs(2);

            loop {
                tokio::select! {
                    Some(event) = rx.recv() => {
                        // Check if any of the changed paths are peer files
                        let is_peer = event.paths.iter().any(|p| {
                            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                                !name.starts_with('.') && name != own_filename
                            } else {
                                false
                            }
                        });

                        if is_peer {
                            debounce_timer = Some(tokio::time::Instant::now() + debounce_duration);
                        }
                    }
                    _ = async {
                        if let Some(deadline) = debounce_timer {
                            tokio::time::sleep_until(deadline).await;
                        } else {
                            // No timer set; sleep forever (effectively disabled)
                            std::future::pending::<()>().await;
                        }
                    } => {
                        debounce_timer = None;

                        // Import all changed peer files
                        let peers = match list_peer_files(&sync_dir, &own_filename) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::error!("failed to list peer files: {e:#}");
                                continue;
                            }
                        };

                        let db = db.lock().expect("db mutex poisoned");
                        for peer_path in peers {
                            import_peer_file(&db, embedder.as_ref(), &peer_path);
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                }
            }
        });

        Ok(handle)
    }
}

/// Import from an open file, auto-detecting gzip compression by magic number.
fn import_from_file(
    db: &Database,
    embedder: &dyn Embedder,
    file: std::fs::File,
) -> Result<super::ImportStats> {
    use std::io::{Read, Seek, SeekFrom};

    let mut buf_reader = std::io::BufReader::new(file);
    let is_gzip = {
        let mut magic = [0u8; 2];
        let n = buf_reader.read(&mut magic)?;
        buf_reader.seek(SeekFrom::Start(0))?;
        n == 2 && magic[0] == 0x1f && magic[1] == 0x8b
    };

    if is_gzip {
        let mut gz = flate2::read::GzDecoder::new(buf_reader);
        super::import(db, |texts| embedder.embed_documents(texts), &mut gz)
    } else {
        super::import(db, |texts| embedder.embed_documents(texts), &mut buf_reader)
    }
}

/// Import a single peer export file, logging the result.
fn import_peer_file(db: &Database, embedder: &dyn Embedder, peer_path: &Path) {
    let file = match std::fs::File::open(peer_path) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("failed to open peer file {}: {e}", peer_path.display());
            return;
        }
    };

    match import_from_file(db, embedder, file) {
        Ok(stats) => {
            tracing::info!(
                peer = %peer_path.display(),
                inserted = stats.memories_inserted,
                updated = stats.memories_updated,
                skipped = stats.memories_skipped,
                "imported peer export"
            );
        }
        Err(e) => {
            tracing::error!("failed to import {}: {e:#}", peer_path.display());
        }
    }
}

/// Detect platform: returns "wsl" under WSL, otherwise falls back to OS.
fn detect_platform() -> String {
    // Check for WSL by looking at /proc/version
    #[cfg(target_os = "linux")]
    {
        if let Ok(version) = std::fs::read_to_string("/proc/version") {
            let lower = version.to_lowercase();
            if lower.contains("microsoft") || lower.contains("wsl") {
                return "wsl".to_string();
            }
        }
    }
    std::env::consts::OS.to_string()
}

/// Detect Linux distro ID, falling back to OS name.
fn detect_distro() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(contents) = std::fs::read_to_string("/etc/os-release") {
            for line in contents.lines() {
                if let Some(id) = line.strip_prefix("ID=") {
                    return id.trim_matches('"').to_string();
                }
            }
        }
    }
    std::env::consts::OS.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_filename_replaces_hostname() {
        let result = resolve_filename("{hostname}");
        let expected = gethostname::gethostname().to_string_lossy().to_string();
        assert_eq!(result, expected);
    }

    #[test]
    fn resolve_filename_replaces_os() {
        let result = resolve_filename("{os}");
        assert!(!result.is_empty());
        // On Linux, should be "linux"
        #[cfg(target_os = "linux")]
        assert_eq!(result, "linux");
    }

    #[test]
    fn resolve_filename_replaces_user() {
        let result = resolve_filename("{user}");
        assert!(!result.is_empty());
        // Should match the actual user
        let expected = whoami::username().unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn resolve_filename_replaces_multiple_variables() {
        let result = resolve_filename("{hostname}-{os}");
        let hostname = gethostname::gethostname().to_string_lossy().to_string();
        let os = std::env::consts::OS;
        assert_eq!(result, format!("{hostname}-{os}"));
    }

    #[test]
    fn resolve_filename_preserves_literal_text() {
        let result = resolve_filename("my-machine");
        assert_eq!(result, "my-machine");
    }

    #[test]
    fn resolve_filename_sanitizes_slashes_and_nul() {
        // Template literals pass through sanitization too
        let result = resolve_filename("foo/bar\\baz\0qux");
        assert_eq!(result, "foo_bar_baz_qux");
    }

    #[test]
    fn resolve_filename_sanitizes_dangerous_chars() {
        let result = resolve_filename("a:b<c>d");
        assert_eq!(result, "a_b_c_d");
    }

    // ── atomic_export tests ──────────────────────────────────────────

    fn test_db() -> Database {
        Database::open_in_memory(&crate::db::DbConfig::default()).unwrap()
    }

    fn mock_embedder() -> crate::embedding::MockEmbedder {
        crate::embedding::MockEmbedder::new(768)
    }

    use crate::embedding::Embedder;

    fn store_test_memory(db: &Database, content: &str) -> String {
        let emb = mock_embedder();
        let embedding = emb.embed_documents(&[content]).unwrap().remove(0);
        db.store(&crate::db::types::StoreParams {
            content,
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &embedding,
        })
        .unwrap()
    }

    #[test]
    fn atomic_export_creates_file_and_returns_true() {
        let db = test_db();
        store_test_memory(&db, "test memory for atomic export");

        let dir = tempfile::tempdir().unwrap();
        let export_path = dir.path().join("test-export.jsonl");
        let opts = ExportOptions::default();

        let changed = atomic_export(&db, &export_path, &opts).unwrap();
        assert!(changed, "first export should return true (file created)");
        assert!(export_path.exists(), "export file should exist");

        // File should contain the exported memory
        let contents = std::fs::read_to_string(&export_path).unwrap();
        assert!(
            contents.contains("test memory for atomic export"),
            "export file should contain the memory"
        );
    }

    #[test]
    fn atomic_export_returns_true_when_content_changed() {
        let db = test_db();
        store_test_memory(&db, "first memory");

        let dir = tempfile::tempdir().unwrap();
        let export_path = dir.path().join("test-export.jsonl");
        let opts = ExportOptions::default();

        // First export
        let first = atomic_export(&db, &export_path, &opts).unwrap();
        assert!(first, "first export should return true");

        // Add new data
        store_test_memory(&db, "second memory");

        // Second export with changed data
        let second = atomic_export(&db, &export_path, &opts).unwrap();
        assert!(second, "export with new data should return true");

        // Verify the file contains both memories
        let contents = std::fs::read_to_string(&export_path).unwrap();
        assert!(contents.contains("first memory"));
        assert!(contents.contains("second memory"));
    }

    #[test]
    fn atomic_export_returns_false_when_unchanged() {
        let db = test_db();
        store_test_memory(&db, "unchanged memory");

        let dir = tempfile::tempdir().unwrap();
        let export_path = dir.path().join("test-export.jsonl");
        let opts = ExportOptions::default();

        // First export
        let first = atomic_export(&db, &export_path, &opts).unwrap();
        assert!(first, "first export should return true");

        // Second export with same data
        let second = atomic_export(&db, &export_path, &opts).unwrap();
        assert!(
            !second,
            "second export with unchanged data should return false"
        );
    }

    // ── restore_from_peers tests ──────────────────────────────────────

    #[test]
    fn restore_from_peers_imports_all_peer_exports() {
        let emb = mock_embedder();

        // Machine A: create a DB, store a memory, export
        let db_a = test_db();
        store_test_memory(&db_a, "memory from machine A");
        let dir = tempfile::tempdir().unwrap();
        let sync_dir = dir.path();
        let opts = ExportOptions::default();
        atomic_export(&db_a, &sync_dir.join("machine-a.jsonl"), &opts).unwrap();

        // Machine B: create a DB, store a memory, export
        let db_b = test_db();
        store_test_memory(&db_b, "memory from machine B");
        atomic_export(&db_b, &sync_dir.join("machine-b.jsonl"), &opts).unwrap();

        // Machine C (our machine): fresh DB, restore from peers
        let db_c = test_db();
        let stats = restore_from_peers(&db_c, &emb, sync_dir, "machine-c.jsonl").unwrap();

        assert_eq!(
            stats.memories_inserted, 2,
            "should import both peers' memories"
        );
    }

    // ── list_peer_files tests ──────────────────────────────────────────

    #[test]
    fn list_peer_files_finds_peers_excludes_own_and_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        let sync_dir = dir.path();

        // Create own file, peer files, and dotfiles
        std::fs::write(sync_dir.join("my-machine.jsonl.gz"), b"own").unwrap();
        std::fs::write(sync_dir.join("workstation.jsonl.gz"), b"peer1").unwrap();
        std::fs::write(sync_dir.join("laptop.jsonl.gz"), b"peer2").unwrap();
        std::fs::write(sync_dir.join(".erinra-export-12345.tmp"), b"temp").unwrap();
        std::fs::write(sync_dir.join(".hidden"), b"hidden").unwrap();

        let peers = list_peer_files(sync_dir, "my-machine.jsonl.gz").unwrap();

        // Should find exactly the two peer files
        let names: Vec<String> = peers
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"workstation.jsonl.gz".to_string()));
        assert!(names.contains(&"laptop.jsonl.gz".to_string()));
    }

    #[test]
    fn list_peer_files_empty_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let peers = list_peer_files(dir.path(), "my-machine.jsonl.gz").unwrap();
        assert!(peers.is_empty());
    }

    // ── SyncHandle tests ─────────────────────────────────────────────

    fn test_sync_config(sync_dir: &Path) -> crate::config::SyncConfig {
        crate::config::SyncConfig {
            enabled: true, // tests need sync enabled
            sync_dir: sync_dir.to_path_buf(),
            filename: "test-machine".to_string(),
            format: crate::config::SyncFormat::Jsonl,
            export_interval: 3600, // large default; tests override as needed
            poll_interval: 0,
            restore_on_start: false,
            export_on_exit: false,
            tombstone_retention_days: 90,
        }
    }

    #[tokio::test]
    async fn sync_handle_exports_immediately_on_start() {
        let db = test_db();
        store_test_memory(&db, "sync handle test memory");

        let dir = tempfile::tempdir().unwrap();
        let config = test_sync_config(dir.path());
        let export_path = dir.path().join("test-machine.jsonl");

        let db = Arc::new(Mutex::new(db));
        let emb: Arc<dyn Embedder> = Arc::new(mock_embedder());

        let handle = SyncHandle::start(db, emb, config).await.unwrap();

        // Give the background task a moment to run the immediate export
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(
            export_path.exists(),
            "export file should be created immediately on start"
        );
        let contents = std::fs::read_to_string(&export_path).unwrap();
        assert!(contents.contains("sync handle test memory"));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn sync_handle_runs_periodic_exports() {
        let db = test_db();
        store_test_memory(&db, "initial memory");

        let dir = tempfile::tempdir().unwrap();
        let mut config = test_sync_config(dir.path());
        config.export_interval = 5; // 5 seconds for test
        let export_path = dir.path().join("test-machine.jsonl");

        let db = Arc::new(Mutex::new(db));
        let emb: Arc<dyn Embedder> = Arc::new(mock_embedder());

        let handle = SyncHandle::start(db.clone(), emb, config).await.unwrap();

        // Advance past first immediate export + yield
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(export_path.exists(), "immediate export should have run");
        let initial_content = std::fs::read(&export_path).unwrap();

        // Add new memory via the shared db
        {
            let db_lock = db.lock().expect("db mutex poisoned");
            store_test_memory(&db_lock, "periodic memory");
        }

        // Advance time past the export interval
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;

        // The file should have been updated with the new memory
        let updated_content = std::fs::read(&export_path).unwrap();
        assert_ne!(
            initial_content, updated_content,
            "periodic export should update the file"
        );
        let contents = String::from_utf8(updated_content).unwrap();
        assert!(contents.contains("periodic memory"));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn sync_handle_imports_when_peer_file_appears() {
        let db = test_db();
        store_test_memory(&db, "local memory");

        let dir = tempfile::tempdir().unwrap();
        let mut config = test_sync_config(dir.path());
        // Use polling for reliable test behavior
        config.poll_interval = 1;
        config.export_interval = 3600; // don't re-export during test

        let db = Arc::new(Mutex::new(db));
        let emb: Arc<dyn Embedder> = Arc::new(mock_embedder());

        let handle = SyncHandle::start(db.clone(), emb, config).await.unwrap();

        // Let the handle do its initial export
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Verify only 1 memory initially
        {
            let db_lock = db.lock().expect("db mutex poisoned");
            assert_eq!(db_lock.count_active_memories().unwrap(), 1);
        }

        // Create a peer export file from a different "machine"
        let peer_db = test_db();
        store_test_memory(&peer_db, "memory from peer machine");
        let opts = ExportOptions::default();
        atomic_export(&peer_db, &dir.path().join("peer.jsonl"), &opts).unwrap();

        // Wait for the poller to detect and import the peer file
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        // Verify the peer's memory was imported (now 2 memories)
        {
            let db_lock = db.lock().expect("db mutex poisoned");
            assert_eq!(
                db_lock.count_active_memories().unwrap(),
                2,
                "peer memory should have been imported by background sync"
            );
        }

        handle.shutdown().await.unwrap();
    }

    #[test]
    fn resolve_filename_output_never_contains_unsafe_chars() {
        // Regardless of what template produces, output should be filename-safe
        let result = resolve_filename("{hostname}");
        let unsafe_chars = ['/', '\\', '\0', ':', '<', '>', '"', '|', '?', '*'];
        for ch in &unsafe_chars {
            assert!(
                !result.contains(*ch),
                "resolved filename should not contain '{ch}'"
            );
        }
    }

    // ── import_from_file tests ────────────────────────────────────────

    #[test]
    fn import_from_file_imports_gzip_jsonl() {
        let emb = mock_embedder();

        // Create a source DB, store a memory, export to plain JSONL, then gzip it.
        let db_src = test_db();
        store_test_memory(&db_src, "gzipped memory");
        let dir = tempfile::tempdir().unwrap();
        let opts = ExportOptions::default();

        // Export to bytes, then gzip-compress and write to file.
        let plain_bytes = export_to_bytes(&db_src, &opts).unwrap();
        let gz_path = dir.path().join("peer.jsonl.gz");
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use std::io::Write;
            let file = std::fs::File::create(&gz_path).unwrap();
            let mut gz = GzEncoder::new(file, Compression::default());
            gz.write_all(&plain_bytes).unwrap();
            gz.finish().unwrap();
        }

        // Import via import_from_file into a fresh DB.
        let db_dst = test_db();
        let file = std::fs::File::open(&gz_path).unwrap();
        let stats = import_from_file(&db_dst, &emb, file).unwrap();

        assert_eq!(stats.memories_inserted, 1);
        assert_eq!(stats.memories_skipped, 0);
        assert_eq!(db_dst.count_active_memories().unwrap(), 1);
    }

    #[test]
    fn import_from_file_imports_plain_jsonl() {
        let emb = mock_embedder();

        // Create a source DB, store a memory, export to a plain JSONL file.
        let db_src = test_db();
        store_test_memory(&db_src, "plain jsonl memory");
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("peer.jsonl");
        let opts = ExportOptions::default();
        atomic_export(&db_src, &file_path, &opts).unwrap();

        // Import via import_from_file into a fresh DB.
        let db_dst = test_db();
        let file = std::fs::File::open(&file_path).unwrap();
        let stats = import_from_file(&db_dst, &emb, file).unwrap();

        assert_eq!(stats.memories_inserted, 1);
        assert_eq!(stats.memories_skipped, 0);
        assert_eq!(db_dst.count_active_memories().unwrap(), 1);
    }
}
