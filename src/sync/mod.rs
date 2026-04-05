//! JSONL export/import and filesystem-based sync.

pub mod background;

use std::io::{BufRead, BufReader, Read, Write};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::db::Database;
use crate::db::types::{ImportAction, ImportMemoryParams, Link, ReconcileDecision, Tombstone};

// ── Sync record types ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
pub enum SyncRecord {
    Memory(MemoryRecord),
    Link(Link),
    Tombstone(Tombstone),
}

/// Sync wire format for memories. Separate from `db::types::Memory` because
/// the wire format excludes local-only fields (last_accessed_at, access_count,
/// truncated) that are not synced between machines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: String,
    pub content: String,
    #[serde(rename = "type")]
    pub memory_type: Option<String>,
    pub projects: Vec<String>,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub archived_at: Option<String>,
}

// ── Export/Import options and stats ──────────────────────────────────

pub struct ExportOptions {
    pub since: Option<String>,
    pub tombstone_retention_days: u32,
    /// If true, purge old tombstones from DB after export.
    /// Should be true for sync cycles, false for standalone export.
    pub purge: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            since: None,
            tombstone_retention_days: 90,
            purge: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct ImportStats {
    pub memories_inserted: usize,
    pub memories_updated: usize,
    pub memories_skipped: usize,
    pub links_inserted: usize,
    pub links_skipped: usize,
    pub tombstones_applied: usize,
    pub tombstones_skipped: usize,
}

// ── Export ───────────────────────────────────────────────────────────

/// Export memories, links, and tombstones to a JSONL writer.
/// Returns the number of records written.
pub fn export(db: &Database, writer: &mut dyn Write, opts: &ExportOptions) -> Result<usize> {
    let mut count = 0;

    // Export memories.
    let memories = db.export_memories(opts.since.as_deref())?;
    for m in &memories {
        let record = SyncRecord::Memory(MemoryRecord {
            id: m.id.clone(),
            content: m.content.clone(),
            memory_type: m.memory_type.clone(),
            projects: m.projects.clone(),
            tags: m.tags.clone(),
            created_at: m.created_at.clone(),
            updated_at: m.updated_at.clone(),
            archived_at: m.archived_at.clone(),
        });
        serde_json::to_writer(&mut *writer, &record)?;
        writer.write_all(b"\n")?;
        count += 1;
    }

    // Export links.
    let links = db.export_links(opts.since.as_deref())?;
    for l in &links {
        let record = SyncRecord::Link(l.clone());
        serde_json::to_writer(&mut *writer, &record)?;
        writer.write_all(b"\n")?;
        count += 1;
    }

    // Export tombstones (within retention window).
    let tombstones = db.export_tombstones(opts.tombstone_retention_days)?;
    for t in &tombstones {
        let record = SyncRecord::Tombstone(t.clone());
        serde_json::to_writer(&mut *writer, &record)?;
        writer.write_all(b"\n")?;
        count += 1;
    }

    // Purge old tombstones from DB if requested (sync cycles, not standalone export).
    if opts.purge {
        db.purge_old_tombstones(opts.tombstone_retention_days)?;
    }

    Ok(count)
}

// ── Import ──────────────────────────────────────────────────────────

/// Import memories, links, and tombstones from a JSONL reader.
/// Expects plain-text JSONL — callers must handle gzip decompression if needed.
/// See `background::import_from_file` for a file-based wrapper with auto-detection.
///
/// Uses a multi-phase pipeline to enable batch embedding (significantly faster than
/// per-record embedding). Phase 1 reconciliation filters skips before the expensive
/// embedding step. Phase 3 uses atomic import_memory() which re-validates inside
/// its transaction, so concurrent modifications are handled safely (at worst, a
/// memory that was embedded turns out to be skipped — wasted compute, not data loss).
pub fn import<F>(db: &Database, embed_batch: F, reader: &mut dyn Read) -> Result<ImportStats>
where
    F: Fn(&[&str]) -> Result<Vec<Vec<f32>>>,
{
    let mut stats = ImportStats::default();
    let buf_reader = BufReader::new(reader);
    let mut pending_memories: Vec<MemoryRecord> = Vec::new();
    let mut deferred_links: Vec<Link> = Vec::new();
    let mut deferred_tombstones: Vec<Tombstone> = Vec::new();

    // Phase 1: Parse + reconcile (collect all records, defer writes)
    // NOTE: Records are processed by type (memories, then links, then tombstones),
    // not in stream order. This matches the export format which writes memories first.
    // JSONL from other sources must follow the same ordering assumption.
    for (line_num, line_result) in buf_reader.lines().enumerate() {
        let line = line_result.with_context(|| format!("failed to read line {}", line_num + 1))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let record: SyncRecord = serde_json::from_str(line)
            .with_context(|| format!("invalid JSON on line {}", line_num + 1))?;

        match record {
            SyncRecord::Memory(m) => {
                let max = db.max_content_size();
                if m.content.len() > max {
                    tracing::warn!(
                        id = m.id,
                        actual = m.content.len(),
                        max,
                        "skipping oversized memory during import"
                    );
                    stats.memories_skipped += 1;
                    continue;
                }
                let decision = db.reconcile_memory(&m.id, &m.updated_at)?;
                match decision {
                    ReconcileDecision::Skip => stats.memories_skipped += 1,
                    _ => pending_memories.push(m),
                }
            }
            SyncRecord::Link(l) => deferred_links.push(l),
            SyncRecord::Tombstone(t) => deferred_tombstones.push(t),
        }
    }

    // Phase 2: Batch embed
    if !pending_memories.is_empty() {
        let texts: Vec<&str> = pending_memories
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        let embeddings = embed_batch(&texts)?;
        anyhow::ensure!(
            embeddings.len() == texts.len(),
            "embed_batch returned {} embeddings for {} inputs",
            embeddings.len(),
            texts.len()
        );

        // Phase 3: Write memories
        for (m, embedding) in pending_memories.iter().zip(embeddings) {
            let projects: Vec<&str> = m.projects.iter().map(|s| s.as_str()).collect();
            let tags: Vec<&str> = m.tags.iter().map(|s| s.as_str()).collect();

            let action = db.import_memory(&ImportMemoryParams {
                id: &m.id,
                content: &m.content,
                memory_type: m.memory_type.as_deref(),
                projects: &projects,
                tags: &tags,
                created_at: &m.created_at,
                updated_at: &m.updated_at,
                archived_at: m.archived_at.as_deref(),
                embedding: &embedding,
            })?;

            match action {
                ImportAction::Inserted => stats.memories_inserted += 1,
                ImportAction::Updated => stats.memories_updated += 1,
                ImportAction::Skipped => stats.memories_skipped += 1,
            }
        }
    }

    // Phase 4: Write links (after memories exist)
    for l in &deferred_links {
        let action = db.import_link(l)?;
        match action {
            ImportAction::Inserted => stats.links_inserted += 1,
            ImportAction::Skipped => stats.links_skipped += 1,
            ImportAction::Updated => {}
        }
    }

    // Phase 5: Apply tombstones (after memories and links exist)
    for t in &deferred_tombstones {
        let applied = db.apply_tombstone(t)?;
        if applied {
            stats.tombstones_applied += 1;
        } else {
            stats.tombstones_skipped += 1;
        }
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
    use crate::db::types::StoreParams;
    use crate::embedding::{Embedder, MockEmbedder};

    fn test_db() -> Database {
        Database::open_in_memory(&DbConfig::default()).unwrap()
    }

    fn mock_embedder() -> MockEmbedder {
        MockEmbedder::new(768)
    }

    fn test_embedding(embedder: &MockEmbedder, text: &str) -> Vec<f32> {
        embedder.embed_one(text).unwrap()
    }

    #[test]
    fn import_with_closure_embeds_and_stores_correctly() {
        let db_a = test_db();
        let emb = mock_embedder();
        let embedding = test_embedding(&emb, "closures are great");

        let id = db_a
            .store(&StoreParams {
                content: "closures are great",
                memory_type: Some("fact"),
                projects: &["test"],
                tags: &["rust"],
                links: &[],
                embedding: &embedding,
            })
            .unwrap();

        // Export from DB A
        let mut buf = Vec::new();
        let opts = ExportOptions::default();
        export(&db_a, &mut buf, &opts).unwrap();

        // Import into fresh DB B using closure syntax
        let db_b = test_db();
        let stats = import(
            &db_b,
            |texts| emb.embed_documents(texts),
            &mut buf.as_slice(),
        )
        .unwrap();
        assert_eq!(stats.memories_inserted, 1);

        // Verify the memory exists with correct content
        let results = db_b.get(&[&id]).unwrap();
        assert_eq!(results.len(), 1);
        let m = &results[0].memory;
        assert_eq!(m.content, "closures are great");
        assert_eq!(m.memory_type.as_deref(), Some("fact"));
        assert_eq!(m.projects, vec!["test"]);
    }

    #[test]
    fn import_closure_error_propagates_as_import_failure() {
        let db_a = test_db();
        let emb = mock_embedder();
        let embedding = test_embedding(&emb, "will fail on import");

        db_a.store(&StoreParams {
            content: "will fail on import",
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &embedding,
        })
        .unwrap();

        // Export from DB A
        let mut buf = Vec::new();
        let opts = ExportOptions::default();
        export(&db_a, &mut buf, &opts).unwrap();

        // Import with a closure that always fails
        let db_b = test_db();
        let result = import(
            &db_b,
            |_texts| Err(anyhow::anyhow!("embedding service unavailable")),
            &mut buf.as_slice(),
        );

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("embedding service unavailable"),
            "error message should contain the closure's error, got: {err_msg}"
        );
    }

    #[test]
    fn import_closure_receives_exact_memory_content() {
        use std::sync::{Arc, Mutex};

        let db_a = test_db();
        let emb = mock_embedder();
        let embedding = test_embedding(&emb, "exact content check");

        db_a.store(&StoreParams {
            content: "exact content check",
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &embedding,
        })
        .unwrap();

        let mut buf = Vec::new();
        let opts = ExportOptions::default();
        export(&db_a, &mut buf, &opts).unwrap();

        // Capture what the batch closure receives
        let captured: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();

        let db_b = test_db();
        import(
            &db_b,
            |texts| {
                captured_clone
                    .lock()
                    .unwrap()
                    .push(texts.iter().map(|t| t.to_string()).collect());
                emb.embed_documents(texts)
            },
            &mut buf.as_slice(),
        )
        .unwrap();

        let batches = captured.lock().unwrap();
        assert_eq!(batches.len(), 1, "should be called once with one batch");
        assert_eq!(batches[0].len(), 1);
        assert_eq!(
            batches[0][0], "exact content check",
            "batch closure should receive the raw content string"
        );
    }

    #[test]
    fn batch_import_embeds_only_pending_memories() {
        use std::sync::{Arc, Mutex};

        let db_a = test_db();
        let emb = mock_embedder();

        // Store two memories in DB A.
        let _id1 = db_a
            .store(&StoreParams {
                content: "memory one",
                memory_type: Some("fact"),
                projects: &["proj"],
                tags: &["a"],
                links: &[],
                embedding: &test_embedding(&emb, "memory one"),
            })
            .unwrap();
        let _id2 = db_a
            .store(&StoreParams {
                content: "memory two",
                memory_type: Some("note"),
                projects: &["proj"],
                tags: &["b"],
                links: &[],
                embedding: &test_embedding(&emb, "memory two"),
            })
            .unwrap();

        // Export from DB A.
        let mut buf = Vec::new();
        let opts = ExportOptions::default();
        export(&db_a, &mut buf, &opts).unwrap();

        // In DB B, pre-populate memory one with a NEWER timestamp (will be skipped on import).
        let db_b = test_db();
        let emb_pre = test_embedding(&emb, "memory one");
        db_b.import_memory(&ImportMemoryParams {
            id: &_id1,
            content: "memory one",
            memory_type: Some("fact"),
            projects: &["proj"],
            tags: &["a"],
            created_at: "2099-01-01T00:00:00.000000Z",
            updated_at: "2099-01-01T00:00:00.000000Z",
            archived_at: None,
            embedding: &emb_pre,
        })
        .unwrap();

        // Import with a capturing closure.
        let captured: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();

        let stats = import(
            &db_b,
            |texts| {
                captured_clone
                    .lock()
                    .unwrap()
                    .push(texts.iter().map(|t| t.to_string()).collect());
                emb.embed_documents(texts)
            },
            &mut buf.as_slice(),
        )
        .unwrap();

        // Verify stats: one skipped (memory one), one inserted (memory two).
        assert_eq!(stats.memories_skipped, 1);
        assert_eq!(stats.memories_inserted, 1);

        // Verify batch closure was called once with only memory two's content.
        let batches = captured.lock().unwrap();
        assert_eq!(
            batches.len(),
            1,
            "embed_batch should be called exactly once"
        );
        assert_eq!(
            batches[0].len(),
            1,
            "batch should contain only the non-skipped memory"
        );
        assert_eq!(batches[0][0], "memory two");
    }

    #[test]
    fn sync_record_link_uses_db_link_type() {
        use crate::db::types::Link;

        // Construct a SyncRecord::Link using the db::types::Link directly.
        let link = Link {
            id: "link-001".into(),
            source_id: "mem-aaa".into(),
            target_id: "mem-bbb".into(),
            relation: "related_to".into(),
            created_at: "2025-01-15T10:00:00Z".into(),
            content: None,
        };
        let record = SyncRecord::Link(link);

        // Serialize and verify wire format.
        let json_str = serde_json::to_string(&record).unwrap();
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(json["record_type"], "link");
        assert_eq!(json["id"], "link-001");
        assert_eq!(json["source_id"], "mem-aaa");
        assert_eq!(json["target_id"], "mem-bbb");
        assert_eq!(json["relation"], "related_to");
        assert_eq!(json["created_at"], "2025-01-15T10:00:00Z");

        // Verify round-trip deserialization.
        let deserialized: SyncRecord = serde_json::from_str(&json_str).unwrap();
        assert!(matches!(deserialized, SyncRecord::Link(_)));
        if let SyncRecord::Link(l) = deserialized {
            assert_eq!(l.id, "link-001");
            assert_eq!(l.source_id, "mem-aaa");
        }
    }

    #[test]
    fn sync_record_tombstone_uses_db_tombstone_type() {
        use crate::db::types::{EntityType, Tombstone, TombstoneAction};

        // Construct a SyncRecord::Tombstone using the db::types::Tombstone directly.
        let tombstone = Tombstone {
            entity_type: EntityType::Memory,
            entity_id: "mem-123".into(),
            action: TombstoneAction::Archived,
            timestamp: "2025-06-01T12:00:00Z".into(),
        };
        let record = SyncRecord::Tombstone(tombstone);

        // Serialize and verify wire format matches the expected JSONL shape.
        let json_str = serde_json::to_string(&record).unwrap();
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(json["record_type"], "tombstone");
        assert_eq!(json["entity_type"], "memory");
        assert_eq!(json["entity_id"], "mem-123");
        assert_eq!(json["action"], "archived");
        assert_eq!(json["timestamp"], "2025-06-01T12:00:00Z");

        // Verify round-trip deserialization.
        let deserialized: SyncRecord = serde_json::from_str(&json_str).unwrap();
        assert!(matches!(deserialized, SyncRecord::Tombstone(_)));
        if let SyncRecord::Tombstone(t) = deserialized {
            assert_eq!(t.entity_type, EntityType::Memory);
            assert_eq!(t.entity_id, "mem-123");
        }
    }

    #[test]
    fn export_import_round_trip_single_memory() {
        let db_a = test_db();
        let emb = mock_embedder();
        let embedding = test_embedding(&emb, "Rust error handling uses Result<T, E>");

        let id = db_a
            .store(&StoreParams {
                content: "Rust error handling uses Result<T, E>",
                memory_type: Some("pattern"),
                projects: &["erinra"],
                tags: &["rust", "errors"],
                links: &[],
                embedding: &embedding,
            })
            .unwrap();

        // Export from DB A
        let mut buf = Vec::new();
        let opts = ExportOptions::default();
        let count = export(&db_a, &mut buf, &opts).unwrap();
        assert!(count >= 1, "should export at least 1 record");

        // Import into fresh DB B
        let db_b = test_db();
        let stats = import(
            &db_b,
            |texts| emb.embed_documents(texts),
            &mut buf.as_slice(),
        )
        .unwrap();
        assert_eq!(stats.memories_inserted, 1);

        // Verify the memory exists in DB B with matching fields
        let results = db_b.get(&[&id]).unwrap();
        assert_eq!(results.len(), 1);
        let m = &results[0].memory;
        assert_eq!(m.content, "Rust error handling uses Result<T, E>");
        assert_eq!(m.memory_type.as_deref(), Some("pattern"));
        assert_eq!(m.projects, vec!["erinra"]);
        assert_eq!(m.tags, vec!["errors", "rust"]); // tags are sorted by DB
        assert!(m.archived_at.is_none());
    }

    #[test]
    fn export_all_record_types_stable_order() {
        let db = test_db();
        let emb = mock_embedder();

        // Store two memories.
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
                links: &[(&id1, "related_to")],
                embedding: &test_embedding(&emb, "second memory"),
            })
            .unwrap();

        // Archive first memory (creates a tombstone).
        db.archive(&id1).unwrap();

        // Export.
        let mut buf = Vec::new();
        let opts = ExportOptions::default();
        export(&db, &mut buf, &opts).unwrap();

        // Parse all records.
        let output = String::from_utf8(buf).unwrap();
        let records: Vec<SyncRecord> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        // Should have: 2 memories, 1 link, 1 tombstone = 4 records.
        assert_eq!(records.len(), 4);

        // Memories come first, then links, then tombstones.
        assert!(matches!(&records[0], SyncRecord::Memory(_)));
        assert!(matches!(&records[1], SyncRecord::Memory(_)));
        assert!(matches!(&records[2], SyncRecord::Link(_)));
        assert!(matches!(&records[3], SyncRecord::Tombstone(_)));

        // Memories are sorted by created_at ASC, id ASC. When timestamps
        // tie (common in fast tests), order depends on UUID sort.
        let mem0 = match &records[0] {
            SyncRecord::Memory(m) => m,
            _ => panic!("expected memory record"),
        };
        let mem1 = match &records[1] {
            SyncRecord::Memory(m) => m,
            _ => panic!("expected memory record"),
        };
        let ids: std::collections::HashSet<&str> =
            [mem0.id.as_str(), mem1.id.as_str()].into_iter().collect();
        assert!(ids.contains(id1.as_str()));
        assert!(ids.contains(id2.as_str()));

        // The archived memory (id1) should have archived_at set.
        let archived = if mem0.id == id1 { mem0 } else { mem1 };
        assert!(
            archived.archived_at.is_some(),
            "archived memory should have archived_at"
        );

        // Link should reference the correct source/target.
        if let SyncRecord::Link(l) = &records[2] {
            assert_eq!(l.source_id, id2);
            assert_eq!(l.target_id, id1);
            assert_eq!(l.relation, "related_to");
        } else {
            panic!("expected link record");
        }

        // Tombstone for the archived memory.
        if let SyncRecord::Tombstone(t) = &records[3] {
            assert_eq!(t.entity_type, crate::db::types::EntityType::Memory);
            assert_eq!(t.entity_id, id1);
            assert_eq!(t.action, crate::db::types::TombstoneAction::Archived);
        } else {
            panic!("expected tombstone record");
        }
    }

    #[test]
    fn incremental_export_filters_by_since() {
        let db = test_db();
        let emb = mock_embedder();

        // Store first memory.
        let _id1 = db
            .store(&StoreParams {
                content: "old memory",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "old memory"),
            })
            .unwrap();

        // Capture a timestamp between the two stores.
        // We can read the first memory's updated_at to use as the boundary.
        let results = db.get(&[&_id1]).unwrap();
        let boundary = results[0].memory.updated_at.clone();

        // Store second memory (will be after boundary).
        // Need a slight delay to ensure different timestamps - use a manual approach.
        // Since SQLite uses strftime with microseconds, consecutive inserts might get
        // the same timestamp. Let's just set the since to a timestamp before any stores
        // and verify, then set to boundary and check only the second appears.
        let _id2 = db
            .store(&StoreParams {
                content: "new memory",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "new memory"),
            })
            .unwrap();

        // Full export: should have 2 memories.
        let mut buf_full = Vec::new();
        let full_opts = ExportOptions::default();
        let full_count = export(&db, &mut buf_full, &full_opts).unwrap();
        assert_eq!(full_count, 2);

        // Incremental export since boundary: should only include memories updated after boundary.
        let mut buf_incr = Vec::new();
        let incr_opts = ExportOptions {
            since: Some(boundary.clone()),
            ..ExportOptions::default()
        };
        let incr_count = export(&db, &mut buf_incr, &incr_opts).unwrap();

        let output = String::from_utf8(buf_incr).unwrap();
        let records: Vec<SyncRecord> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        // The incremental export should include the second memory but not the first
        // (assuming they got different timestamps). If same timestamp, both are excluded
        // since we use > not >=.
        // Check that at most 1 memory is in the incremental export.
        let memory_records: Vec<&MemoryRecord> = records
            .iter()
            .filter_map(|r| {
                if let SyncRecord::Memory(m) = r {
                    Some(m)
                } else {
                    None
                }
            })
            .collect();

        // At minimum, verify that the incremental count <= full count.
        assert!(
            incr_count <= full_count,
            "incremental export should return fewer records than full"
        );

        // If the second memory has a different timestamp, it should appear.
        // If timestamps are the same, neither should appear (> not >=).
        for m in &memory_records {
            assert_ne!(
                m.id, _id1,
                "old memory should not appear in incremental export if it has older timestamp"
            );
        }
    }

    #[test]
    fn tombstone_retention_purge() {
        let db = test_db();
        let emb = mock_embedder();

        // Store and archive a memory to create a tombstone.
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
        db.archive(&id).unwrap();

        // With a large retention window (90 days), the tombstone should be exported.
        let mut buf = Vec::new();
        let opts = ExportOptions {
            tombstone_retention_days: 90,
            ..ExportOptions::default()
        };
        export(&db, &mut buf, &opts).unwrap();

        let output = String::from_utf8(buf).unwrap();
        let tombstone_count = output
            .lines()
            .filter(|line| line.contains("\"record_type\":\"tombstone\""))
            .count();
        assert_eq!(tombstone_count, 1, "recent tombstone should be included");

        // Re-create the tombstone (it was purged by the previous export).
        // Actually, with 90 day retention the tombstone is recent so it should still be in DB.
        // Let's verify that purging with 0 days removes everything.

        // First store and archive another to get a fresh tombstone.
        let id2 = db
            .store(&StoreParams {
                content: "another archived",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "another archived"),
            })
            .unwrap();
        db.archive(&id2).unwrap();

        // Export with 0-day retention and purge enabled: tombstone is "too old" even if just created
        // because 0-day means only tombstones from the future would survive.
        let mut buf2 = Vec::new();
        let opts2 = ExportOptions {
            tombstone_retention_days: 0,
            purge: true, // enable purge for this test
            ..ExportOptions::default()
        };
        export(&db, &mut buf2, &opts2).unwrap();

        let output2 = String::from_utf8(buf2).unwrap();
        let tombstone_count2 = output2
            .lines()
            .filter(|line| line.contains("\"record_type\":\"tombstone\""))
            .count();
        assert_eq!(
            tombstone_count2, 0,
            "0-day retention should exclude all tombstones"
        );

        // Verify the tombstones were also purged from the DB (purge=true).
        let remaining = db.export_tombstones(9999).unwrap();
        assert_eq!(
            remaining.len(),
            0,
            "old tombstones should be purged from DB"
        );
    }

    #[test]
    fn import_conflict_resolution_newer_wins_older_skipped() {
        let db = test_db();
        let emb = mock_embedder();

        // Store a memory with known content.
        let embedding = test_embedding(&emb, "original content");
        let id = db
            .store(&StoreParams {
                content: "original content",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &embedding,
            })
            .unwrap();

        // Get the local memory's updated_at.
        let local = db.get(&[&id]).unwrap();
        let _local_ts = &local[0].memory.updated_at;

        // Case 1: Import with a NEWER timestamp — should update.
        let newer_ts = "2099-01-01T00:00:00.000000Z";
        let jsonl_newer = format!(
            r#"{{"record_type":"memory","id":"{}","content":"updated remotely","type":null,"projects":[],"tags":[],"created_at":"2020-01-01T00:00:00.000000Z","updated_at":"{}","archived_at":null}}"#,
            id, newer_ts
        );
        let jsonl_newer = jsonl_newer + "\n";

        let stats = import(
            &db,
            |texts| emb.embed_documents(texts),
            &mut jsonl_newer.as_bytes(),
        )
        .unwrap();
        assert_eq!(stats.memories_updated, 1);
        assert_eq!(stats.memories_skipped, 0);

        // Verify content was updated.
        let updated = db.get(&[&id]).unwrap();
        assert_eq!(updated[0].memory.content, "updated remotely");

        // Case 2: Import with an OLDER timestamp — should skip.
        let older_ts = "2000-01-01T00:00:00.000000Z";
        let jsonl_older = format!(
            r#"{{"record_type":"memory","id":"{}","content":"old version","type":null,"projects":[],"tags":[],"created_at":"2020-01-01T00:00:00.000000Z","updated_at":"{}","archived_at":null}}"#,
            id, older_ts
        );
        let jsonl_older = jsonl_older + "\n";

        let stats2 = import(
            &db,
            |texts| emb.embed_documents(texts),
            &mut jsonl_older.as_bytes(),
        )
        .unwrap();
        assert_eq!(stats2.memories_skipped, 1);
        assert_eq!(stats2.memories_updated, 0);

        // Content should still be the newer version.
        let still_updated = db.get(&[&id]).unwrap();
        assert_eq!(still_updated[0].memory.content, "updated remotely");

        // Case 3: Import with SAME timestamp (tie) — local wins, should skip.
        let jsonl_tie = format!(
            r#"{{"record_type":"memory","id":"{}","content":"tie version","type":null,"projects":[],"tags":[],"created_at":"2020-01-01T00:00:00.000000Z","updated_at":"{}","archived_at":null}}"#,
            id,
            newer_ts // same as what's in DB after case 1
        );
        let jsonl_tie = jsonl_tie + "\n";

        let stats3 = import(
            &db,
            |texts| emb.embed_documents(texts),
            &mut jsonl_tie.as_bytes(),
        )
        .unwrap();
        assert_eq!(stats3.memories_skipped, 1);

        // Content should not have changed.
        let unchanged = db.get(&[&id]).unwrap();
        assert_eq!(unchanged[0].memory.content, "updated remotely");
    }

    #[test]
    fn import_tombstone_archives_active_memory() {
        let db = test_db();
        let emb = mock_embedder();

        // Store an active memory.
        let id = db
            .store(&StoreParams {
                content: "will be archived by tombstone",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "will be archived by tombstone"),
            })
            .unwrap();

        // Verify it's active.
        let before = db.get(&[&id]).unwrap();
        assert!(before[0].memory.archived_at.is_none());

        // Import a tombstone for this memory.
        let ts = "2099-01-01T00:00:00.000000Z";
        let jsonl = format!(
            r#"{{"record_type":"tombstone","entity_type":"memory","entity_id":"{}","action":"archived","timestamp":"{}"}}"#,
            id, ts
        );
        let jsonl = jsonl + "\n";

        let stats = import(
            &db,
            |texts| emb.embed_documents(texts),
            &mut jsonl.as_bytes(),
        )
        .unwrap();
        assert_eq!(stats.tombstones_applied, 1);

        // Verify the memory is now archived.
        let after = db.get(&[&id]).unwrap();
        assert!(after[0].memory.archived_at.is_some());

        // Import same tombstone again — should be skipped (already archived).
        let stats2 = import(
            &db,
            |texts| emb.embed_documents(texts),
            &mut jsonl.as_bytes(),
        )
        .unwrap();
        assert_eq!(stats2.tombstones_skipped, 1);
        assert_eq!(stats2.tombstones_applied, 0);
    }

    #[test]
    fn gzip_round_trip() {
        use flate2::Compression;
        use flate2::read::GzDecoder;
        use flate2::write::GzEncoder;

        let db_a = test_db();
        let emb = mock_embedder();

        let id = db_a
            .store(&StoreParams {
                content: "gzip test memory",
                memory_type: Some("note"),
                projects: &["test-proj"],
                tags: &["gzip"],
                links: &[],
                embedding: &test_embedding(&emb, "gzip test memory"),
            })
            .unwrap();

        // Export with gzip compression.
        let mut compressed = Vec::new();
        {
            let mut gz_writer = GzEncoder::new(&mut compressed, Compression::default());
            let opts = ExportOptions::default();
            let count = export(&db_a, &mut gz_writer, &opts).unwrap();
            assert_eq!(count, 1);
            gz_writer.finish().unwrap();
        }

        // Compressed data should not be valid UTF-8 (it's binary).
        assert!(String::from_utf8(compressed.clone()).is_err());

        // Import with gzip decompression.
        let db_b = test_db();
        let mut gz_reader = GzDecoder::new(compressed.as_slice());
        let stats = import(&db_b, |texts| emb.embed_documents(texts), &mut gz_reader).unwrap();
        assert_eq!(stats.memories_inserted, 1);

        // Verify data integrity.
        let results = db_b.get(&[&id]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.content, "gzip test memory");
        assert_eq!(results[0].memory.memory_type.as_deref(), Some("note"));
        assert_eq!(results[0].memory.projects, vec!["test-proj"]);
    }

    #[test]
    fn full_round_trip_all_record_types_with_consolidated_types() {
        let db_a = test_db();
        let emb = mock_embedder();

        // Create two memories with a link between them.
        let id1 = db_a
            .store(&StoreParams {
                content: "first memory for consolidation test",
                memory_type: Some("note"),
                projects: &["proj-x"],
                tags: &["tag-a", "tag-b"],
                links: &[],
                embedding: &test_embedding(&emb, "first memory for consolidation test"),
            })
            .unwrap();
        let id2 = db_a
            .store(&StoreParams {
                content: "second memory for consolidation test",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[(&id1, "related_to")],
                embedding: &test_embedding(&emb, "second memory for consolidation test"),
            })
            .unwrap();

        // Archive first memory to create a tombstone.
        db_a.archive(&id1).unwrap();

        // Export all records from DB A.
        let mut buf = Vec::new();
        let opts = ExportOptions::default();
        let count = export(&db_a, &mut buf, &opts).unwrap();
        // 2 memories + 1 link + 1 tombstone = 4 records.
        assert_eq!(count, 4, "should export 4 records");

        // Parse and verify the wire format uses consolidated types.
        let output = String::from_utf8(buf.clone()).unwrap();
        let records: Vec<SyncRecord> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 4);

        // Verify SyncRecord::Link contains Link (not LinkRecord).
        if let SyncRecord::Link(l) = &records[2] {
            assert_eq!(l.source_id, id2);
            assert_eq!(l.target_id, id1);
            assert_eq!(l.relation, "related_to");
        } else {
            panic!("expected link record at index 2");
        }

        // Verify SyncRecord::Tombstone contains Tombstone (not TombstoneRecord).
        if let SyncRecord::Tombstone(t) = &records[3] {
            assert_eq!(t.entity_type, crate::db::types::EntityType::Memory);
            assert_eq!(t.entity_id, id1);
            assert_eq!(t.action, crate::db::types::TombstoneAction::Archived);
        } else {
            panic!("expected tombstone record at index 3");
        }

        // Import into fresh DB B.
        let db_b = test_db();
        let stats = import(
            &db_b,
            |texts| emb.embed_documents(texts),
            &mut buf.as_slice(),
        )
        .unwrap();
        assert_eq!(stats.memories_inserted, 2);
        assert_eq!(stats.links_inserted, 1);
        assert_eq!(stats.tombstones_applied, 0); // memory already imported as archived

        // Verify the archived memory round-tripped correctly.
        let m1 = db_b.get(&[&id1]).unwrap();
        assert_eq!(m1.len(), 1);
        assert_eq!(m1[0].memory.content, "first memory for consolidation test");
        assert_eq!(m1[0].memory.memory_type.as_deref(), Some("note"));
        assert!(m1[0].memory.archived_at.is_some(), "should be archived");
        assert_eq!(m1[0].memory.projects, vec!["proj-x"]);
        assert_eq!(m1[0].memory.tags, vec!["tag-a", "tag-b"]);

        // Verify second memory and link.
        let m2 = db_b.get(&[&id2]).unwrap();
        assert_eq!(m2.len(), 1);
        assert_eq!(m2[0].memory.content, "second memory for consolidation test");
        assert!(m2[0].memory.archived_at.is_none());
        assert_eq!(m2[0].outgoing_links.len(), 1);
        assert_eq!(m2[0].outgoing_links[0].target_id, id1);
    }

    #[test]
    fn import_errors_on_embedding_count_mismatch() {
        let db_a = test_db();
        let emb = mock_embedder();

        // Store 2 memories in DB A.
        db_a.store(&StoreParams {
            content: "memory alpha",
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, "memory alpha"),
        })
        .unwrap();
        db_a.store(&StoreParams {
            content: "memory beta",
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, "memory beta"),
        })
        .unwrap();

        // Export from DB A.
        let mut buf = Vec::new();
        let opts = ExportOptions::default();
        export(&db_a, &mut buf, &opts).unwrap();

        // Import into fresh DB B with a closure that returns fewer embeddings than inputs.
        let db_b = test_db();
        let result = import(
            &db_b,
            |_texts| Ok(vec![vec![0.0; 768]]), // Returns 1 embedding for 2 inputs
            &mut buf.as_slice(),
        );

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("embeddings"),
            "error should mention embeddings, got: {err_msg}"
        );
    }
}
