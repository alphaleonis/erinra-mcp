//! CRUD operations for memories, tags, projects, and links.
//!
//! All mutating operations use `unchecked_transaction()` rather than `transaction()` because
//! `Database` exposes only `&self` (not `&mut self`), and rusqlite's `transaction()` requires
//! `&mut Connection`. `unchecked_transaction()` skips the compile-time exclusivity check but
//! is safe here since each method runs to completion before returning.

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::OptionalExtension;
use rusqlite::types::Value;
use uuid::Uuid;

use super::Database;
use super::error::{DbError, DbResult};
use super::helpers::{
    build_base_filter, embedding_to_bytes, fill_projects_and_tags, insert_projects, insert_tags,
    map_link, map_memory_row,
};
use super::types::*;

impl Database {
    // ── store ────────────────────────────────────────────────────────────

    /// Insert a new memory with its projects, tags, links, and embedding.
    /// Returns the generated UUID.
    pub fn store(&self, p: &StoreParams) -> DbResult<String> {
        // Safety net: enforce limit regardless of caller (MCP, import, etc.).
        let max = self.max_content_size();
        if p.content.len() > max {
            return Err(DbError::ContentTooLarge {
                actual: p.content.len(),
                max,
            });
        }

        let id = Uuid::new_v4().to_string();
        let emb_bytes = embedding_to_bytes(p.embedding);

        let tx = self.conn().unchecked_transaction()?;

        tx.execute(
            "INSERT INTO memories (id, content, type, embedding) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, p.content, p.memory_type, emb_bytes],
        )?;

        insert_projects(&tx, &id, p.projects)?;
        insert_tags(&tx, &id, p.tags)?;

        // Deduplicate links — keep only the first occurrence of each (target_id, relation).
        // Silent dedup here (vs. DuplicateLink error in link()) because store() accepts
        // batch input from the LLM where accidental duplicates are expected; link() is an
        // explicit single-link operation where duplicates indicate a caller logic error.
        let mut seen_links = HashSet::new();
        let unique_links: Vec<_> = p
            .links
            .iter()
            .filter(|&&(target_id, relation)| seen_links.insert((target_id, relation)))
            .collect();

        // Validate link targets exist before inserting.
        for &&(target_id, _) in &unique_links {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
                [target_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(DbError::NotFound {
                    entity: "link target memory",
                    id: target_id.to_string(),
                });
            }
        }

        // Insert links (this memory as source).
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO links (id, source_id, target_id, relation) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for &&(target_id, relation) in &unique_links {
                let link_id = Uuid::new_v4().to_string();
                stmt.execute(rusqlite::params![link_id, id, target_id, relation])?;
            }
        }

        // Embedding is stored in two places:
        // - memories.embedding (BLOB): used for export/import so embeddings travel with the row
        // - memory_embeddings (vec0): used for cosine-similarity vector search
        // Both must be kept in sync on store, update, and reembed.
        tx.execute(
            "INSERT INTO memory_embeddings (memory_id, embedding) VALUES (?1, ?2)",
            rusqlite::params![id, emb_bytes],
        )?;

        tx.commit()?;
        Ok(id)
    }

    // ── update ───────────────────────────────────────────────────────────

    /// Modify an existing memory. Re-embeds if content changed.
    /// Omitted fields (None) are not modified. Provided fields fully replace.
    pub fn update(&self, id: &str, p: &UpdateParams) -> DbResult<UpdateResult> {
        // Safety net: enforce limit regardless of caller (MCP, import, etc.).
        if let Some(content) = p.content {
            let max = self.max_content_size();
            if content.len() > max {
                return Err(DbError::ContentTooLarge {
                    actual: content.len(),
                    max,
                });
            }
        }

        let tx = self.conn().unchecked_transaction()?;

        // Verify the memory exists and is not archived.
        let archived_at: Option<Option<String>> = tx
            .query_row(
                "SELECT archived_at FROM memories WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        match archived_at {
            None => {
                return Err(DbError::NotFound {
                    entity: "memory",
                    id: id.to_string(),
                });
            }
            Some(Some(_)) => {
                return Err(DbError::AlreadyArchived {
                    id: id.to_string(),
                    operation: "update".into(),
                });
            }
            Some(None) => {} // active memory, proceed
        }

        // Content and embedding must be updated together — a content change without
        // a new embedding would leave the vec0 search index stale.
        if p.content.is_some() && p.embedding.is_none() {
            return Err(DbError::InvalidInput {
                message: "embedding is required when content is changed".into(),
            });
        }

        let has_content_changes = p.content.is_some()
            || p.memory_type.is_change()
            || p.projects.is_some()
            || p.tags.is_some();

        let has_changes = has_content_changes || p.embedding.is_some();

        if !has_changes {
            let updated_at: String = tx.query_row(
                "SELECT updated_at FROM memories WHERE id = ?1",
                [id],
                |row| row.get(0),
            )?;
            tx.commit()?;
            return Ok(UpdateResult {
                id: id.to_string(),
                updated_at,
            });
        }

        // Only bump updated_at for content changes, not embedding-only updates (reembed).
        if has_content_changes {
            tx.execute(
                "UPDATE memories SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
                [id],
            )?;
        }

        // Update content + embedding (always paired due to the guard above).
        if let Some(content) = p.content {
            // Guard at line 111 ensures p.embedding is Some when p.content is Some.
            let emb_bytes = embedding_to_bytes(
                p.embedding
                    .expect("embedding required when content is set — enforced by guard above"),
            );
            tx.execute(
                "UPDATE memories SET content = ?1, embedding = ?2 WHERE id = ?3",
                rusqlite::params![content, emb_bytes, id],
            )?;
            tx.execute("DELETE FROM memory_embeddings WHERE memory_id = ?1", [id])?;
            tx.execute(
                "INSERT INTO memory_embeddings (memory_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![id, &emb_bytes],
            )?;
        } else if let Some(embedding) = p.embedding {
            // Embedding-only update (e.g. reembed without content change).
            let emb_bytes = embedding_to_bytes(embedding);
            tx.execute(
                "UPDATE memories SET embedding = ?1 WHERE id = ?2",
                rusqlite::params![emb_bytes, id],
            )?;
            tx.execute("DELETE FROM memory_embeddings WHERE memory_id = ?1", [id])?;
            tx.execute(
                "INSERT INTO memory_embeddings (memory_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![id, emb_bytes],
            )?;
        }

        match &p.memory_type {
            FieldUpdate::NoChange => {}
            FieldUpdate::Clear => {
                tx.execute("UPDATE memories SET type = NULL WHERE id = ?1", [id])?;
            }
            FieldUpdate::Set(memory_type) => {
                tx.execute(
                    "UPDATE memories SET type = ?1 WHERE id = ?2",
                    rusqlite::params![memory_type, id],
                )?;
            }
        }

        if let Some(projects) = p.projects {
            tx.execute("DELETE FROM memory_projects WHERE memory_id = ?1", [id])?;
            insert_projects(&tx, id, projects)?;
        }

        if let Some(tags) = p.tags {
            tx.execute("DELETE FROM tags WHERE memory_id = ?1", [id])?;
            insert_tags(&tx, id, tags)?;
        }

        let updated_at: String = tx.query_row(
            "SELECT updated_at FROM memories WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        tx.commit()?;

        Ok(UpdateResult {
            id: id.to_string(),
            updated_at,
        })
    }

    // ── archive ──────────────────────────────────────────────────────────

    /// Soft-delete a memory by setting archived_at. Creates a tombstone for sync.
    pub fn archive(&self, id: &str) -> DbResult<ArchiveResult> {
        let tx = self.conn().unchecked_transaction()?;

        let archived_at: Option<Option<String>> = tx
            .query_row(
                "SELECT archived_at FROM memories WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;

        match archived_at {
            None => {
                return Err(DbError::NotFound {
                    entity: "memory",
                    id: id.to_string(),
                });
            }
            Some(Some(_)) => {
                return Err(DbError::AlreadyArchived {
                    id: id.to_string(),
                    operation: "archive".into(),
                });
            }
            Some(None) => {} // active memory, proceed
        }

        tx.execute(
            "UPDATE memories SET archived_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            [id],
        )?;

        tx.execute(
            "INSERT OR REPLACE INTO tombstones (entity_type, entity_id, action) \
             VALUES ('memory', ?1, 'archived')",
            [id],
        )?;

        let new_archived_at: String = tx.query_row(
            "SELECT archived_at FROM memories WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;

        tx.commit()?;

        Ok(ArchiveResult {
            id: id.to_string(),
            archived_at: new_archived_at,
        })
    }

    // ── unarchive ────────────────────────────────────────────────────────

    /// Restore an archived memory to active status.
    ///
    /// Clears `archived_at`, bumps `updated_at`, and creates a tombstone with
    /// action `'unarchived'`. Returns `NotArchived` if the memory is already
    /// active, `NotFound` if the ID doesn't exist.
    pub fn unarchive(&self, id: &str) -> DbResult<UnarchiveResult> {
        let tx = self.conn().unchecked_transaction()?;

        let archived_at: Option<Option<String>> = tx
            .query_row(
                "SELECT archived_at FROM memories WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;

        match archived_at {
            None => {
                return Err(DbError::NotFound {
                    entity: "memory",
                    id: id.to_string(),
                });
            }
            Some(None) => {
                return Err(DbError::NotArchived { id: id.to_string() });
            }
            Some(Some(_)) => {} // archived memory, proceed
        }

        tx.execute(
            "UPDATE memories SET archived_at = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            [id],
        )?;

        tx.execute(
            "INSERT OR REPLACE INTO tombstones (entity_type, entity_id, action) \
             VALUES ('memory', ?1, 'unarchived')",
            [id],
        )?;

        let new_updated_at: String = tx.query_row(
            "SELECT updated_at FROM memories WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;

        tx.commit()?;

        Ok(UnarchiveResult {
            id: id.to_string(),
            updated_at: new_updated_at,
        })
    }

    // ── get ──────────────────────────────────────────────────────────────

    /// Fetch full details of specific memories including links in both directions.
    /// Duplicate IDs are collapsed — each memory appears at most once in the result.
    pub fn get(&self, ids: &[&str]) -> DbResult<Vec<MemoryWithLinks>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        // Deduplicate while preserving request order.
        let mut seen = HashSet::new();
        let deduped: Vec<&str> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
        let ids = deduped.as_slice();

        let conn = self.conn();
        let placeholders = vec!["?"; ids.len()].join(",");
        let id_values: Vec<Value> = ids.iter().map(|id| Value::Text(id.to_string())).collect();

        // Fetch memory rows.
        let sql = format!(
            "SELECT id, content, type, created_at, updated_at, archived_at, \
                    last_accessed_at, access_count \
             FROM memories WHERE id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut memories: Vec<Memory> = stmt
            .query_map(rusqlite::params_from_iter(&id_values), map_memory_row)?
            .collect::<Result<_, _>>()?;

        // Verify all requested IDs were found.
        if memories.len() != ids.len() {
            let found: std::collections::HashSet<&str> =
                memories.iter().map(|m| m.id.as_str()).collect();
            for &id in ids {
                if !found.contains(id) {
                    return Err(DbError::NotFound {
                        entity: "memory",
                        id: id.to_string(),
                    });
                }
            }
        }

        // Fetch projects and tags.
        let mem_ids: Vec<String> = memories.iter().map(|m| m.id.clone()).collect();
        let mem_id_refs: Vec<&str> = mem_ids.iter().map(|s| s.as_str()).collect();
        fill_projects_and_tags(conn, &mut memories, &mem_id_refs)?;

        // Fetch outgoing links.
        let sql = format!(
            "SELECT id, source_id, target_id, relation, created_at FROM links \
             WHERE source_id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut outgoing: Vec<Link> = stmt
            .query_map(rusqlite::params_from_iter(&id_values), map_link)?
            .collect::<Result<_, _>>()?;

        // Fetch incoming links.
        let sql = format!(
            "SELECT id, source_id, target_id, relation, created_at FROM links \
             WHERE target_id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut incoming: Vec<Link> = stmt
            .query_map(rusqlite::params_from_iter(&id_values), map_link)?
            .collect::<Result<_, _>>()?;

        // Enrich links with content snippets from linked memories.
        // Outgoing links reference target_id, incoming links reference source_id.
        let mut linked_ids: HashSet<&str> = HashSet::new();
        for link in &outgoing {
            linked_ids.insert(&link.target_id);
        }
        for link in &incoming {
            linked_ids.insert(&link.source_id);
        }
        if !linked_ids.is_empty() {
            let linked_ids_vec: Vec<&str> = linked_ids.into_iter().collect();
            let snippet_placeholders = vec!["?"; linked_ids_vec.len()].join(",");
            let snippet_sql = format!(
                "SELECT id, CASE WHEN LENGTH(content) > 100 \
                 THEN SUBSTR(content, 1, 97) || '...' \
                 ELSE content END \
                 FROM memories WHERE id IN ({snippet_placeholders})"
            );
            let snippet_params: Vec<Value> = linked_ids_vec
                .iter()
                .map(|id| Value::Text(id.to_string()))
                .collect();
            let mut stmt = conn.prepare(&snippet_sql)?;
            let snippets: std::collections::HashMap<String, String> = stmt
                .query_map(rusqlite::params_from_iter(&snippet_params), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<_, _>>()?;

            for link in &mut outgoing {
                link.content = snippets.get(&link.target_id).cloned();
            }
            for link in &mut incoming {
                link.content = snippets.get(&link.source_id).cloned();
            }
        }

        // Sort memories to match the caller's requested ID order.
        let position: std::collections::HashMap<&str, usize> =
            ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        memories.sort_by_key(|m| position.get(m.id.as_str()).copied().unwrap_or(usize::MAX));

        let results: Vec<MemoryWithLinks> = memories
            .into_iter()
            .map(|mem| {
                let out = outgoing
                    .iter()
                    .filter(|l| l.source_id == mem.id)
                    .cloned()
                    .collect();
                let inc = incoming
                    .iter()
                    .filter(|l| l.target_id == mem.id)
                    .cloned()
                    .collect();
                MemoryWithLinks {
                    memory: mem,
                    outgoing_links: out,
                    incoming_links: inc,
                }
            })
            .collect();

        Ok(results)
    }

    // ── list ─────────────────────────────────────────────────────────────

    /// Filter and browse memories without a search query.
    pub fn list(&self, p: &ListParams) -> DbResult<ListResult> {
        // Wrap count + data queries in a transaction for consistency.
        let tx = self.conn().unchecked_transaction()?;

        let filter = build_base_filter(&p.filter);

        let where_clause = if filter.sql.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", filter.sql)
        };

        // Count query (uses only filter params).
        let count_sql = format!("SELECT COUNT(*) FROM memories m {where_clause}");
        let total: i64 = tx.query_row(
            &count_sql,
            rusqlite::params_from_iter(&filter.params),
            |row| row.get(0),
        )?;

        // Data query. Build params in SQL appearance order:
        // 1. Filter params (in WHERE)
        // 2. Limit/offset (at end)
        let mut data_params: Vec<Value> = filter.params;
        data_params.push(Value::Integer(p.limit as i64));
        data_params.push(Value::Integer(p.offset as i64));

        let data_sql = format!(
            "SELECT m.id, m.content, m.type, m.created_at, m.updated_at, \
                    m.archived_at, m.last_accessed_at, m.access_count \
             FROM memories m {where_clause} \
             ORDER BY m.updated_at DESC LIMIT ? OFFSET ?"
        );
        let memories: Vec<Memory> = {
            let mut stmt = tx.prepare(&data_sql)?;
            stmt.query_map(rusqlite::params_from_iter(&data_params), map_memory_row)?
                .collect::<Result<_, _>>()?
        };

        // Fetch projects and tags for the returned memories (within the same transaction).
        let mut memories = memories;
        if !memories.is_empty() {
            let mem_ids: Vec<String> = memories.iter().map(|m| m.id.clone()).collect();
            let mem_id_refs: Vec<&str> = mem_ids.iter().map(|s| s.as_str()).collect();
            fill_projects_and_tags(&tx, &mut memories, &mem_id_refs)?;
        }

        // Apply content truncation in application layer (not SQL SUBSTR) for simpler
        // parameter binding, consistent Unicode scalar value counting, and unit-testability.
        if let Some(max) = p.content_max_length {
            for m in &mut memories {
                m.truncate(max);
            }
        }

        tx.commit()?;
        Ok(ListResult { memories, total })
    }

    // ── merge ────────────────────────────────────────────────────────────

    /// Combine multiple memories into one. Archives sources with "supersedes" links.
    pub fn merge(&self, p: &MergeParams) -> DbResult<MergeResult> {
        let max = self.max_content_size();
        if p.content.len() > max {
            return Err(DbError::ContentTooLarge {
                actual: p.content.len(),
                max,
            });
        }
        if p.source_ids.is_empty() {
            return Err(DbError::InvalidInput {
                message: "source_ids must not be empty".into(),
            });
        }
        let unique_ids: std::collections::HashSet<&str> = p.source_ids.iter().copied().collect();
        if unique_ids.len() != p.source_ids.len() {
            return Err(DbError::InvalidInput {
                message: "duplicate source_ids in merge request".into(),
            });
        }

        let tx = self.conn().unchecked_transaction()?;

        // Verify all sources exist and are not archived.
        for &source_id in p.source_ids {
            let archived_at: Option<Option<String>> = tx
                .query_row(
                    "SELECT archived_at FROM memories WHERE id = ?1",
                    [source_id],
                    |row| row.get(0),
                )
                .optional()?;
            match archived_at {
                None => {
                    return Err(DbError::NotFound {
                        entity: "source memory",
                        id: source_id.to_string(),
                    });
                }
                Some(Some(_)) => {
                    return Err(DbError::AlreadyArchived {
                        id: source_id.to_string(),
                        operation: "merge".into(),
                    });
                }
                Some(None) => {} // active memory, proceed
            }
        }

        // Create the new merged memory.
        let new_id = Uuid::new_v4().to_string();
        let emb_bytes = embedding_to_bytes(p.embedding);

        tx.execute(
            "INSERT INTO memories (id, content, type, embedding) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![new_id, p.content, p.memory_type, emb_bytes],
        )?;

        insert_projects(&tx, &new_id, p.projects)?;
        insert_tags(&tx, &new_id, p.tags)?;

        tx.execute(
            "INSERT INTO memory_embeddings (memory_id, embedding) VALUES (?1, ?2)",
            rusqlite::params![new_id, emb_bytes],
        )?;

        // Archive each source and create a supersedes link.
        let mut archived = Vec::with_capacity(p.source_ids.len());
        for &source_id in p.source_ids {
            tx.execute(
                "UPDATE memories SET archived_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
                 WHERE id = ?1 AND archived_at IS NULL",
                [source_id],
            )?;
            tx.execute(
                "INSERT OR REPLACE INTO tombstones (entity_type, entity_id, action) \
                 VALUES ('memory', ?1, 'archived')",
                [source_id],
            )?;

            let link_id = Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO links (id, source_id, target_id, relation) \
                 VALUES (?1, ?2, ?3, 'supersedes')",
                rusqlite::params![link_id, new_id, source_id],
            )?;

            archived.push(source_id.to_string());
        }

        tx.commit()?;

        Ok(MergeResult {
            id: new_id,
            archived,
        })
    }

    // ── link / unlink ────────────────────────────────────────────────────

    /// Create a directional link between two memories.
    pub fn link(&self, source_id: &str, target_id: &str, relation: &str) -> DbResult<Link> {
        if source_id == target_id {
            return Err(DbError::InvalidInput {
                message: "source and target must be different memories".into(),
            });
        }
        if relation.trim().is_empty() {
            return Err(DbError::InvalidInput {
                message: "relation must not be empty".into(),
            });
        }

        let tx = self.conn().unchecked_transaction()?;

        // Validate both memories exist.
        for (label, mem_id) in [("source memory", source_id), ("target memory", target_id)] {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
                [mem_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(DbError::NotFound {
                    entity: label,
                    id: mem_id.to_string(),
                });
            }
        }

        let id = Uuid::new_v4().to_string();
        let insert_result = tx.execute(
            "INSERT INTO links (id, source_id, target_id, relation) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, source_id, target_id, relation],
        );

        if let Err(rusqlite::Error::SqliteFailure(err, _)) = &insert_result
            && err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
        {
            return Err(DbError::DuplicateLink {
                source_id: source_id.to_string(),
                target_id: target_id.to_string(),
                relation: relation.to_string(),
            });
        }
        insert_result?;

        let created_at: String =
            tx.query_row("SELECT created_at FROM links WHERE id = ?1", [&id], |row| {
                row.get(0)
            })?;

        tx.commit()?;

        Ok(Link {
            id,
            source_id: source_id.to_string(),
            target_id: target_id.to_string(),
            relation: relation.to_string(),
            created_at,
            content: None,
        })
    }

    /// Remove a link by its ID. Creates a tombstone for sync. Returns count removed.
    pub fn unlink_by_id(&self, link_id: &str) -> DbResult<usize> {
        let tx = self.conn().unchecked_transaction()?;

        let removed = tx.execute("DELETE FROM links WHERE id = ?1", [link_id])?;

        if removed == 0 {
            return Err(DbError::NotFound {
                entity: "link",
                id: link_id.to_string(),
            });
        }

        tx.execute(
            "INSERT OR REPLACE INTO tombstones (entity_type, entity_id, action) \
             VALUES ('link', ?1, 'deleted')",
            [link_id],
        )?;

        tx.commit()?;
        Ok(removed)
    }

    /// Remove links matching source, target, and relation.
    /// Creates tombstones for each removed link. Returns count removed.
    pub fn unlink_by_endpoints(
        &self,
        source_id: &str,
        target_id: &str,
        relation: &str,
    ) -> DbResult<usize> {
        let tx = self.conn().unchecked_transaction()?;

        // Collect IDs of links to delete (for tombstones).
        let link_ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM links WHERE source_id = ?1 AND target_id = ?2 AND relation = ?3",
            )?;
            stmt.query_map(rusqlite::params![source_id, target_id, relation], |row| {
                row.get(0)
            })?
            .collect::<Result<_, _>>()?
        };

        if link_ids.is_empty() {
            tx.commit()?;
            return Ok(0);
        }

        // Delete and create tombstones.
        let removed = tx.execute(
            "DELETE FROM links WHERE source_id = ?1 AND target_id = ?2 AND relation = ?3",
            rusqlite::params![source_id, target_id, relation],
        )?;

        for link_id in &link_ids {
            tx.execute(
                "INSERT OR REPLACE INTO tombstones (entity_type, entity_id, action) \
                 VALUES ('link', ?1, 'deleted')",
                [link_id],
            )?;
        }

        tx.commit()?;
        Ok(removed)
    }

    // ── discover ─────────────────────────────────────────────────────────

    /// Aggregate counts for projects, types, tags, relations, and basic stats.
    pub fn discover(&self) -> DbResult<DiscoverResult> {
        let tx = self.conn().unchecked_transaction()?;

        let projects = Self::query_name_counts(
            &tx,
            "SELECT mp.project, COUNT(*) FROM memory_projects mp \
             JOIN memories m ON mp.memory_id = m.id \
             WHERE m.archived_at IS NULL \
             GROUP BY mp.project ORDER BY COUNT(*) DESC",
        )?;

        let types = Self::query_name_counts(
            &tx,
            "SELECT m.type, COUNT(*) FROM memories m \
             WHERE m.type IS NOT NULL AND m.archived_at IS NULL \
             GROUP BY m.type ORDER BY COUNT(*) DESC",
        )?;

        let tags = Self::query_name_counts(
            &tx,
            "SELECT t.tag, COUNT(*) FROM tags t \
             JOIN memories m ON t.memory_id = m.id \
             WHERE m.archived_at IS NULL \
             GROUP BY t.tag ORDER BY COUNT(*) DESC",
        )?;

        // Only require the source to be non-archived. Links to archived memories
        // (e.g. supersedes links from merge) should still appear in discovery so
        // the LLM can learn about existing relation types.
        let relations = Self::query_name_counts(
            &tx,
            "SELECT l.relation, COUNT(*) FROM links l \
             JOIN memories ms ON l.source_id = ms.id \
             WHERE ms.archived_at IS NULL \
             GROUP BY l.relation ORDER BY COUNT(*) DESC",
        )?;

        let total_memories: i64 = tx.query_row(
            "SELECT COUNT(*) FROM memories WHERE archived_at IS NULL",
            [],
            |row: &rusqlite::Row<'_>| row.get(0),
        )?;

        let total_archived: i64 = tx.query_row(
            "SELECT COUNT(*) FROM memories WHERE archived_at IS NOT NULL",
            [],
            |row: &rusqlite::Row<'_>| row.get(0),
        )?;

        // page_count * page_size gives approximate storage size.
        let storage_size_bytes: i64 = tx
            .query_row(
                "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                [],
                |row: &rusqlite::Row<'_>| row.get(0),
            )
            .unwrap_or(0);

        let embedding_model: String = tx
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                ["embedding_model"],
                |row: &rusqlite::Row<'_>| row.get(0),
            )
            .optional()?
            .unwrap_or_default();

        tx.commit()?;

        Ok(DiscoverResult {
            projects,
            types,
            tags,
            relations,
            stats: DbStats {
                total_memories,
                total_archived,
                storage_size_bytes,
                embedding_model,
            },
        })
    }

    // ── status ────────────────────────────────────────────────────────────

    /// Gather database status information for the CLI `status` command.
    pub fn status(&self) -> DbResult<StatusInfo> {
        let tx = self.conn().unchecked_transaction()?;

        let total_memories: i64 = tx.query_row(
            "SELECT COUNT(*) FROM memories WHERE archived_at IS NULL",
            [],
            |row| row.get(0),
        )?;

        let total_archived: i64 = tx.query_row(
            "SELECT COUNT(*) FROM memories WHERE archived_at IS NOT NULL",
            [],
            |row| row.get(0),
        )?;

        let total_links: i64 = tx.query_row("SELECT COUNT(*) FROM links", [], |row| row.get(0))?;

        let embedding_model: String = tx
            .query_row(
                "SELECT value FROM metadata WHERE key = 'embedding_model'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_default();

        let embedding_dimensions: u32 = tx
            .query_row(
                "SELECT value FROM metadata WHERE key = 'embedding_dimensions'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let schema_version: u32 = tx
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let storage_size_bytes: i64 = tx
            .query_row(
                "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        tx.commit()?;

        Ok(StatusInfo {
            stats: DbStats {
                total_memories,
                total_archived,
                storage_size_bytes,
                embedding_model,
            },
            total_links,
            embedding_dimensions,
            schema_version,
        })
    }

    // ── reembed ──────────────────────────────────────────────────────────

    /// Count all non-archived memories (for progress reporting).
    pub fn count_active_memories(&self) -> DbResult<i64> {
        let count = self.conn().query_row(
            "SELECT COUNT(*) FROM memories WHERE archived_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Fetch a batch of (id, content) pairs for re-embedding.
    /// Returns non-archived memories ordered by rowid, paginated by limit/offset.
    pub fn fetch_memory_batch(&self, limit: u32, offset: u32) -> DbResult<Vec<(String, String)>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, content FROM memories WHERE archived_at IS NULL \
             ORDER BY rowid LIMIT ?1 OFFSET ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit, offset], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut batch = Vec::new();
        for row in rows {
            batch.push(row?);
        }
        Ok(batch)
    }

    /// Update only the embedding for a memory (no content change, no updated_at bump).
    /// Used by the reembed command.
    pub fn update_embedding(&self, id: &str, embedding: &[f32]) -> DbResult<()> {
        let emb_bytes = embedding_to_bytes(embedding);
        let tx = self.conn().unchecked_transaction()?;
        let updated = tx.execute(
            "UPDATE memories SET embedding = ?1 WHERE id = ?2 AND archived_at IS NULL",
            rusqlite::params![emb_bytes, id],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound {
                entity: "memory",
                id: id.to_string(),
            });
        }
        tx.execute("DELETE FROM memory_embeddings WHERE memory_id = ?1", [id])?;
        tx.execute(
            "INSERT INTO memory_embeddings (memory_id, embedding) VALUES (?1, ?2)",
            rusqlite::params![id, emb_bytes],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ── helpers ──────────────────────────────────────────────────────────

    fn query_name_counts(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<NameCount>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(NameCount {
                    name: row.get(0)?,
                    count: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
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

    // ── store tests ──────────────────────────────────────────────────────

    #[test]
    fn store_basic() {
        let db = test_db();
        let emb = mock_embedder();
        let embedding = test_embedding(&emb, "test memory");

        let id = db
            .store(&StoreParams {
                content: "test memory",
                memory_type: Some("pattern"),
                projects: &["proj-a"],
                tags: &["rust", "testing"],
                links: &[],
                embedding: &embedding,
            })
            .unwrap();

        assert!(!id.is_empty());

        // Verify via get.
        let results = db.get(&[&id]).unwrap();
        assert_eq!(results.len(), 1);
        let m = &results[0].memory;
        assert_eq!(m.content, "test memory");
        assert_eq!(m.memory_type.as_deref(), Some("pattern"));
        assert_eq!(m.projects, vec!["proj-a"]);
        assert_eq!(m.tags, vec!["rust", "testing"]);
        assert!(m.archived_at.is_none());
    }

    #[test]
    fn store_with_links() {
        let db = test_db();
        let emb = mock_embedder();

        let id1 = db
            .store(&StoreParams {
                content: "first",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "first"),
            })
            .unwrap();

        let id2 = db
            .store(&StoreParams {
                content: "second, related to first",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[(&id1, "related_to")],
                embedding: &test_embedding(&emb, "second"),
            })
            .unwrap();

        let results = db.get(&[&id2]).unwrap();
        assert_eq!(results[0].outgoing_links.len(), 1);
        assert_eq!(results[0].outgoing_links[0].target_id, id1);
        assert_eq!(results[0].outgoing_links[0].relation, "related_to");

        // id1 should have an incoming link.
        let results = db.get(&[&id1]).unwrap();
        assert_eq!(results[0].incoming_links.len(), 1);
        assert_eq!(results[0].incoming_links[0].source_id, id2);
    }

    #[test]
    fn store_minimal() {
        let db = test_db();
        let emb = mock_embedder();
        let embedding = test_embedding(&emb, "just content");

        let id = db
            .store(&StoreParams {
                content: "just content",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &embedding,
            })
            .unwrap();

        let results = db.get(&[&id]).unwrap();
        let m = &results[0].memory;
        assert_eq!(m.content, "just content");
        assert_eq!(m.memory_type, None);
        assert!(m.projects.is_empty());
        assert!(m.tags.is_empty());
    }

    // ── update tests ─────────────────────────────────────────────────────

    #[test]
    fn update_content_and_tags() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "original",
                memory_type: Some("pattern"),
                projects: &["proj"],
                tags: &["old-tag"],
                links: &[],
                embedding: &test_embedding(&emb, "original"),
            })
            .unwrap();

        let new_embedding = test_embedding(&emb, "updated content");
        let result = db
            .update(
                &id,
                &UpdateParams {
                    content: Some("updated content"),
                    memory_type: FieldUpdate::NoChange,
                    projects: None,
                    tags: Some(&["new-tag-a", "new-tag-b"]),
                    embedding: Some(&new_embedding),
                },
            )
            .unwrap();

        assert_eq!(result.id, id);

        let fetched = db.get(&[&id]).unwrap();
        let m = &fetched[0].memory;
        assert_eq!(m.content, "updated content");
        assert_eq!(m.memory_type.as_deref(), Some("pattern")); // unchanged
        assert_eq!(m.projects, vec!["proj"]); // unchanged
        assert_eq!(m.tags, vec!["new-tag-a", "new-tag-b"]); // replaced
    }

    #[test]
    fn update_clear_memory_type() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "typed memory",
                memory_type: Some("pattern"),
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "typed memory"),
            })
            .unwrap();

        // Verify type is set.
        let fetched = db.get(&[&id]).unwrap();
        assert_eq!(fetched[0].memory.memory_type.as_deref(), Some("pattern"));

        // Clear the type using FieldUpdate::Clear.
        db.update(
            &id,
            &UpdateParams {
                content: None,
                memory_type: FieldUpdate::Clear,
                projects: None,
                tags: None,
                embedding: None,
            },
        )
        .unwrap();

        let fetched = db.get(&[&id]).unwrap();
        assert_eq!(fetched[0].memory.memory_type, None); // cleared
    }

    #[test]
    fn update_set_memory_type() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "typed memory",
                memory_type: Some("pattern"),
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "typed memory"),
            })
            .unwrap();

        // Change type from "pattern" to "decision".
        db.update(
            &id,
            &UpdateParams {
                content: None,
                memory_type: FieldUpdate::Set("decision"),
                projects: None,
                tags: None,
                embedding: None,
            },
        )
        .unwrap();

        let fetched = db.get(&[&id]).unwrap();
        assert_eq!(fetched[0].memory.memory_type.as_deref(), Some("decision"));
    }

    #[test]
    fn update_nonexistent_fails() {
        let db = test_db();
        let err = db
            .update(
                "nonexistent-id",
                &UpdateParams {
                    content: None,
                    memory_type: FieldUpdate::Set("x"),
                    projects: None,
                    tags: None,
                    embedding: None,
                },
            )
            .unwrap_err();
        assert!(
            matches!(err, DbError::NotFound { entity: "memory", ref id, .. } if id == "nonexistent-id")
        );
    }

    #[test]
    fn update_no_changes_is_noop() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "stable",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "stable"),
            })
            .unwrap();

        let before = db.get(&[&id]).unwrap()[0].memory.updated_at.clone();

        let result = db
            .update(
                &id,
                &UpdateParams {
                    content: None,
                    memory_type: FieldUpdate::NoChange,
                    projects: None,
                    tags: None,
                    embedding: None,
                },
            )
            .unwrap();

        assert_eq!(result.updated_at, before);
    }

    #[test]
    fn update_content_without_embedding_fails() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "original",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "original"),
            })
            .unwrap();

        let err = db
            .update(
                &id,
                &UpdateParams {
                    content: Some("changed content"),
                    memory_type: FieldUpdate::NoChange,
                    projects: None,
                    tags: None,
                    embedding: None, // missing! should fail
                },
            )
            .unwrap_err();
        assert!(matches!(err, DbError::InvalidInput { .. }));
        assert!(err.to_string().contains("embedding is required"));
    }

    #[test]
    fn update_embedding_only() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "content",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "content"),
            })
            .unwrap();

        let before = db.get(&[&id]).unwrap()[0].memory.updated_at.clone();

        // Updating only the embedding (e.g. after reembed) should work
        // but should NOT bump updated_at (reembed is not a content change).
        let new_emb = test_embedding(&emb, "different embedding");
        let result = db
            .update(
                &id,
                &UpdateParams {
                    content: None,
                    memory_type: FieldUpdate::NoChange,
                    projects: None,
                    tags: None,
                    embedding: Some(&new_emb),
                },
            )
            .unwrap();

        // updated_at should NOT change for embedding-only updates.
        assert_eq!(result.updated_at, before);
    }

    #[test]
    fn update_archived_memory_fails() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "will archive",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "will archive"),
            })
            .unwrap();

        db.archive(&id).unwrap();

        // Attempting to update an archived memory should fail.
        let err = db
            .update(
                &id,
                &UpdateParams {
                    content: None,
                    memory_type: FieldUpdate::Set("changed"),
                    projects: None,
                    tags: None,
                    embedding: None,
                },
            )
            .unwrap_err();
        assert!(matches!(err, DbError::AlreadyArchived { id: ref err_id, .. } if err_id == &id));
    }

    // ── archive tests ────────────────────────────────────────────────────

    #[test]
    fn archive_sets_timestamp() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "to archive",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "to archive"),
            })
            .unwrap();

        let result = db.archive(&id).unwrap();
        assert_eq!(result.id, id);
        assert!(!result.archived_at.is_empty());

        let fetched = db.get(&[&id]).unwrap();
        assert!(fetched[0].memory.archived_at.is_some());
    }

    #[test]
    fn archive_already_archived_fails() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "will archive twice",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "will archive twice"),
            })
            .unwrap();

        db.archive(&id).unwrap();
        let err = db.archive(&id).unwrap_err();
        assert!(matches!(err, DbError::AlreadyArchived { id: ref err_id, .. } if err_id == &id));
    }

    #[test]
    fn archive_creates_tombstone() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "tombstone test",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "tombstone test"),
            })
            .unwrap();

        db.archive(&id).unwrap();

        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM tombstones WHERE entity_type = 'memory' AND entity_id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    // ── unarchive tests ─────────────────────────────────────────────────

    #[test]
    fn unarchive_restores_archived_memory() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "to unarchive",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "to unarchive"),
            })
            .unwrap();

        db.archive(&id).unwrap();

        // Verify it's archived
        let fetched = db.get(&[&id]).unwrap();
        assert!(fetched[0].memory.archived_at.is_some());

        let result = db.unarchive(&id).unwrap();
        assert_eq!(result.id, id);
        assert!(!result.updated_at.is_empty());

        // Verify archived_at is cleared
        let fetched = db.get(&[&id]).unwrap();
        assert!(fetched[0].memory.archived_at.is_none());
    }

    #[test]
    fn unarchive_active_memory_fails() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "active memory",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "active memory"),
            })
            .unwrap();

        let err = db.unarchive(&id).unwrap_err();
        assert!(matches!(err, DbError::NotArchived { id: ref err_id } if err_id == &id));
    }

    #[test]
    fn unarchive_creates_tombstone() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "tombstone unarchive test",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "tombstone unarchive test"),
            })
            .unwrap();

        db.archive(&id).unwrap();
        db.unarchive(&id).unwrap();

        let action: String = db
            .conn()
            .query_row(
                "SELECT action FROM tombstones WHERE entity_type = 'memory' AND entity_id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(action, "unarchived");
    }

    #[test]
    fn unarchive_not_found() {
        let db = test_db();
        let err = db.unarchive("nonexistent-id").unwrap_err();
        assert!(
            matches!(err, DbError::NotFound { entity: "memory", ref id } if id == "nonexistent-id")
        );
    }

    // ── get tests ────────────────────────────────────────────────────────

    #[test]
    fn get_empty_ids() {
        let db = test_db();
        let results = db.get(&[]).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn get_missing_id_fails() {
        let db = test_db();
        let err = db.get(&["nonexistent"]).unwrap_err();
        assert!(
            matches!(err, DbError::NotFound { entity: "memory", ref id, .. } if id == "nonexistent")
        );
    }

    #[test]
    fn get_multiple() {
        let db = test_db();
        let emb = mock_embedder();

        let id1 = db
            .store(&StoreParams {
                content: "mem one",
                memory_type: None,
                projects: &["proj"],
                tags: &["a"],
                links: &[],
                embedding: &test_embedding(&emb, "mem one"),
            })
            .unwrap();

        let id2 = db
            .store(&StoreParams {
                content: "mem two",
                memory_type: Some("decision"),
                projects: &[],
                tags: &["b"],
                links: &[],
                embedding: &test_embedding(&emb, "mem two"),
            })
            .unwrap();

        let results = db.get(&[&id1, &id2]).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn get_preserves_request_order() {
        let db = test_db();
        let emb = mock_embedder();

        let id1 = db
            .store(&StoreParams {
                content: "first memory",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "first memory"),
            })
            .unwrap();

        let id2 = db
            .store(&StoreParams {
                content: "second memory",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "second memory"),
            })
            .unwrap();

        // Request in reverse order — results must match requested order, not insertion order.
        let results = db.get(&[&id2, &id1]).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].memory.id, id2);
        assert_eq!(results[1].memory.id, id1);
    }

    #[test]
    fn get_populates_link_content_snippets() {
        let db = test_db();
        let emb = mock_embedder();

        // Store two memories and link them.
        let id1 = db
            .store(&StoreParams {
                content: "First memory about Rust error handling",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "first"),
            })
            .unwrap();

        let id2 = db
            .store(&StoreParams {
                content: "Second memory referencing the first",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[(&id1, "related_to")],
                embedding: &test_embedding(&emb, "second"),
            })
            .unwrap();

        // get() on id2 should populate the outgoing link's content with id1's content.
        let results = db.get(&[&id2]).unwrap();
        assert_eq!(results[0].outgoing_links.len(), 1);
        let outgoing = &results[0].outgoing_links[0];
        assert_eq!(
            outgoing.content.as_deref(),
            Some("First memory about Rust error handling"),
            "outgoing link should have content snippet populated"
        );

        // get() on id1 should populate the incoming link's content with id2's content.
        let results = db.get(&[&id1]).unwrap();
        assert_eq!(results[0].incoming_links.len(), 1);
        let incoming = &results[0].incoming_links[0];
        assert_eq!(
            incoming.content.as_deref(),
            Some("Second memory referencing the first"),
            "incoming link should have content snippet populated"
        );
    }

    #[test]
    fn get_link_content_snippets_truncated_to_100_chars() {
        let db = test_db();
        let emb = mock_embedder();

        // Create a memory with content longer than 100 characters.
        let long_content = "A".repeat(200);
        let id1 = db
            .store(&StoreParams {
                content: &long_content,
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "long"),
            })
            .unwrap();

        let id2 = db
            .store(&StoreParams {
                content: "short",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[(&id1, "related_to")],
                embedding: &test_embedding(&emb, "short"),
            })
            .unwrap();

        let results = db.get(&[&id2]).unwrap();
        let snippet = results[0].outgoing_links[0].content.as_ref().unwrap();
        assert_eq!(
            snippet.chars().count(),
            100,
            "content snippet should be truncated to 100 characters, got {}",
            snippet.chars().count()
        );
        let expected = format!("{}...", &"A".repeat(97));
        assert_eq!(
            snippet, &expected,
            "truncated snippet should end with ellipsis"
        );
    }

    // ── list tests ───────────────────────────────────────────────────────

    #[test]
    fn list_default_excludes_archived() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "will archive",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "will archive"),
            })
            .unwrap();

        db.store(&StoreParams {
            content: "stays active",
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, "stays active"),
        })
        .unwrap();

        db.archive(&id).unwrap();

        let result = db.list(&ListParams::default()).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.memories[0].content, "stays active");
    }

    #[test]
    fn list_include_archived() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "archived one",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "archived one"),
            })
            .unwrap();

        db.archive(&id).unwrap();

        let result = db
            .list(&ListParams {
                filter: FilterParams {
                    include_archived: true,
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.total, 1);
    }

    #[test]
    fn list_filter_by_project() {
        let db = test_db();
        let emb = mock_embedder();

        db.store(&StoreParams {
            content: "in proj-a",
            memory_type: None,
            projects: &["proj-a"],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, "in proj-a"),
        })
        .unwrap();

        db.store(&StoreParams {
            content: "in proj-b",
            memory_type: None,
            projects: &["proj-b"],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, "in proj-b"),
        })
        .unwrap();

        db.store(&StoreParams {
            content: "global (no project)",
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, "global"),
        })
        .unwrap();

        // Filter by proj-a, include_global = true (default).
        let result = db
            .list(&ListParams {
                filter: FilterParams {
                    projects: Some(&["proj-a"]),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.total, 2); // proj-a + global

        // Filter by proj-a, include_global = false.
        let result = db
            .list(&ListParams {
                filter: FilterParams {
                    projects: Some(&["proj-a"]),
                    include_global: false,
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.memories[0].content, "in proj-a");
    }

    #[test]
    fn list_filter_by_tags_and() {
        let db = test_db();
        let emb = mock_embedder();

        db.store(&StoreParams {
            content: "has both tags",
            memory_type: None,
            projects: &[],
            tags: &["rust", "testing"],
            links: &[],
            embedding: &test_embedding(&emb, "both"),
        })
        .unwrap();

        db.store(&StoreParams {
            content: "only rust",
            memory_type: None,
            projects: &[],
            tags: &["rust"],
            links: &[],
            embedding: &test_embedding(&emb, "only rust"),
        })
        .unwrap();

        let result = db
            .list(&ListParams {
                filter: FilterParams {
                    tags: Some(&["rust", "testing"]),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.memories[0].content, "has both tags");
    }

    #[test]
    fn list_filter_by_type() {
        let db = test_db();
        let emb = mock_embedder();

        db.store(&StoreParams {
            content: "a pattern",
            memory_type: Some("pattern"),
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, "a pattern"),
        })
        .unwrap();

        db.store(&StoreParams {
            content: "a decision",
            memory_type: Some("decision"),
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, "a decision"),
        })
        .unwrap();

        let result = db
            .list(&ListParams {
                filter: FilterParams {
                    memory_type: Some("pattern"),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.memories[0].content, "a pattern");
    }

    #[test]
    fn list_pagination() {
        let db = test_db();
        let emb = mock_embedder();

        for i in 0..5 {
            let content = format!("memory {i}");
            db.store(&StoreParams {
                content: &content,
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, &content),
            })
            .unwrap();
        }

        let result = db
            .list(&ListParams {
                limit: 2,
                offset: 0,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.total, 5);
        assert_eq!(result.memories.len(), 2);

        let result = db
            .list(&ListParams {
                limit: 2,
                offset: 4,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.memories.len(), 1);
    }

    #[test]
    fn list_content_truncation() {
        let db = test_db();
        let emb = mock_embedder();

        let long_content = "a".repeat(1000);
        db.store(&StoreParams {
            content: &long_content,
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, "long"),
        })
        .unwrap();

        let result = db
            .list(&ListParams {
                content_max_length: Some(100),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.memories[0].content.len(), 100);
    }

    #[test]
    fn list_time_filters_created_after_and_before() {
        let db = test_db();
        let emb = mock_embedder();

        let old_id = db
            .store(&StoreParams {
                content: "old memory",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "old memory"),
            })
            .unwrap();

        let new_id = db
            .store(&StoreParams {
                content: "new memory",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "new memory"),
            })
            .unwrap();

        // Backdate old memory.
        db.conn()
            .execute(
                "UPDATE memories SET created_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1",
                [&old_id],
            )
            .unwrap();

        // created_after = 2025 should only return the new memory.
        let result = db
            .list(&ListParams {
                filter: FilterParams {
                    time: TimeFilters {
                        created_after: Some("2025-01-01T00:00:00.000Z"),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.memories[0].id, new_id);

        // created_before = 2021 should only return the old memory.
        let result = db
            .list(&ListParams {
                filter: FilterParams {
                    time: TimeFilters {
                        created_before: Some("2021-01-01T00:00:00.000Z"),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.memories[0].id, old_id);
    }

    #[test]
    fn list_time_filters_updated_after_and_before() {
        let db = test_db();
        let emb = mock_embedder();

        let id1 = db
            .store(&StoreParams {
                content: "stale memory",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "stale memory"),
            })
            .unwrap();

        let id2 = db
            .store(&StoreParams {
                content: "fresh memory",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "fresh memory"),
            })
            .unwrap();

        // Set id1 updated_at to the past.
        db.conn()
            .execute(
                "UPDATE memories SET updated_at = '2020-06-01T00:00:00.000Z' WHERE id = ?1",
                [&id1],
            )
            .unwrap();

        // updated_after = 2025 should only return id2.
        let result = db
            .list(&ListParams {
                filter: FilterParams {
                    time: TimeFilters {
                        updated_after: Some("2025-01-01T00:00:00.000Z"),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.memories[0].id, id2);

        // updated_before = 2021 should only return id1.
        let result = db
            .list(&ListParams {
                filter: FilterParams {
                    time: TimeFilters {
                        updated_before: Some("2021-01-01T00:00:00.000Z"),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.memories[0].id, id1);
    }

    // ── merge tests ──────────────────────────────────────────────────────

    #[test]
    fn merge_archives_sources_and_creates_links() {
        let db = test_db();
        let emb = mock_embedder();

        let id1 = db
            .store(&StoreParams {
                content: "old memory 1",
                memory_type: Some("pattern"),
                projects: &["proj"],
                tags: &["rust"],
                links: &[],
                embedding: &test_embedding(&emb, "old 1"),
            })
            .unwrap();

        let id2 = db
            .store(&StoreParams {
                content: "old memory 2",
                memory_type: Some("pattern"),
                projects: &["proj"],
                tags: &["rust"],
                links: &[],
                embedding: &test_embedding(&emb, "old 2"),
            })
            .unwrap();

        let result = db
            .merge(&MergeParams {
                source_ids: &[&id1, &id2],
                content: "merged memory combining 1 and 2",
                memory_type: Some("pattern"),
                projects: &["proj"],
                tags: &["rust", "merged"],
                embedding: &test_embedding(&emb, "merged"),
            })
            .unwrap();

        assert!(!result.id.is_empty());
        assert_eq!(result.archived.len(), 2);

        // Sources should be archived.
        let sources = db.get(&[&id1, &id2]).unwrap();
        assert!(sources[0].memory.archived_at.is_some());
        assert!(sources[1].memory.archived_at.is_some());

        // New memory should have supersedes links.
        let merged = db.get(&[&result.id]).unwrap();
        assert_eq!(merged[0].outgoing_links.len(), 2);
        assert!(
            merged[0]
                .outgoing_links
                .iter()
                .all(|l| l.relation == "supersedes")
        );
    }

    #[test]
    fn merge_empty_sources_fails() {
        let db = test_db();
        let emb = mock_embedder();
        let err = db
            .merge(&MergeParams {
                source_ids: &[],
                content: "merged",
                memory_type: None,
                projects: &[],
                tags: &[],
                embedding: &test_embedding(&emb, "merged"),
            })
            .unwrap_err();
        assert!(matches!(err, DbError::InvalidInput { .. }));
        assert!(err.to_string().contains("source_ids must not be empty"));
    }

    // ── link / unlink tests ──────────────────────────────────────────────

    #[test]
    fn link_and_unlink_by_id() {
        let db = test_db();
        let emb = mock_embedder();

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
        assert_eq!(link.source_id, id1);
        assert_eq!(link.target_id, id2);
        assert_eq!(link.relation, "related_to");

        let removed = db.unlink_by_id(&link.id).unwrap();
        assert_eq!(removed, 1);

        // Verify link is gone.
        let fetched = db.get(&[&id1]).unwrap();
        assert!(fetched[0].outgoing_links.is_empty());
    }

    #[test]
    fn unlink_by_endpoints() {
        let db = test_db();
        let emb = mock_embedder();

        let id1 = db
            .store(&StoreParams {
                content: "src",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "src"),
            })
            .unwrap();

        let id2 = db
            .store(&StoreParams {
                content: "tgt",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "tgt"),
            })
            .unwrap();

        db.link(&id1, &id2, "caused_by").unwrap();
        let removed = db.unlink_by_endpoints(&id1, &id2, "caused_by").unwrap();
        assert_eq!(removed, 1);
    }

    #[test]
    fn unlink_creates_tombstone() {
        let db = test_db();
        let emb = mock_embedder();

        let id1 = db
            .store(&StoreParams {
                content: "s",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "s"),
            })
            .unwrap();

        let id2 = db
            .store(&StoreParams {
                content: "t",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "t"),
            })
            .unwrap();

        let link = db.link(&id1, &id2, "related_to").unwrap();
        db.unlink_by_id(&link.id).unwrap();

        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM tombstones WHERE entity_type = 'link' AND entity_id = ?1",
                [&link.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    // ── discover tests ───────────────────────────────────────────────────

    #[test]
    fn discover_aggregates_correctly() {
        let db = test_db();
        let emb = mock_embedder();

        db.store(&StoreParams {
            content: "mem 1",
            memory_type: Some("pattern"),
            projects: &["proj-a", "proj-b"],
            tags: &["rust"],
            links: &[],
            embedding: &test_embedding(&emb, "mem 1"),
        })
        .unwrap();

        db.store(&StoreParams {
            content: "mem 2",
            memory_type: Some("decision"),
            projects: &["proj-a"],
            tags: &["rust", "testing"],
            links: &[],
            embedding: &test_embedding(&emb, "mem 2"),
        })
        .unwrap();

        let result = db.discover().unwrap();

        // Projects: proj-a=2, proj-b=1
        assert_eq!(result.projects.len(), 2);
        assert_eq!(result.projects[0].name, "proj-a");
        assert_eq!(result.projects[0].count, 2);

        // Types: pattern=1, decision=1
        assert_eq!(result.types.len(), 2);

        // Tags: rust=2, testing=1
        assert_eq!(result.tags.len(), 2);
        assert_eq!(result.tags[0].name, "rust");
        assert_eq!(result.tags[0].count, 2);

        // Stats.
        assert_eq!(result.stats.total_memories, 2);
        assert_eq!(result.stats.total_archived, 0);
        assert_eq!(result.stats.embedding_model, "NomicEmbedTextV15Q");
    }

    #[test]
    fn discover_excludes_archived_from_counts() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "will archive",
                memory_type: Some("pattern"),
                projects: &["proj"],
                tags: &["tag"],
                links: &[],
                embedding: &test_embedding(&emb, "will archive"),
            })
            .unwrap();

        db.archive(&id).unwrap();

        let result = db.discover().unwrap();
        assert!(result.projects.is_empty());
        assert!(result.types.is_empty());
        assert!(result.tags.is_empty());
        assert_eq!(result.stats.total_memories, 0);
        assert_eq!(result.stats.total_archived, 1);
    }

    // ── content size validation tests ────────────────────────────────────

    fn small_content_db() -> Database {
        let config = DbConfig {
            max_content_size: 20,
            ..DbConfig::default()
        };
        Database::open_in_memory(&config).unwrap()
    }

    #[test]
    fn store_rejects_oversized_content() {
        let db = small_content_db();
        let emb = mock_embedder();
        let big = "x".repeat(21);
        let embedding = test_embedding(&emb, &big);

        let err = db
            .store(&StoreParams {
                content: &big,
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &embedding,
            })
            .unwrap_err();
        assert!(matches!(
            err,
            DbError::ContentTooLarge {
                actual: 21,
                max: 20
            }
        ));
    }

    #[test]
    fn store_accepts_content_at_limit() {
        let db = small_content_db();
        let emb = mock_embedder();
        let exact = "x".repeat(20);
        let embedding = test_embedding(&emb, &exact);

        db.store(&StoreParams {
            content: &exact,
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &embedding,
        })
        .unwrap();
    }

    #[test]
    fn update_rejects_oversized_content() {
        let db = small_content_db();
        let emb = mock_embedder();
        let embedding = test_embedding(&emb, "ok");

        let id = db
            .store(&StoreParams {
                content: "ok",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &embedding,
            })
            .unwrap();

        let big = "x".repeat(21);
        let big_embedding = test_embedding(&emb, &big);

        let err = db
            .update(
                &id,
                &UpdateParams {
                    content: Some(&big),
                    memory_type: FieldUpdate::NoChange,
                    projects: None,
                    tags: None,
                    embedding: Some(&big_embedding),
                },
            )
            .unwrap_err();
        assert!(matches!(
            err,
            DbError::ContentTooLarge {
                actual: 21,
                max: 20
            }
        ));
    }

    #[test]
    fn update_allows_non_content_changes_regardless_of_size() {
        let db = small_content_db();
        let emb = mock_embedder();
        let embedding = test_embedding(&emb, "ok");

        let id = db
            .store(&StoreParams {
                content: "ok",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &embedding,
            })
            .unwrap();

        // Tag-only update should not trigger content size check.
        db.update(
            &id,
            &UpdateParams {
                content: None,
                memory_type: FieldUpdate::NoChange,
                projects: None,
                tags: Some(&["new-tag"]),
                embedding: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn merge_rejects_oversized_content() {
        let db = small_content_db();
        let emb = mock_embedder();
        let embedding = test_embedding(&emb, "a");

        let id = db
            .store(&StoreParams {
                content: "a",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &embedding,
            })
            .unwrap();

        let big = "x".repeat(21);
        let big_embedding = test_embedding(&emb, &big);

        let err = db
            .merge(&MergeParams {
                source_ids: &[&id],
                content: &big,
                memory_type: None,
                projects: &[],
                tags: &[],
                embedding: &big_embedding,
            })
            .unwrap_err();
        assert!(matches!(
            err,
            DbError::ContentTooLarge {
                actual: 21,
                max: 20
            }
        ));
    }

    // ── status tests ─────────────────────────────────────────────────────

    #[test]
    fn status_empty_db() {
        let db = test_db();
        let info = db.status().unwrap();
        assert_eq!(info.stats.total_memories, 0);
        assert_eq!(info.stats.total_archived, 0);
        assert_eq!(info.total_links, 0);
    }

    #[test]
    fn status_counts_active_and_archived() {
        let db = test_db();
        let emb = mock_embedder();

        // Store 3 memories.
        let ids: Vec<String> = (0..3)
            .map(|i| {
                let content = format!("memory {i}");
                db.store(&StoreParams {
                    content: &content,
                    memory_type: None,
                    projects: &[],
                    tags: &[],
                    links: &[],
                    embedding: &test_embedding(&emb, &content),
                })
                .unwrap()
            })
            .collect();

        // Archive one.
        db.archive(&ids[0]).unwrap();

        let info = db.status().unwrap();
        assert_eq!(info.stats.total_memories, 2);
        assert_eq!(info.stats.total_archived, 1);
    }

    #[test]
    fn status_counts_links() {
        let db = test_db();
        let emb = mock_embedder();

        let id1 = db
            .store(&StoreParams {
                content: "first",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "first"),
            })
            .unwrap();

        let _id2 = db
            .store(&StoreParams {
                content: "second",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[(&id1, "related_to")],
                embedding: &test_embedding(&emb, "second"),
            })
            .unwrap();

        let info = db.status().unwrap();
        assert_eq!(info.total_links, 1);
    }

    #[test]
    fn status_returns_metadata() {
        let db = test_db();
        let info = db.status().unwrap();
        assert_eq!(info.stats.embedding_model, "NomicEmbedTextV15Q");
        assert_eq!(info.embedding_dimensions, 768);
        assert_eq!(info.schema_version, 2);
        assert!(info.stats.storage_size_bytes >= 0);
    }

    #[test]
    fn link_duplicate_returns_error() {
        let db = test_db();
        let emb = mock_embedder();

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

        // First link succeeds.
        db.link(&id1, &id2, "related_to").unwrap();

        // Second identical link should return DuplicateLink error.
        let err = db.link(&id1, &id2, "related_to").unwrap_err();
        match err {
            DbError::DuplicateLink {
                source_id,
                target_id,
                relation,
            } => {
                assert_eq!(source_id, id1);
                assert_eq!(target_id, id2);
                assert_eq!(relation, "related_to");
            }
            other => panic!("expected DuplicateLink, got: {other:?}"),
        }
    }

    #[test]
    fn link_same_endpoints_different_relation_succeeds() {
        let db = test_db();
        let emb = mock_embedder();

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

        db.link(&id1, &id2, "related_to").unwrap();
        // Different relation on same endpoints should succeed.
        let link2 = db.link(&id1, &id2, "caused_by").unwrap();
        assert_eq!(link2.relation, "caused_by");
    }

    #[test]
    fn link_same_source_relation_different_target_succeeds() {
        let db = test_db();
        let emb = mock_embedder();

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
                content: "target-a",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "target-a"),
            })
            .unwrap();

        let id3 = db
            .store(&StoreParams {
                content: "target-b",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "target-b"),
            })
            .unwrap();

        db.link(&id1, &id2, "related_to").unwrap();
        // Same source + relation but different target should succeed.
        let link2 = db.link(&id1, &id3, "related_to").unwrap();
        assert_eq!(link2.target_id, id3);
    }

    #[test]
    fn store_deduplicates_links() {
        let db = test_db();
        let emb = mock_embedder();

        // Create a target memory first.
        let target_id = db
            .store(&StoreParams {
                content: "target",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "target"),
            })
            .unwrap();

        // Store a memory with duplicate links — same (target, relation) twice.
        let source_id = db
            .store(&StoreParams {
                content: "source with dup links",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[
                    (&target_id, "related_to"),
                    (&target_id, "related_to"), // duplicate
                ],
                embedding: &test_embedding(&emb, "source with dup links"),
            })
            .unwrap();

        // Should have exactly one link, not two.
        let results = db.get(&[&source_id]).unwrap();
        assert_eq!(results[0].outgoing_links.len(), 1);
    }

    #[test]
    fn schema_has_unique_index_on_links() {
        let db = test_db();
        let index_exists: bool = db
            .conn()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type = 'index' AND name = 'idx_links_unique' \
                 AND sql LIKE '%UNIQUE%')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            index_exists,
            "idx_links_unique should exist as a UNIQUE index"
        );
    }

    // ── reembed tests ───────────────────────────────────────────────────

    #[test]
    fn count_active_memories_excludes_archived() {
        let db = test_db();
        let emb = mock_embedder();

        // Store two memories.
        let id1 = db
            .store(&StoreParams {
                content: "memory one",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "memory one"),
            })
            .unwrap();
        db.store(&StoreParams {
            content: "memory two",
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, "memory two"),
        })
        .unwrap();

        assert_eq!(db.count_active_memories().unwrap(), 2);

        db.archive(&id1).unwrap();
        assert_eq!(db.count_active_memories().unwrap(), 1);
    }

    #[test]
    fn fetch_memory_batch_pagination() {
        let db = test_db();
        let emb = mock_embedder();

        // Store 5 memories.
        for i in 0..5 {
            let content = format!("memory {i}");
            db.store(&StoreParams {
                content: &content,
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, &content),
            })
            .unwrap();
        }

        // Fetch in batches of 2.
        let batch1 = db.fetch_memory_batch(2, 0).unwrap();
        assert_eq!(batch1.len(), 2);

        let batch2 = db.fetch_memory_batch(2, 2).unwrap();
        assert_eq!(batch2.len(), 2);

        let batch3 = db.fetch_memory_batch(2, 4).unwrap();
        assert_eq!(batch3.len(), 1);

        let batch4 = db.fetch_memory_batch(2, 6).unwrap();
        assert!(batch4.is_empty());
    }

    #[test]
    fn fetch_memory_batch_excludes_archived() {
        let db = test_db();
        let emb = mock_embedder();

        let id = db
            .store(&StoreParams {
                content: "will be archived",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "will be archived"),
            })
            .unwrap();
        db.store(&StoreParams {
            content: "stays active",
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, "stays active"),
        })
        .unwrap();

        db.archive(&id).unwrap();

        let batch = db.fetch_memory_batch(100, 0).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].1, "stays active");
    }

    #[test]
    fn update_embedding_replaces_vector() {
        let db = test_db();
        let emb = mock_embedder();
        let original_embedding = test_embedding(&emb, "original");

        let id = db
            .store(&StoreParams {
                content: "original content",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &original_embedding,
            })
            .unwrap();

        // Re-embed with a different vector.
        let new_embedding = test_embedding(&emb, "completely different text");
        db.update_embedding(&id, &new_embedding).unwrap();

        // Verify the BLOB was updated.
        let stored_blob: Vec<u8> = db
            .conn()
            .query_row(
                "SELECT embedding FROM memories WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        let expected_bytes = embedding_to_bytes(&new_embedding);
        assert_eq!(stored_blob, expected_bytes);

        // Verify vec0 was updated (search should find it with new embedding).
        let results = db.find_similar(&new_embedding, 1, &[], None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.id, id);
    }

    #[test]
    fn update_embedding_not_found() {
        let db = test_db();
        let emb = mock_embedder();
        let embedding = test_embedding(&emb, "test");
        let result = db.update_embedding("nonexistent", &embedding);
        assert!(result.is_err());
    }

    #[test]
    fn update_embedding_skips_archived() {
        let db = test_db();
        let emb = mock_embedder();
        let embedding = test_embedding(&emb, "test");

        let id = db
            .store(&StoreParams {
                content: "will be archived",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &embedding,
            })
            .unwrap();
        db.archive(&id).unwrap();

        let new_embedding = test_embedding(&emb, "new");
        let result = db.update_embedding(&id, &new_embedding);
        assert!(result.is_err());
    }
}
