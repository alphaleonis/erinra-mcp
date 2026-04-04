//! SQLite schema, queries, and migrations (rusqlite + FTS5 + sqlite-vec).

pub mod error;
mod helpers;
mod ops_core;
mod ops_search;
mod ops_sync;
mod search;
pub mod types;

use std::path::Path;
use std::sync::Once;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension};

/// Current schema version. Increment when adding migrations.
const SCHEMA_VERSION: u32 = 2;

static SQLITE_VEC_INIT: Once = Once::new();

/// Register the sqlite-vec extension as an auto-extension.
/// Safe to call multiple times — only registers once.
fn register_sqlite_vec() {
    SQLITE_VEC_INIT.call_once(|| {
        // SAFETY: sqlite3_vec_init conforms to the sqlite3_auto_extension callback
        // signature (fn(*mut sqlite3, *mut *mut c_char, *const sqlite3_api_routines) -> c_int).
        // sqlite3_auto_extension is process-global: once registered, the extension
        // loads into ALL subsequent connections in this process.
        unsafe {
            #[allow(clippy::missing_transmute_annotations)]
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// Configuration for database initialization.
pub struct DbConfig {
    /// SQLite busy timeout in milliseconds.
    pub busy_timeout_ms: u32,
    /// Number of dimensions in the embedding vectors.
    pub embedding_dimensions: u32,
    /// Name of the embedding model (e.g. "NomicEmbedTextV15Q").
    pub embedding_model: String,
    /// Maximum content size in bytes (enforced on store/update/merge).
    pub max_content_size: usize,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            busy_timeout_ms: crate::config::DatabaseConfig::default().busy_timeout,
            embedding_dimensions: 768, // model-dependent; overridden at runtime
            embedding_model: crate::config::EmbeddingConfig::default().model,
            max_content_size: crate::config::StoreConfig::default().max_content_size,
        }
    }
}

/// Manages the SQLite database connection and schema.
pub struct Database {
    conn: Connection,
    /// Maximum content size in bytes (enforced on store/update/merge).
    max_content_size: usize,
}

impl Database {
    /// Open (or create) a database at the given path.
    ///
    /// Verifies that the stored embedding config matches the provided config —
    /// callers must have loaded the embedder to supply correct dimensions.
    pub fn open(path: &Path, config: &DbConfig) -> Result<Self> {
        register_sqlite_vec();
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open database at {}", path.display()))?;
        Self::restrict_file_permissions(path)?;
        let db = Self::init(conn, config)?;
        db.verify_embedding_config(config)?;
        Ok(db)
    }

    /// Open an in-memory database (for testing).
    #[cfg(test)]
    pub fn open_in_memory(config: &DbConfig) -> Result<Self> {
        register_sqlite_vec();
        let conn = Connection::open_in_memory()?;
        let db = Self::init(conn, config)?;
        db.verify_embedding_config(config)?;
        Ok(db)
    }

    /// Open an existing database for read-only inspection.
    ///
    /// Skips both embedding config verification and schema migrations.
    /// Used by CLI commands (e.g. `status`) that don't load the embedder
    /// and should not modify the database.
    pub fn open_unverified(path: &Path, config: &DbConfig) -> Result<Self> {
        register_sqlite_vec();
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open database at {}", path.display()))?;
        Self::restrict_file_permissions(path)?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "busy_timeout", config.busy_timeout_ms)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self {
            conn,
            max_content_size: config.max_content_size,
        })
    }

    /// Set restrictive permissions (owner-only read/write) on the database file.
    #[cfg(unix)]
    fn restrict_file_permissions(path: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set permissions on {}", path.display()))
    }

    #[cfg(not(unix))]
    fn restrict_file_permissions(_path: &Path) -> Result<()> {
        Ok(())
    }

    fn init(conn: Connection, config: &DbConfig) -> Result<Self> {
        // WAL mode for concurrent access (no-op on in-memory databases).
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "busy_timeout", config.busy_timeout_ms)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let mut db = Self {
            conn,
            max_content_size: config.max_content_size,
        };
        db.migrate(config)?;
        Ok(db)
    }

    /// Returns a reference to the underlying connection.
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Maximum content size in bytes.
    pub(crate) fn max_content_size(&self) -> usize {
        self.max_content_size
    }

    /// Get a metadata value by key.
    pub fn get_metadata(&self, key: &str) -> Result<Option<String>> {
        let value = self
            .conn
            .query_row("SELECT value FROM metadata WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(value)
    }

    /// Set a metadata value.
    pub fn set_metadata(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Recreate the vec0 virtual table with new dimensions.
    /// Drops all existing embeddings — the caller must re-insert them.
    pub fn recreate_vec_table(&self, new_dims: u32) -> Result<()> {
        self.conn
            .execute("DROP TABLE IF EXISTS memory_embeddings", [])?;
        self.conn.execute(
            &format!(
                "CREATE VIRTUAL TABLE memory_embeddings USING vec0(
                    memory_id TEXT PRIMARY KEY,
                    embedding float[{new_dims}] distance_metric=cosine
                )"
            ),
            [],
        )?;
        Ok(())
    }

    /// Apply schema migrations up to SCHEMA_VERSION.
    fn migrate(&mut self, config: &DbConfig) -> Result<()> {
        // Bootstrap the metadata table (must exist before we can check schema version).
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;

        let current = self.schema_version()?;

        for version in (current + 1)..=SCHEMA_VERSION {
            let tx = self.conn.transaction()?;
            match version {
                1 => Self::migration_v1(&tx, config)?,
                2 => Self::migration_v2(&tx)?,
                _ => bail!("unknown schema version: {version}"),
            }
            tx.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', ?1)",
                [version.to_string()],
            )?;
            tx.commit()?;
        }

        Ok(())
    }

    fn schema_version(&self) -> Result<u32> {
        let version: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match version {
            Some(v) => Ok(v
                .parse::<u32>()
                .context("corrupted schema_version in metadata")?),
            None => Ok(0),
        }
    }

    fn migration_v1(tx: &rusqlite::Transaction, config: &DbConfig) -> Result<()> {
        tx.execute_batch(
            "-- Core memories table.
            -- IMPORTANT: Must NOT use WITHOUT ROWID. FTS5 external content table uses
            -- implicit rowid. VACUUM may reassign rowids — avoid VACUUM or rebuild
            -- FTS index after.
            CREATE TABLE memories (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                type TEXT,
                embedding BLOB,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                archived_at TEXT,
                last_accessed_at TEXT,
                access_count INTEGER NOT NULL DEFAULT 0
            );

            -- Project associations (many-to-many)
            CREATE TABLE memory_projects (
                memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                project TEXT NOT NULL,
                PRIMARY KEY (memory_id, project)
            );
            CREATE INDEX idx_memory_projects_project ON memory_projects(project);

            -- Tags (many-to-many)
            CREATE TABLE tags (
                memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                tag TEXT NOT NULL,
                PRIMARY KEY (memory_id, tag)
            );
            CREATE INDEX idx_tags_tag ON tags(tag);

            -- Links between memories
            CREATE TABLE links (
                id TEXT PRIMARY KEY,
                source_id TEXT NOT NULL REFERENCES memories(id) ON DELETE RESTRICT,
                target_id TEXT NOT NULL REFERENCES memories(id) ON DELETE RESTRICT,
                relation TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );
            CREATE INDEX idx_links_source ON links(source_id);
            CREATE INDEX idx_links_target ON links(target_id);

            -- Tombstones for sync convergence
            CREATE TABLE tombstones (
                entity_type TEXT NOT NULL,
                entity_id TEXT NOT NULL,
                action TEXT NOT NULL,
                timestamp TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                PRIMARY KEY (entity_type, entity_id)
            );

            -- FTS5 full-text search index on memory content (external content table)
            CREATE VIRTUAL TABLE memories_fts USING fts5(
                content,
                content='memories',
                content_rowid='rowid'
            );

            -- Triggers to keep FTS5 index in sync with memories table
            CREATE TRIGGER memories_fts_insert AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
            END;

            CREATE TRIGGER memories_fts_delete AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, content)
                    VALUES ('delete', old.rowid, old.content);
            END;

            -- vec0 virtual tables are not covered by FK CASCADE, so clean up
            -- orphaned embeddings explicitly via trigger.
            CREATE TRIGGER memories_embeddings_delete AFTER DELETE ON memories BEGIN
                DELETE FROM memory_embeddings WHERE memory_id = old.id;
            END;

            CREATE TRIGGER memories_fts_update AFTER UPDATE OF content ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, content)
                    VALUES ('delete', old.rowid, old.content);
                INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
            END;",
        )?;

        // vec0 virtual table for vector search — dimension is configurable per embedding model.
        // Cosine distance matches the design doc's "cosine similarity" search strategy.
        // Safety: embedding_dimensions is u32, so format! can only produce digits here.
        tx.execute(
            &format!(
                "CREATE VIRTUAL TABLE memory_embeddings USING vec0(
                    memory_id TEXT PRIMARY KEY,
                    embedding float[{dim}] distance_metric=cosine
                )",
                dim = config.embedding_dimensions
            ),
            [],
        )?;

        // Store embedding configuration in metadata for mismatch detection on startup.
        tx.execute(
            "INSERT INTO metadata (key, value) VALUES ('embedding_model', ?1)",
            [&config.embedding_model],
        )?;
        tx.execute(
            "INSERT INTO metadata (key, value) VALUES ('embedding_dimensions', ?1)",
            [config.embedding_dimensions.to_string()],
        )?;

        Ok(())
    }

    /// Add unique constraint on links to prevent duplicate (source, target, relation) triples.
    fn migration_v2(tx: &rusqlite::Transaction) -> Result<()> {
        tx.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_links_unique \
             ON links(source_id, target_id, relation);",
        )?;
        Ok(())
    }

    /// Verify that the stored embedding config matches the provided config.
    /// Refuses to start if there's a mismatch (vectors from different models are incomparable).
    fn verify_embedding_config(&self, config: &DbConfig) -> Result<()> {
        if let Some(stored_model) = self.get_metadata("embedding_model")?
            && stored_model != config.embedding_model
        {
            bail!(
                "embedding model mismatch: database uses '{}' but config specifies '{}'. \
                     Run `erinra reembed --model {}` to re-embed with the new model.",
                stored_model,
                config.embedding_model,
                config.embedding_model
            );
        }
        if let Some(stored_dims) = self.get_metadata("embedding_dimensions")? {
            let stored: u32 = stored_dims
                .parse()
                .context("invalid embedding_dimensions in metadata")?;
            if stored != config.embedding_dimensions {
                bail!(
                    "embedding dimensions mismatch: database uses {} but config specifies {}",
                    stored,
                    config.embedding_dimensions
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> DbConfig {
        DbConfig::default()
    }

    #[test]
    fn open_in_memory_sets_schema_version() {
        let db = Database::open_in_memory(&test_config()).unwrap();
        assert_eq!(db.schema_version().unwrap(), 2);
    }

    #[test]
    fn all_tables_created() {
        let db = Database::open_in_memory(&test_config()).unwrap();
        let tables: Vec<String> = db
            .conn()
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        for expected in [
            "memories",
            "memory_projects",
            "tags",
            "links",
            "tombstones",
            "metadata",
        ] {
            assert!(
                tables.contains(&expected.to_string()),
                "missing table: {expected}"
            );
        }
    }

    #[test]
    fn fts5_index_created() {
        let db = Database::open_in_memory(&test_config()).unwrap();
        let count: u32 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'memories_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn vec0_table_created() {
        let db = Database::open_in_memory(&test_config()).unwrap();
        let count: u32 = db
            .conn()
            .query_row("SELECT count(*) FROM memory_embeddings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn embedding_metadata_stored() {
        let db = Database::open_in_memory(&test_config()).unwrap();
        assert_eq!(
            db.get_metadata("embedding_model").unwrap().as_deref(),
            Some("NomicEmbedTextV15Q")
        );
        assert_eq!(
            db.get_metadata("embedding_dimensions").unwrap().as_deref(),
            Some("768")
        );
    }

    #[test]
    fn embedding_model_mismatch_rejected() {
        let db = Database::open_in_memory(&test_config()).unwrap();
        let bad_config = DbConfig {
            embedding_model: "DifferentModel".to_string(),
            ..DbConfig::default()
        };
        let err = db.verify_embedding_config(&bad_config).unwrap_err();
        assert!(err.to_string().contains("embedding model mismatch"));
    }

    #[test]
    fn embedding_dimensions_mismatch_rejected() {
        let db = Database::open_in_memory(&test_config()).unwrap();
        let bad_config = DbConfig {
            embedding_dimensions: 384,
            ..DbConfig::default()
        };
        let err = db.verify_embedding_config(&bad_config).unwrap_err();
        assert!(err.to_string().contains("embedding dimensions mismatch"));
    }

    #[test]
    fn migration_is_idempotent() {
        let config = test_config();
        let mut db = Database::open_in_memory(&config).unwrap();
        assert_eq!(db.schema_version().unwrap(), 2);
        // Running migrate again should be a no-op.
        db.migrate(&config).unwrap();
        assert_eq!(db.schema_version().unwrap(), 2);
    }

    #[test]
    fn metadata_get_set_overwrite() {
        let db = Database::open_in_memory(&test_config()).unwrap();
        db.set_metadata("test_key", "value1").unwrap();
        assert_eq!(
            db.get_metadata("test_key").unwrap().as_deref(),
            Some("value1")
        );
        db.set_metadata("test_key", "value2").unwrap();
        assert_eq!(
            db.get_metadata("test_key").unwrap().as_deref(),
            Some("value2")
        );
    }

    #[test]
    fn metadata_missing_key_returns_none() {
        let db = Database::open_in_memory(&test_config()).unwrap();
        assert_eq!(db.get_metadata("nonexistent").unwrap(), None);
    }

    #[test]
    fn fts5_triggers_sync_content() {
        let db = Database::open_in_memory(&test_config()).unwrap();
        let conn = db.conn();

        // Insert a memory.
        conn.execute(
            "INSERT INTO memories (id, content) VALUES ('m1', 'hello world rust programming')",
            [],
        )
        .unwrap();

        // FTS should find it by keyword.
        let count: u32 = conn
            .query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'rust'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Update content — old term should vanish, new term should appear.
        conn.execute(
            "UPDATE memories SET content = 'hello world python programming' WHERE id = 'm1'",
            [],
        )
        .unwrap();

        let rust_count: u32 = conn
            .query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'rust'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rust_count, 0);

        let python_count: u32 = conn
            .query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'python'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(python_count, 1);

        // Delete the memory — FTS should be empty.
        conn.execute("DELETE FROM memories WHERE id = 'm1'", [])
            .unwrap();

        let count: u32 = conn
            .query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'python'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn vec0_insert_and_query() {
        let db = Database::open_in_memory(&test_config()).unwrap();
        let conn = db.conn();

        conn.execute(
            "INSERT INTO memories (id, content) VALUES ('m1', 'test memory')",
            [],
        )
        .unwrap();

        // Create a 768-dim vector.
        let mut embedding = vec![0.0f32; 768];
        embedding[0] = 1.0;
        let bytes: Vec<u8> = embedding.iter().flat_map(|v| v.to_le_bytes()).collect();

        conn.execute(
            "INSERT INTO memory_embeddings (memory_id, embedding) VALUES (?1, ?2)",
            rusqlite::params!["m1", bytes],
        )
        .unwrap();

        let count: u32 = conn
            .query_row("SELECT count(*) FROM memory_embeddings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);

        // KNN query should return it.
        let query_bytes: Vec<u8> = embedding.iter().flat_map(|v| v.to_le_bytes()).collect();
        let (id, distance): (String, f32) = conn
            .query_row(
                "SELECT memory_id, distance FROM memory_embeddings
                 WHERE embedding MATCH ?1
                 ORDER BY distance ASC LIMIT 1",
                rusqlite::params![query_bytes],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(id, "m1");
        assert!(distance < 0.001, "self-match distance should be ~0");
    }

    #[test]
    fn foreign_keys_enforced() {
        let db = Database::open_in_memory(&test_config()).unwrap();
        let err = db
            .conn()
            .execute(
                "INSERT INTO tags (memory_id, tag) VALUES ('nonexistent', 'test')",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("FOREIGN KEY constraint failed"),
            "expected FK violation, got: {err}"
        );
    }

    #[test]
    fn file_based_db_uses_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        let db = Database::open(&db_path, &test_config()).unwrap();
        let mode: String = db
            .conn()
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    #[cfg(unix)]
    fn file_based_db_has_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        let _db = Database::open(&db_path, &test_config()).unwrap();
        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "database file should be owner-only (0600), got {mode:o}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn open_unverified_has_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        // Create the DB first so open_unverified has a file to open.
        let _db = Database::open(&db_path, &test_config()).unwrap();
        drop(_db);
        let _db = Database::open_unverified(&db_path, &test_config()).unwrap();
        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "database file should be owner-only (0600), got {mode:o}"
        );
    }
}
