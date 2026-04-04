//! Sync operations: JSONL export/import, tombstones, and conflict resolution.
//!
//! Uses `unchecked_transaction()` for the same reason as `ops_core` — see that module's
//! doc comment for the full rationale (`Database` exposes `&self`, not `&mut self`).

use rusqlite::OptionalExtension;
use rusqlite::types::Value;

use super::Database;
use super::error::{DbError, DbResult};
use super::helpers::*;
use super::types::*;

impl Database {
    // ── tombstones ───────────────────────────────────────────────────────

    /// Create a tombstone entry directly (used in tests).
    /// Production import uses `apply_tombstone` which handles conflict resolution.
    #[cfg(test)]
    pub fn create_tombstone(
        &self,
        entity_type: EntityType,
        entity_id: &str,
        action: TombstoneAction,
    ) -> DbResult<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO tombstones (entity_type, entity_id, action) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![entity_type.as_str(), entity_id, action.as_str()],
        )?;
        Ok(())
    }

    // ── sync export/import ─────────────────────────────────────────────

    /// Export all memories (including archived), sorted by created_at ASC, id ASC.
    /// If `since` is provided, only memories with updated_at > since are returned.
    pub fn export_memories(&self, since: Option<&str>) -> DbResult<Vec<Memory>> {
        let conn = self.conn();

        let (sql, params): (String, Vec<Value>) = if let Some(ts) = since {
            (
                "SELECT id, content, type, created_at, updated_at, archived_at, \
                        last_accessed_at, access_count \
                 FROM memories WHERE updated_at > ?1 \
                 ORDER BY created_at ASC, id ASC"
                    .to_string(),
                vec![Value::Text(ts.to_string())],
            )
        } else {
            (
                "SELECT id, content, type, created_at, updated_at, archived_at, \
                        last_accessed_at, access_count \
                 FROM memories ORDER BY created_at ASC, id ASC"
                    .to_string(),
                vec![],
            )
        };

        let mut stmt = conn.prepare(&sql)?;
        let mut memories: Vec<Memory> = stmt
            .query_map(rusqlite::params_from_iter(&params), map_memory_row)?
            .collect::<Result<_, _>>()?;

        let ids: Vec<String> = memories.iter().map(|m| m.id.clone()).collect();
        let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        fill_projects_and_tags(conn, &mut memories, &id_refs)?;

        Ok(memories)
    }

    /// Export all links, sorted by created_at ASC, id ASC.
    /// If `since` is provided, only links with created_at > since are returned.
    pub fn export_links(&self, since: Option<&str>) -> DbResult<Vec<Link>> {
        let conn = self.conn();

        let (sql, params): (String, Vec<Value>) = if let Some(ts) = since {
            (
                "SELECT id, source_id, target_id, relation, created_at \
                 FROM links WHERE created_at > ?1 \
                 ORDER BY created_at ASC, id ASC"
                    .to_string(),
                vec![Value::Text(ts.to_string())],
            )
        } else {
            (
                "SELECT id, source_id, target_id, relation, created_at \
                 FROM links ORDER BY created_at ASC, id ASC"
                    .to_string(),
                vec![],
            )
        };

        let mut stmt = conn.prepare(&sql)?;
        let links: Vec<Link> = stmt
            .query_map(rusqlite::params_from_iter(&params), map_link)?
            .collect::<Result<_, _>>()?;

        Ok(links)
    }

    /// Export tombstones newer than max_age_days.
    /// Sorted by timestamp ASC, entity_id ASC.
    pub fn export_tombstones(&self, max_age_days: u32) -> DbResult<Vec<Tombstone>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT entity_type, entity_id, action, timestamp \
             FROM tombstones \
             WHERE timestamp > strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?1) \
             ORDER BY timestamp ASC, entity_id ASC",
        )?;
        let cutoff = format!("-{max_age_days} days");
        let rows: Vec<Tombstone> = stmt
            .query_map([&cutoff], |row| {
                Ok(Tombstone {
                    entity_type: row.get(0)?,
                    entity_id: row.get(1)?,
                    action: row.get(2)?,
                    timestamp: row.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Delete tombstones older than max_age_days. Returns count deleted.
    pub fn purge_old_tombstones(&self, max_age_days: u32) -> DbResult<usize> {
        let cutoff = format!("-{max_age_days} days");
        let deleted = self.conn().execute(
            "DELETE FROM tombstones WHERE timestamp <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?1)",
            [&cutoff],
        )?;
        Ok(deleted)
    }

    /// Import a memory via upsert: insert if new, update if remote is newer, skip otherwise.
    /// Returns the action taken.
    pub fn import_memory(&self, params: &ImportMemoryParams) -> DbResult<ImportAction> {
        let ImportMemoryParams {
            id,
            content,
            memory_type,
            projects,
            tags,
            created_at,
            updated_at,
            archived_at,
            embedding,
        } = params;

        // Safety net: enforce limit regardless of caller (MCP, import, etc.).
        let max = self.max_content_size();
        if content.len() > max {
            return Err(DbError::ContentTooLarge {
                actual: content.len(),
                max,
            });
        }

        let conn = self.conn();

        // Check if memory already exists.
        let existing_updated_at: Option<String> = conn
            .query_row(
                "SELECT updated_at FROM memories WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;

        match existing_updated_at {
            Some(local_ts) => {
                // Skip if local is newer or equal (local wins on tie).
                if local_ts.as_str() >= *updated_at {
                    return Ok(ImportAction::Skipped);
                }

                // Remote is newer — update.
                let emb_bytes = embedding_to_bytes(embedding);
                let tx = conn.unchecked_transaction()?;

                tx.execute(
                    "UPDATE memories SET content = ?1, type = ?2, \
                     updated_at = ?3, archived_at = ?4, \
                     embedding = ?5 WHERE id = ?6",
                    rusqlite::params![content, memory_type, updated_at, archived_at, emb_bytes, id],
                )?;

                // Replace projects and tags.
                tx.execute("DELETE FROM memory_projects WHERE memory_id = ?1", [id])?;
                tx.execute("DELETE FROM tags WHERE memory_id = ?1", [id])?;
                insert_projects(&tx, id, projects)?;
                insert_tags(&tx, id, tags)?;

                // Update vec0 embedding.
                tx.execute("DELETE FROM memory_embeddings WHERE memory_id = ?1", [id])?;
                tx.execute(
                    "INSERT INTO memory_embeddings (memory_id, embedding) VALUES (?1, ?2)",
                    rusqlite::params![id, emb_bytes],
                )?;

                tx.commit()?;
                Ok(ImportAction::Updated)
            }
            None => {
                // New memory — insert.
                let emb_bytes = embedding_to_bytes(embedding);
                let tx = conn.unchecked_transaction()?;

                tx.execute(
                    "INSERT INTO memories (id, content, type, created_at, updated_at, archived_at, embedding) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![id, content, memory_type, created_at, updated_at, archived_at, emb_bytes],
                )?;

                insert_projects(&tx, id, projects)?;
                insert_tags(&tx, id, tags)?;

                tx.execute(
                    "INSERT INTO memory_embeddings (memory_id, embedding) VALUES (?1, ?2)",
                    rusqlite::params![id, emb_bytes],
                )?;

                tx.commit()?;
                Ok(ImportAction::Inserted)
            }
        }
    }

    /// Import a link. Inserts if the link ID doesn't exist, skips otherwise.
    pub fn import_link(&self, link: &Link) -> DbResult<ImportAction> {
        let conn = self.conn();

        // Check if link already exists.
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM links WHERE id = ?1)",
            [&link.id],
            |row| row.get(0),
        )?;

        if exists {
            return Ok(ImportAction::Skipped);
        }

        // Verify both memory endpoints exist.
        let source_exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
            [&link.source_id],
            |row| row.get(0),
        )?;
        let target_exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
            [&link.target_id],
            |row| row.get(0),
        )?;

        if !source_exists || !target_exists {
            tracing::warn!(
                link_id = link.id,
                source_id = link.source_id,
                target_id = link.target_id,
                source_exists,
                target_exists,
                "import_link skipped: one or both endpoint memories do not exist"
            );
            return Ok(ImportAction::Skipped);
        }

        conn.execute(
            "INSERT INTO links (id, source_id, target_id, relation, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                link.id,
                link.source_id,
                link.target_id,
                link.relation,
                link.created_at
            ],
        )?;

        Ok(ImportAction::Inserted)
    }

    /// Apply a tombstone: archive/unarchive/delete the entity, then record the tombstone.
    /// Returns true if the tombstone was applied (state changed), false if skipped.
    pub fn apply_tombstone(&self, tombstone: &Tombstone) -> DbResult<bool> {
        let entity_type_str = tombstone.entity_type.as_str();
        let entity_id = tombstone.entity_id.as_str();
        let action_str = tombstone.action.as_str();
        let timestamp = tombstone.timestamp.as_str();

        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;

        match tombstone.entity_type {
            EntityType::Memory => {
                let archived_at: Option<Option<String>> = tx
                    .query_row(
                        "SELECT archived_at FROM memories WHERE id = ?1",
                        [entity_id],
                        |row| row.get(0),
                    )
                    .optional()?;

                let applied = match (&tombstone.action, archived_at) {
                    (TombstoneAction::Archived, Some(None)) => {
                        // Active memory — archive it.
                        tx.execute(
                            "UPDATE memories SET archived_at = ?1 WHERE id = ?2",
                            rusqlite::params![timestamp, entity_id],
                        )?;
                        true
                    }
                    (TombstoneAction::Unarchived, Some(Some(_))) => {
                        // Archived memory — clear archived_at.
                        tx.execute(
                            "UPDATE memories SET archived_at = NULL, updated_at = ?1 WHERE id = ?2",
                            rusqlite::params![timestamp, entity_id],
                        )?;
                        true
                    }
                    _ => {
                        // Already in desired state or doesn't exist — just record the tombstone.
                        false
                    }
                };

                if !applied {
                    tx.execute(
                        "INSERT OR REPLACE INTO tombstones (entity_type, entity_id, action, timestamp) \
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![entity_type_str, entity_id, action_str, timestamp],
                    )?;
                    tx.commit()?;
                    return Ok(false);
                }
            }
            EntityType::Link => {
                let link_exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM links WHERE id = ?1)",
                    [entity_id],
                    |row| row.get(0),
                )?;
                if !link_exists {
                    tx.execute(
                        "INSERT OR REPLACE INTO tombstones (entity_type, entity_id, action, timestamp) \
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![entity_type_str, entity_id, action_str, timestamp],
                    )?;
                    tx.commit()?;
                    return Ok(false);
                }
                tx.execute("DELETE FROM links WHERE id = ?1", [entity_id])?;
            }
        }

        tx.execute(
            "INSERT OR REPLACE INTO tombstones (entity_type, entity_id, action, timestamp) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![entity_type_str, entity_id, action_str, timestamp],
        )?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
    use crate::embedding::{Embedder, MockEmbedder};

    fn test_db() -> Database {
        Database::open_in_memory(&DbConfig::default()).unwrap()
    }

    fn mock_embedder() -> MockEmbedder {
        MockEmbedder::new(768)
    }

    fn test_embedding(embedder: &MockEmbedder, text: &str) -> Vec<f32> {
        embedder.embed_documents(&[text]).unwrap().remove(0)
    }

    fn small_content_db() -> Database {
        let config = DbConfig {
            max_content_size: 20,
            ..DbConfig::default()
        };
        Database::open_in_memory(&config).unwrap()
    }

    #[test]
    fn apply_tombstone_link_nonexistent_returns_false() {
        let db = test_db();

        // Apply tombstone for a link that doesn't exist — should return false (not applied).
        let tombstone = Tombstone {
            entity_type: EntityType::Link,
            entity_id: "nonexistent-link-id".into(),
            action: TombstoneAction::Deleted,
            timestamp: "2026-01-01T00:00:00.000000Z".into(),
        };
        let applied = db.apply_tombstone(&tombstone).unwrap();
        assert!(
            !applied,
            "tombstone for nonexistent link should return false"
        );
    }

    #[test]
    fn apply_tombstone_link_idempotent() {
        let db = test_db();
        let emb = mock_embedder();

        // Create two memories and a link between them.
        let id1 = db
            .store(&StoreParams {
                content: "source",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "source"),
            })
            .unwrap();
        let id2 = db
            .store(&StoreParams {
                content: "target",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "target"),
            })
            .unwrap();
        let link = db.link(&id1, &id2, "related_to").unwrap();

        // First application should return true (link was deleted).
        let tombstone = Tombstone {
            entity_type: EntityType::Link,
            entity_id: link.id.clone(),
            action: TombstoneAction::Deleted,
            timestamp: "2026-01-01T00:00:00.000000Z".into(),
        };
        let applied = db.apply_tombstone(&tombstone).unwrap();
        assert!(applied, "first tombstone application should return true");

        // Second application should return false (link already gone).
        let applied2 = db.apply_tombstone(&tombstone).unwrap();
        assert!(
            !applied2,
            "second tombstone application should return false"
        );
    }

    #[test]
    fn apply_tombstone_accepts_tombstone_struct() {
        let db = test_db();
        let emb = mock_embedder();

        // Create a memory.
        let id = db
            .store(&StoreParams {
                content: "to be archived via struct",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "to be archived via struct"),
            })
            .unwrap();

        // Apply tombstone using Tombstone struct.
        let tombstone = Tombstone {
            entity_type: EntityType::Memory,
            entity_id: id.clone(),
            action: TombstoneAction::Archived,
            timestamp: "2026-01-01T00:00:00.000000Z".into(),
        };
        let applied = db.apply_tombstone(&tombstone).unwrap();
        assert!(applied, "should archive active memory");

        // Verify the memory is archived.
        let results = db.get(&[&id]).unwrap();
        assert!(results[0].memory.archived_at.is_some());

        // Second application should return false.
        let applied2 = db.apply_tombstone(&tombstone).unwrap();
        assert!(!applied2, "already archived should return false");
    }

    #[test]
    fn export_tombstones_returns_tombstone_structs() {
        let db = test_db();

        // Create a tombstone directly.
        db.create_tombstone(EntityType::Memory, "mem-abc", TombstoneAction::Archived)
            .unwrap();

        // Export and verify we get Tombstone structs with correct fields.
        let tombstones: Vec<Tombstone> = db.export_tombstones(90).unwrap();
        assert_eq!(tombstones.len(), 1);
        assert_eq!(tombstones[0].entity_type, EntityType::Memory);
        assert_eq!(tombstones[0].entity_id, "mem-abc");
        assert_eq!(tombstones[0].action, TombstoneAction::Archived);
        assert!(
            !tombstones[0].timestamp.is_empty(),
            "timestamp should be set"
        );
    }

    #[test]
    fn import_memory_accepts_params_struct() {
        let db = test_db();
        let emb = mock_embedder();
        let embedding = test_embedding(&emb, "imported via struct");

        let params = ImportMemoryParams {
            id: "import-struct-1",
            content: "imported via struct",
            memory_type: Some("note"),
            projects: &["proj-a"],
            tags: &["rust"],
            created_at: "2026-01-01T00:00:00.000000Z",
            updated_at: "2026-01-01T00:00:00.000000Z",
            archived_at: None,
            embedding: &embedding,
        };
        let action = db.import_memory(&params).unwrap();
        assert_eq!(action, ImportAction::Inserted);

        // Verify the memory was stored correctly.
        let results = db.get(&["import-struct-1"]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.content, "imported via struct");
        assert_eq!(results[0].memory.memory_type.as_deref(), Some("note"));
        assert_eq!(results[0].memory.projects, vec!["proj-a"]);
        assert_eq!(results[0].memory.tags, vec!["rust"]);
    }

    #[test]
    fn import_memory_rejects_oversized_content() {
        let db = small_content_db();
        let emb = mock_embedder();
        let big = "x".repeat(21);
        let embedding = test_embedding(&emb, &big);

        let params = ImportMemoryParams {
            id: "import-id-1",
            content: &big,
            memory_type: None,
            projects: &[],
            tags: &[],
            created_at: "2026-01-01T00:00:00.000000Z",
            updated_at: "2026-01-01T00:00:00.000000Z",
            archived_at: None,
            embedding: &embedding,
        };
        let err = db.import_memory(&params).unwrap_err();
        assert!(matches!(
            err,
            DbError::ContentTooLarge {
                actual: 21,
                max: 20
            }
        ));
    }

    #[test]
    fn unarchive_tombstone_exports_and_applies_correctly() {
        let db = test_db();
        let emb = mock_embedder();

        // Store and archive a memory.
        let id = db
            .store(&StoreParams {
                content: "will be archived then unarchived",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "will be archived then unarchived"),
            })
            .unwrap();
        db.archive(&id).unwrap();

        // Unarchive it — this writes an 'unarchived' tombstone.
        db.unarchive(&id).unwrap();

        // Export tombstones — this must NOT fail even though the action is 'unarchived'.
        let tombstones = db.export_tombstones(90).unwrap();
        assert_eq!(tombstones.len(), 1);
        assert_eq!(tombstones[0].entity_id, id);
        assert_eq!(tombstones[0].action, TombstoneAction::Unarchived);

        // Now test that apply_tombstone handles the Unarchived action:
        // create a fresh DB, store and archive the same memory, then apply the tombstone.
        let db2 = test_db();
        let emb2 = mock_embedder();
        let embedding = test_embedding(&emb2, "will be archived then unarchived");
        db2.import_memory(&ImportMemoryParams {
            id: &id,
            content: "will be archived then unarchived",
            memory_type: None,
            projects: &[],
            tags: &[],
            created_at: "2026-01-01T00:00:00.000000Z",
            updated_at: "2026-01-01T00:00:00.000000Z",
            archived_at: Some("2026-01-02T00:00:00.000000Z"),
            embedding: &embedding,
        })
        .unwrap();

        // Memory should be archived.
        let results = db2.get(&[&id]).unwrap();
        assert!(results[0].memory.archived_at.is_some());

        // Apply the unarchive tombstone.
        let applied = db2.apply_tombstone(&tombstones[0]).unwrap();
        assert!(applied, "unarchive tombstone should be applied");

        // Memory should now be unarchived.
        let results = db2.get(&[&id]).unwrap();
        assert!(
            results[0].memory.archived_at.is_none(),
            "archived_at should be cleared after applying unarchive tombstone"
        );
    }
}
