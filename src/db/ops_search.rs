//! Search operations: hybrid vector + FTS5 search and cosine similarity.
//!
//! Uses `unchecked_transaction()` for the same reason as `ops_core` — see that module's
//! doc comment for the full rationale (`Database` exposes `&self`, not `&mut self`).

use std::collections::{HashMap, HashSet};

use rusqlite::types::Value;

use super::Database;
use super::error::DbResult;
use super::helpers::*;
use super::types::*;

impl Database {
    // ── search ────────────────────────────────────────────────────────────

    /// Find memories by hybrid vector + FTS5 search with RRF merging.
    ///
    /// Combines vector similarity search (via sqlite-vec) with full-text keyword
    /// search (via FTS5), merges results using Reciprocal Rank Fusion, applies
    /// filters, updates access tracking, and returns results with links.
    pub fn search(&self, p: &SearchParams) -> DbResult<SearchResult> {
        use super::search::{self, RankedItem};

        let tx = self.conn().unchecked_transaction()?;

        // ── Build filter clause ──────────────────────────────────────────
        // Encapsulated as a cohesive unit so each search path (vector post-filter,
        // FTS5 inline filter) gets an independent copy of the SQL + params.
        let filter = build_base_filter(&p.filter);

        // ── Vector search ────────────────────────────────────────────────
        // Over-fetch 4x to compensate for post-filter losses. vec0 virtual tables
        // don't support JOINs, so we must fetch candidates first and filter second.
        // With very selective filters (>75% rejection), results may be fewer than
        // requested — acceptable at expected scale (<10k memories). The floor of 40
        // ensures enough candidates for small limit values.
        // Cap to prevent excessive memory use in sqlite-vec KNN search. At max values
        // (limit=500, offset=10K), oversample reaches 42K candidates. Generous for
        // expected scale (<10K memories) while bounding resource use.
        let capped_limit = p.limit.min(500) as i64;
        let capped_offset = p.offset.min(10_000) as i64;
        let oversample = ((capped_limit + capped_offset) * 4).max(40);

        let vec_results: Vec<(String, f32)> = if p.query_embedding.is_empty() {
            vec![]
        } else {
            let emb_bytes = embedding_to_bytes(p.query_embedding);
            let candidates: Vec<(String, f32)> = {
                let mut stmt = tx.prepare(
                    "SELECT memory_id, distance FROM memory_embeddings \
                     WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2",
                )?;
                stmt.query_map(rusqlite::params![emb_bytes, oversample], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<Result<_, _>>()?
            };

            // Post-filter against memory filters.
            if candidates.is_empty() || filter.sql.is_empty() {
                candidates
            } else {
                let candidate_ids: Vec<&str> =
                    candidates.iter().map(|(id, _)| id.as_str()).collect();
                let ph = vec!["?"; candidate_ids.len()].join(",");

                let sql = format!(
                    "SELECT m.id FROM memories m WHERE {} AND m.id IN ({ph})",
                    filter.sql
                );
                let mut all_params = filter.params.clone();
                for id in &candidate_ids {
                    all_params.push(Value::Text(id.to_string()));
                }

                let mut stmt = tx.prepare(&sql)?;
                let valid: HashSet<String> = stmt
                    .query_map(rusqlite::params_from_iter(&all_params), |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<Result<_, _>>()?;

                candidates
                    .into_iter()
                    .filter(|(id, _)| valid.contains(id))
                    .collect()
            }
        };

        // ── FTS5 search ──────────────────────────────────────────────────
        let fts_results: Vec<String> = match search::escape_fts5_query(p.query) {
            None => vec![],
            Some(escaped) => {
                let fts_where = if filter.sql.is_empty() {
                    "WHERE memories_fts MATCH ?".to_string()
                } else {
                    format!("WHERE memories_fts MATCH ? AND {}", filter.sql)
                };

                let sql = format!(
                    "SELECT m.id FROM memories_fts \
                     JOIN memories m ON m.rowid = memories_fts.rowid \
                     {fts_where} \
                     ORDER BY memories_fts.rank \
                     LIMIT ?"
                );

                let mut fts_params: Vec<Value> = Vec::new();
                fts_params.push(Value::Text(escaped));
                fts_params.extend(filter.params.clone());
                fts_params.push(Value::Integer(oversample));

                let mut stmt = tx.prepare(&sql)?;
                stmt.query_map(rusqlite::params_from_iter(&fts_params), |row| row.get(0))?
                    .collect::<Result<_, _>>()?
            }
        };

        // ── RRF merge ────────────────────────────────────────────────────
        let vec_ranked: Vec<RankedItem> = vec_results
            .iter()
            .enumerate()
            .map(|(i, (id, _))| RankedItem {
                id: id.clone(),
                rank: (i + 1) as u32,
            })
            .collect();

        let fts_ranked: Vec<RankedItem> = fts_results
            .iter()
            .enumerate()
            .map(|(i, id)| RankedItem {
                id: id.clone(),
                rank: (i + 1) as u32,
            })
            .collect();

        let mut merged = search::rrf_merge(&[&vec_ranked, &fts_ranked], p.rrf_k);
        let mut total = merged.len() as i64;

        // ── Reranking (optional) ────────────────────────────────────────
        // Reranker failure is non-fatal: fall back to RRF scores rather than
        // destroying an already-computed result set.
        if let Some(reranker) = p.reranker
            && !merged.is_empty()
        {
            match rerank_candidates(reranker, &tx, &merged, p.query, p.reranker_threshold) {
                Ok(reranked) => {
                    total = reranked.len() as i64;
                    merged = reranked;
                }
                Err(e) => {
                    tracing::warn!("reranker failed, falling back to RRF scores: {e:#}");
                }
            }
        }

        // Apply offset and limit.
        let page: Vec<(String, f64)> = merged
            .into_iter()
            .skip(p.offset as usize)
            .take(p.limit as usize)
            .collect();

        if page.is_empty() {
            tx.commit()?;
            return Ok(SearchResult {
                results: vec![],
                total,
            });
        }

        // ── Fetch full memory details ────────────────────────────────────
        let hit_ids: Vec<String> = page.iter().map(|(id, _)| id.clone()).collect();
        let score_map: HashMap<&str, f64> = page.iter().map(|(id, s)| (id.as_str(), *s)).collect();

        let placeholders = vec!["?"; hit_ids.len()].join(",");
        let id_values: Vec<Value> = hit_ids.iter().map(|id| Value::Text(id.clone())).collect();

        // ── Update access tracking ───────────────────────────────────────
        // Runs before the SELECT so returned Memory structs reflect the
        // post-access values (access_count incremented, last_accessed_at set).
        {
            let mut stmt = tx.prepare(
                "UPDATE memories SET \
                     last_accessed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
                     access_count = access_count + 1 \
                 WHERE id = ?",
            )?;
            for id in &hit_ids {
                stmt.execute([id])?;
            }
        }

        let sql = format!(
            "SELECT m.id, m.content, m.type, m.created_at, m.updated_at, \
                    m.archived_at, m.last_accessed_at, m.access_count \
             FROM memories m WHERE m.id IN ({placeholders})"
        );
        let mut memories: Vec<Memory> = {
            let mut stmt = tx.prepare(&sql)?;
            stmt.query_map(rusqlite::params_from_iter(&id_values), map_memory_row)?
                .collect::<Result<_, _>>()?
        };

        let hit_id_refs: Vec<&str> = hit_ids.iter().map(|s| s.as_str()).collect();
        fill_projects_and_tags(&tx, &mut memories, &hit_id_refs)?;

        // Apply content truncation in application layer.
        if let Some(max) = p.content_max_length {
            for m in &mut memories {
                m.truncate(max);
            }
        }

        // Fetch links (both directions).
        let outgoing: Vec<Link> = {
            let sql = format!(
                "SELECT id, source_id, target_id, relation, created_at FROM links \
                 WHERE source_id IN ({placeholders})"
            );
            let mut stmt = tx.prepare(&sql)?;
            stmt.query_map(rusqlite::params_from_iter(&id_values), map_link)?
                .collect::<Result<_, _>>()?
        };
        let incoming: Vec<Link> = {
            let sql = format!(
                "SELECT id, source_id, target_id, relation, created_at FROM links \
                 WHERE target_id IN ({placeholders})"
            );
            let mut stmt = tx.prepare(&sql)?;
            stmt.query_map(rusqlite::params_from_iter(&id_values), map_link)?
                .collect::<Result<_, _>>()?
        };

        tx.commit()?;

        // ── Assemble results in RRF score order ──────────────────────────
        memories.sort_by(|a, b| {
            let sa = score_map.get(a.id.as_str()).copied().unwrap_or(0.0);
            let sb = score_map.get(b.id.as_str()).copied().unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });

        let results = memories
            .into_iter()
            .map(|mem| {
                let score = score_map.get(mem.id.as_str()).copied().unwrap_or(0.0);
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
                SearchHit {
                    memory: mem,
                    outgoing_links: out,
                    incoming_links: inc,
                    score,
                }
            })
            .collect();

        Ok(SearchResult { results, total })
    }

    // ── find_similar ─────────────────────────────────────────────────────

    /// Find memories with similar content using vector cosine similarity.
    /// Returns `(Memory, similarity)` pairs sorted by descending similarity.
    /// Filters out archived memories and excluded IDs.
    pub fn find_similar(
        &self,
        embedding: &[f32],
        limit: usize,
        exclude_ids: &[&str],
        content_max_length: Option<u32>,
    ) -> DbResult<Vec<(Memory, f64)>> {
        if embedding.is_empty() || limit == 0 {
            return Ok(vec![]);
        }

        let emb_bytes = embedding_to_bytes(embedding);
        let oversample = (limit as i64 * 4).max(20);
        let exclude_set: HashSet<&str> = exclude_ids.iter().copied().collect();

        let tx = self.conn().unchecked_transaction()?;

        // KNN search in vec0.
        let candidates: Vec<(String, f64)> = {
            let mut stmt = tx.prepare(
                "SELECT memory_id, distance FROM memory_embeddings \
                 WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2",
            )?;
            stmt.query_map(rusqlite::params![emb_bytes, oversample], |row| {
                let id: String = row.get(0)?;
                let dist: f32 = row.get(1)?;
                // cosine similarity = 1 - cosine_distance. The distance_metric is set
                // to 'cosine' in migration_v1 (db/mod.rs). If the metric changes,
                // this formula must be updated.
                Ok((id, 1.0 - dist as f64))
            })?
            .collect::<Result<_, _>>()?
        };

        // Filter: exclude specified IDs, keep only active (non-archived).
        let filtered_ids: Vec<&str> = candidates
            .iter()
            .filter(|(id, _)| !exclude_set.contains(id.as_str()))
            .map(|(id, _)| id.as_str())
            .collect();

        if filtered_ids.is_empty() {
            tx.commit()?;
            return Ok(vec![]);
        }

        let ph = vec!["?"; filtered_ids.len()].join(",");
        let active_sql =
            format!("SELECT id FROM memories WHERE id IN ({ph}) AND archived_at IS NULL");
        let active_params: Vec<Value> = filtered_ids
            .iter()
            .map(|id| Value::Text(id.to_string()))
            .collect();
        let active_ids: HashSet<String> = {
            let mut stmt = tx.prepare(&active_sql)?;
            stmt.query_map(rusqlite::params_from_iter(&active_params), |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<_, _>>()?
        };

        // Build similarity map and collect IDs (in similarity order).
        let sim_map: HashMap<&str, f64> = candidates
            .iter()
            .map(|(id, sim)| (id.as_str(), *sim))
            .collect();
        let result_ids: Vec<&str> = candidates
            .iter()
            .filter(|(id, _)| active_ids.contains(id) && !exclude_set.contains(id.as_str()))
            .take(limit)
            .map(|(id, _)| id.as_str())
            .collect();

        if result_ids.is_empty() {
            tx.commit()?;
            return Ok(vec![]);
        }

        // Fetch memory details.
        let ph = vec!["?"; result_ids.len()].join(",");
        let fetch_params: Vec<Value> = result_ids
            .iter()
            .map(|id| Value::Text(id.to_string()))
            .collect();

        let sql = format!(
            "SELECT m.id, m.content, m.type, m.created_at, m.updated_at, \
                    m.archived_at, m.last_accessed_at, m.access_count \
             FROM memories m WHERE m.id IN ({ph})"
        );
        let mut memories: Vec<Memory> = {
            let mut stmt = tx.prepare(&sql)?;
            stmt.query_map(rusqlite::params_from_iter(&fetch_params), map_memory_row)?
                .collect::<Result<_, _>>()?
        };

        fill_projects_and_tags(&tx, &mut memories, &result_ids)?;

        // Apply content truncation in application layer.
        if let Some(max) = content_max_length {
            for m in &mut memories {
                m.truncate(max);
            }
        }

        tx.commit()?;

        // Sort by similarity (descending) and pair with scores.
        let mut paired: Vec<(Memory, f64)> = memories
            .into_iter()
            .map(|mem| {
                let sim = sim_map.get(mem.id.as_str()).copied().unwrap_or(0.0);
                (mem, sim)
            })
            .collect();
        paired.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        Ok(paired)
    }
}

/// Rerank merged RRF candidates using a cross-encoder model.
///
/// Fetches content for all candidate IDs, scores each (query, content) pair,
/// filters below threshold, and returns sorted by reranker score descending.
// NOTE: Content is fetched again in the full-memory query after pagination.
// Acceptable at expected scale (<100 candidates); refactor if profiling shows impact.
fn rerank_candidates(
    reranker: &dyn crate::embedding::Reranker,
    tx: &rusqlite::Transaction<'_>,
    merged: &[(String, f64)],
    query: &str,
    threshold: f64,
) -> anyhow::Result<Vec<(String, f64)>> {
    let merged_ids: Vec<&str> = merged.iter().map(|(id, _)| id.as_str()).collect();
    let ph = vec!["?"; merged_ids.len()].join(",");
    let id_params: Vec<Value> = merged_ids
        .iter()
        .map(|id| Value::Text(id.to_string()))
        .collect();
    let sql = format!("SELECT id, content FROM memories WHERE id IN ({ph})");
    let content_map: HashMap<String, String> = {
        let mut stmt = tx.prepare(&sql)?;
        stmt.query_map(rusqlite::params_from_iter(&id_params), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<HashMap<_, _>, _>>()?
    };

    // Build content list in merged order.
    let contents: Vec<(String, String)> = merged_ids
        .iter()
        .filter_map(|id| {
            let content = content_map.get(*id);
            if content.is_none() {
                tracing::warn!(
                    "reranker: candidate {id} missing from content_map (should not happen within transaction)"
                );
            }
            content.map(|c| (id.to_string(), c.clone()))
        })
        .collect();

    let doc_refs: Vec<&str> = contents.iter().map(|(_, c)| c.as_str()).collect();
    let scores = reranker.rerank(query, &doc_refs)?;

    // Pair IDs with reranker scores, filter by threshold (inclusive), sort descending.
    let mut reranked: Vec<(String, f64)> = contents
        .iter()
        .zip(scores)
        .map(|((id, _), s)| (id.clone(), s as f64))
        .collect();
    reranked.retain(|(_, score)| *score >= threshold);
    reranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    Ok(reranked)
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

    /// Store several memories and return their IDs for search tests.
    fn seed_search_db(db: &Database, emb: &MockEmbedder) -> Vec<String> {
        let items = [
            (
                "Rust error handling with Result and Option types",
                Some("pattern"),
                &["erinra"][..],
                &["rust", "error-handling"][..],
            ),
            (
                "Python list comprehensions for data transformation",
                Some("pattern"),
                &["data-pipeline"][..],
                &["python"][..],
            ),
            (
                "SQLite WAL mode enables concurrent readers",
                Some("decision"),
                &["erinra"][..],
                &["sqlite", "concurrency"][..],
            ),
            (
                "Use tokio for async runtime in Rust projects",
                Some("decision"),
                &["erinra"][..],
                &["rust", "async"][..],
            ),
            (
                "Git rebase workflow for clean history",
                Some("pattern"),
                &[][..],
                &["git"][..],
            ),
        ];

        items
            .iter()
            .map(|(content, typ, projects, tags)| {
                db.store(&StoreParams {
                    content,
                    memory_type: *typ,
                    projects,
                    tags,
                    links: &[],
                    embedding: &test_embedding(emb, content),
                })
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn search_returns_results() {
        let db = test_db();
        let emb = mock_embedder();
        let ids = seed_search_db(&db, &emb);

        let query_embedding = emb.embed_query("rust error handling").unwrap();
        let results = db
            .search(&SearchParams {
                query: "rust error handling",
                query_embedding: &query_embedding,
                ..Default::default()
            })
            .unwrap()
            .results;

        // Should return some results (FTS5 matches "rust" and "error" and "handling").
        assert!(!results.is_empty());
        assert!(results.len() <= 10);
        // The most relevant memory ("Rust error handling with Result...") must appear.
        assert!(
            results.iter().any(|h| h.memory.id == ids[0]),
            "expected the 'Rust error handling' memory in results"
        );
        // All results should have a positive score.
        for hit in &results {
            assert!(hit.score > 0.0);
        }
    }

    #[test]
    fn search_filters_by_project() {
        let db = test_db();
        let emb = mock_embedder();
        seed_search_db(&db, &emb);

        let query_embedding = emb.embed_query("patterns").unwrap();
        let results = db
            .search(&SearchParams {
                query: "patterns",
                query_embedding: &query_embedding,
                filter: FilterParams {
                    projects: Some(&["data-pipeline"]),
                    include_global: false,
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .results;

        // Only "Python list comprehensions" belongs to data-pipeline.
        // With include_global=false, global memories (git rebase) are excluded.
        assert!(!results.is_empty(), "expected at least one result");
        for hit in &results {
            assert!(
                hit.memory.projects.contains(&"data-pipeline".to_string()),
                "unexpected project in result: {:?}",
                hit.memory.projects
            );
        }
    }

    #[test]
    fn search_filters_by_type() {
        let db = test_db();
        let emb = mock_embedder();
        seed_search_db(&db, &emb);

        let query_embedding = emb.embed_query("decisions").unwrap();
        let results = db
            .search(&SearchParams {
                query: "decisions",
                query_embedding: &query_embedding,
                filter: FilterParams {
                    memory_type: Some("decision"),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .results;

        assert!(!results.is_empty(), "expected at least one result");
        for hit in &results {
            assert_eq!(hit.memory.memory_type.as_deref(), Some("decision"));
        }
    }

    #[test]
    fn search_filters_by_tags() {
        let db = test_db();
        let emb = mock_embedder();
        seed_search_db(&db, &emb);

        let query_embedding = emb.embed_query("rust async").unwrap();
        let results = db
            .search(&SearchParams {
                query: "rust async",
                query_embedding: &query_embedding,
                filter: FilterParams {
                    tags: Some(&["rust"]),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .results;

        assert!(!results.is_empty(), "expected at least one result");
        for hit in &results {
            assert!(
                hit.memory.tags.contains(&"rust".to_string()),
                "expected 'rust' tag, got: {:?}",
                hit.memory.tags
            );
        }
    }

    #[test]
    fn search_excludes_archived() {
        let db = test_db();
        let emb = mock_embedder();
        let ids = seed_search_db(&db, &emb);

        // Archive the first memory.
        db.archive(&ids[0]).unwrap();

        let query_embedding = emb.embed_query("rust error handling").unwrap();
        let results = db
            .search(&SearchParams {
                query: "rust error handling",
                query_embedding: &query_embedding,
                ..Default::default()
            })
            .unwrap()
            .results;

        assert!(!results.is_empty(), "expected non-archived results");
        for hit in &results {
            assert_ne!(hit.memory.id, ids[0], "archived memory should be excluded");
        }
    }

    #[test]
    fn search_updates_access_tracking() {
        let db = test_db();
        let emb = mock_embedder();
        let ids = seed_search_db(&db, &emb);

        // Verify initial access_count is 0.
        let before = db.get(&[&ids[0]]).unwrap();
        assert_eq!(before[0].memory.access_count, 0);
        assert!(before[0].memory.last_accessed_at.is_none());

        // Search — the first memory should be returned (FTS5 matches "Rust").
        let query_embedding = emb.embed_query("Rust error handling with Result").unwrap();
        let results = db
            .search(&SearchParams {
                query: "Rust error handling with Result",
                query_embedding: &query_embedding,
                ..Default::default()
            })
            .unwrap()
            .results;

        // ids[0] must be in results (FTS5 matches "Rust", "error", "handling", "Result").
        let hit = results
            .iter()
            .find(|h| h.memory.id == ids[0])
            .expect("expected ids[0] in search results to verify access tracking");
        // The returned SearchHit should reflect post-access values (UPDATE runs before SELECT).
        assert_eq!(hit.memory.access_count, 1);
        assert!(hit.memory.last_accessed_at.is_some());
        // Verify via a separate get() that the database state is also correct.
        let after = db.get(&[&ids[0]]).unwrap();
        assert_eq!(after[0].memory.access_count, 1);
        assert!(after[0].memory.last_accessed_at.is_some());
    }

    #[test]
    fn search_includes_links() {
        let db = test_db();
        let emb = mock_embedder();
        let ids = seed_search_db(&db, &emb);

        // Create a link between first two memories.
        db.link(&ids[0], &ids[2], "related_to").unwrap();

        let query_embedding = emb.embed_query("Rust error handling with Result").unwrap();
        let results = db
            .search(&SearchParams {
                query: "Rust error handling with Result",
                query_embedding: &query_embedding,
                ..Default::default()
            })
            .unwrap()
            .results;

        // The first memory must be returned and should have an outgoing link.
        let hit = results
            .iter()
            .find(|h| h.memory.id == ids[0])
            .expect("expected ids[0] in search results to verify links");
        assert_eq!(hit.outgoing_links.len(), 1);
        assert_eq!(hit.outgoing_links[0].relation, "related_to");
    }

    #[test]
    fn search_content_truncation() {
        let db = test_db();
        let emb = mock_embedder();

        let long_content = format!("rust {}", "x".repeat(1000));
        db.store(&StoreParams {
            content: &long_content,
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, &long_content),
        })
        .unwrap();

        let query_embedding = emb.embed_query("rust").unwrap();
        let results = db
            .search(&SearchParams {
                query: "rust",
                query_embedding: &query_embedding,
                content_max_length: Some(50),
                ..Default::default()
            })
            .unwrap()
            .results;

        assert!(!results.is_empty());
        for hit in &results {
            // content_max_length counts Unicode characters (via Memory::truncate).
            assert!(hit.memory.content.chars().count() <= 50);
            assert!(
                hit.memory.truncated,
                "long content should be marked as truncated"
            );
        }
    }

    #[test]
    fn search_content_truncation_unicode() {
        let db = test_db();
        let emb = mock_embedder();

        // Use multi-byte UTF-8 content: CJK characters are 3 bytes each.
        // "rust " (5 chars) + 100 CJK chars = 105 chars, but 5 + 300 = 305 bytes.
        let long_content = format!("rust {}", "\u{9519}".repeat(100));
        assert!(long_content.len() > 50); // More than 50 bytes.
        assert!(long_content.chars().count() > 50); // More than 50 characters.

        db.store(&StoreParams {
            content: &long_content,
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, &long_content),
        })
        .unwrap();

        let query_embedding = emb.embed_query("rust").unwrap();
        let results = db
            .search(&SearchParams {
                query: "rust",
                query_embedding: &query_embedding,
                content_max_length: Some(50),
                ..Default::default()
            })
            .unwrap()
            .results;

        assert!(!results.is_empty());
        for hit in &results {
            // Memory::truncate counts characters, not bytes. 50 CJK chars = 150 bytes.
            let char_count = hit.memory.content.chars().count();
            assert!(char_count <= 50, "got {char_count} chars, expected <= 50");
            assert!(
                hit.memory.content.len() > char_count,
                "multi-byte chars should make len > char count"
            );
            assert!(
                hit.memory.truncated,
                "long content should be marked as truncated"
            );
        }
    }

    #[test]
    fn search_empty_query_returns_vector_only() {
        let db = test_db();
        let emb = mock_embedder();
        seed_search_db(&db, &emb);

        // Empty query string skips FTS5, but vector search still runs.
        let query_embedding = emb.embed_query("rust patterns").unwrap();
        let results = db
            .search(&SearchParams {
                query: "",
                query_embedding: &query_embedding,
                ..Default::default()
            })
            .unwrap()
            .results;

        // Should still get results from vector search alone.
        assert!(!results.is_empty());
    }

    #[test]
    fn search_empty_db() {
        let db = test_db();
        let emb = mock_embedder();

        let query_embedding = emb.embed_query("anything").unwrap();
        let results = db
            .search(&SearchParams {
                query: "anything",
                query_embedding: &query_embedding,
                ..Default::default()
            })
            .unwrap()
            .results;

        assert!(results.is_empty());
    }

    #[test]
    fn search_pagination() {
        let db = test_db();
        let emb = mock_embedder();
        seed_search_db(&db, &emb);

        let query_embedding = emb.embed_query("programming").unwrap();

        let page1 = db
            .search(&SearchParams {
                query: "programming",
                query_embedding: &query_embedding,
                limit: 2,
                offset: 0,
                ..Default::default()
            })
            .unwrap()
            .results;

        let page2 = db
            .search(&SearchParams {
                query: "programming",
                query_embedding: &query_embedding,
                limit: 2,
                offset: 2,
                ..Default::default()
            })
            .unwrap()
            .results;

        // Both pages should have results and not overlap.
        assert!(!page1.is_empty(), "expected page1 results");
        assert!(!page2.is_empty(), "expected page2 results");
        let page1_ids: Vec<&str> = page1.iter().map(|h| h.memory.id.as_str()).collect();
        for hit in &page2 {
            assert!(
                !page1_ids.contains(&hit.memory.id.as_str()),
                "page2 should not contain page1 results"
            );
        }
    }

    #[test]
    fn search_created_after_filters_older_memories() {
        let db = test_db();
        let emb = mock_embedder();

        // Store two memories (both get "now" timestamps from SQLite).
        let old_id = db
            .store(&StoreParams {
                content: "Rust ownership and borrowing rules",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "Rust ownership and borrowing rules"),
            })
            .unwrap();

        let new_id = db
            .store(&StoreParams {
                content: "Rust lifetimes and ownership advanced patterns",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "Rust lifetimes and ownership advanced patterns"),
            })
            .unwrap();

        // Backdating the "old" memory to 2020 via direct SQL.
        db.conn()
            .execute(
                "UPDATE memories SET created_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1",
                [&old_id],
            )
            .unwrap();

        // Search with created_after = 2025 — should only find the new memory.
        let query_embedding = emb.embed_query("Rust ownership").unwrap();
        let results = db
            .search(&SearchParams {
                query: "Rust ownership",
                query_embedding: &query_embedding,
                filter: FilterParams {
                    time: TimeFilters {
                        created_after: Some("2025-01-01T00:00:00.000Z"),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .results;

        // The old memory (2020) should be excluded.
        let result_ids: Vec<&str> = results.iter().map(|h| h.memory.id.as_str()).collect();
        assert!(
            !result_ids.contains(&old_id.as_str()),
            "old memory should be excluded by created_after filter"
        );
        assert!(
            result_ids.contains(&new_id.as_str()),
            "new memory should be included"
        );
    }

    #[test]
    fn search_created_before_filters_newer_memories() {
        let db = test_db();
        let emb = mock_embedder();

        let old_id = db
            .store(&StoreParams {
                content: "Rust ownership and borrowing rules",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "Rust ownership and borrowing rules"),
            })
            .unwrap();

        let new_id = db
            .store(&StoreParams {
                content: "Rust lifetimes and ownership advanced patterns",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "Rust lifetimes and ownership advanced patterns"),
            })
            .unwrap();

        // Backdating the "old" memory to 2020.
        db.conn()
            .execute(
                "UPDATE memories SET created_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1",
                [&old_id],
            )
            .unwrap();

        // Search with created_before = 2021 — should only find the old memory.
        let query_embedding = emb.embed_query("Rust ownership").unwrap();
        let results = db
            .search(&SearchParams {
                query: "Rust ownership",
                query_embedding: &query_embedding,
                filter: FilterParams {
                    time: TimeFilters {
                        created_before: Some("2021-01-01T00:00:00.000Z"),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .results;

        let result_ids: Vec<&str> = results.iter().map(|h| h.memory.id.as_str()).collect();
        assert!(
            result_ids.contains(&old_id.as_str()),
            "old memory should be included by created_before filter"
        );
        assert!(
            !result_ids.contains(&new_id.as_str()),
            "new memory should be excluded by created_before filter"
        );
    }

    #[test]
    fn search_updated_after_and_before_filter_by_update_time() {
        let db = test_db();
        let emb = mock_embedder();

        let id1 = db
            .store(&StoreParams {
                content: "Rust pattern matching basics",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "Rust pattern matching basics"),
            })
            .unwrap();

        let id2 = db
            .store(&StoreParams {
                content: "Rust pattern matching advanced exhaustiveness",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "Rust pattern matching advanced exhaustiveness"),
            })
            .unwrap();

        // Set id1's updated_at to the past, id2 stays "now" (2026-ish).
        db.conn()
            .execute(
                "UPDATE memories SET updated_at = '2020-06-01T00:00:00.000Z' WHERE id = ?1",
                [&id1],
            )
            .unwrap();

        // updated_after = 2025 should exclude id1.
        let query_embedding = emb.embed_query("Rust pattern matching").unwrap();
        let results = db
            .search(&SearchParams {
                query: "Rust pattern matching",
                query_embedding: &query_embedding,
                filter: FilterParams {
                    time: TimeFilters {
                        updated_after: Some("2025-01-01T00:00:00.000Z"),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .results;

        let result_ids: Vec<&str> = results.iter().map(|h| h.memory.id.as_str()).collect();
        assert!(
            !result_ids.contains(&id1.as_str()),
            "id1 with old updated_at should be excluded by updated_after"
        );
        assert!(
            result_ids.contains(&id2.as_str()),
            "id2 with recent updated_at should be included"
        );

        // updated_before = 2021 should only include id1.
        let results = db
            .search(&SearchParams {
                query: "Rust pattern matching",
                query_embedding: &query_embedding,
                filter: FilterParams {
                    time: TimeFilters {
                        updated_before: Some("2021-01-01T00:00:00.000Z"),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .results;

        let result_ids: Vec<&str> = results.iter().map(|h| h.memory.id.as_str()).collect();
        assert!(
            result_ids.contains(&id1.as_str()),
            "id1 should be included by updated_before"
        );
        assert!(
            !result_ids.contains(&id2.as_str()),
            "id2 should be excluded by updated_before"
        );
    }

    #[test]
    fn search_time_filters_apply_to_both_fts_and_vector_paths() {
        let db = test_db();
        let emb = mock_embedder();

        // Store a memory and backdate it.
        let old_id = db
            .store(&StoreParams {
                content: "SQLite database indexing strategies",
                memory_type: None,
                projects: &[],
                tags: &[],
                links: &[],
                embedding: &test_embedding(&emb, "SQLite database indexing strategies"),
            })
            .unwrap();

        db.conn()
            .execute(
                "UPDATE memories SET created_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1",
                [&old_id],
            )
            .unwrap();

        // Search with text that matches via FTS5 ("SQLite") and vector similarity.
        // The time filter should exclude it from both paths.
        let query_embedding = emb.embed_query("SQLite indexing").unwrap();
        let results = db
            .search(&SearchParams {
                query: "SQLite indexing",
                query_embedding: &query_embedding,
                filter: FilterParams {
                    time: TimeFilters {
                        created_after: Some("2025-01-01T00:00:00.000Z"),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .results;

        // No results because the only matching memory is too old.
        assert!(
            results.is_empty(),
            "time filter should exclude old memory from both FTS5 and vector paths"
        );
    }

    #[test]
    fn search_results_ordered_by_score() {
        let db = test_db();
        let emb = mock_embedder();
        seed_search_db(&db, &emb);

        let query_embedding = emb.embed_query("rust error handling").unwrap();
        let results = db
            .search(&SearchParams {
                query: "rust error handling",
                query_embedding: &query_embedding,
                ..Default::default()
            })
            .unwrap()
            .results;

        // Scores should be in descending order.
        for window in results.windows(2) {
            assert!(
                window[0].score >= window[1].score,
                "results not in descending score order: {} < {}",
                window[0].score,
                window[1].score
            );
        }
    }

    #[test]
    fn search_with_reranker_reorders_by_reranker_scores() {
        use crate::embedding::MockReranker;

        let db = test_db();
        let emb = mock_embedder();

        // Store memories with varying word overlap to the query.
        // MockReranker scores by word overlap with the query.
        // Query will be "sqlite concurrent access".
        let contents = [
            "python is a dynamically typed language", // 0 overlap words
            "sqlite database storage engine",         // 1 overlap: "sqlite"
            "sqlite uses wal mode for concurrent access", // 3 overlap: "sqlite", "concurrent", "access"
        ];

        let ids: Vec<String> = contents
            .iter()
            .map(|content| {
                db.store(&StoreParams {
                    content,
                    memory_type: None,
                    projects: &[],
                    tags: &[],
                    links: &[],
                    embedding: &test_embedding(&emb, content),
                })
                .unwrap()
            })
            .collect();

        let reranker = MockReranker::new();
        let query = "sqlite concurrent access";
        let query_embedding = emb.embed_query(query).unwrap();
        let result = db
            .search(&SearchParams {
                query,
                query_embedding: &query_embedding,
                reranker: Some(&reranker),
                reranker_threshold: 0.0,
                ..Default::default()
            })
            .unwrap();

        // With reranker, scores should reflect word overlap, not RRF.
        // ids[2] has 3 overlap words -> highest score
        // ids[1] has 1 overlap word -> middle score
        // ids[0] has 0 overlap words -> score 0.0 (included: 0.0 >= 0.0 threshold)
        assert!(!result.results.is_empty(), "expected results with reranker");

        // The first result should be the one with 3 overlapping words.
        assert_eq!(
            result.results[0].memory.id, ids[2],
            "highest reranker score (3 word overlap) should be first"
        );

        // Scores should be in descending order.
        for window in result.results.windows(2) {
            assert!(
                window[0].score >= window[1].score,
                "reranked results not in descending score order: {} < {}",
                window[0].score,
                window[1].score
            );
        }

        // The scores should reflect the reranker's word overlap count.
        // ids[2] -> 3.0, ids[1] -> 1.0, ids[0] -> 0.0
        let top_hit = result
            .results
            .iter()
            .find(|h| h.memory.id == ids[2])
            .unwrap();
        assert!(
            (top_hit.score - 3.0).abs() < f64::EPSILON,
            "expected score 3.0 for 3-word overlap, got {}",
            top_hit.score
        );
    }

    #[test]
    fn search_reranker_threshold_excludes_low_scores() {
        use crate::embedding::MockReranker;

        let db = test_db();
        let emb = mock_embedder();

        // MockReranker scores by word overlap.
        // Query: "sqlite concurrent access"
        // doc0: 0 overlap words -> score 0.0
        // doc1: 1 overlap word  -> score 1.0
        // doc2: 3 overlap words -> score 3.0
        let contents = [
            "python is a dynamically typed language",
            "sqlite database storage engine",
            "sqlite uses wal mode for concurrent access",
        ];

        let ids: Vec<String> = contents
            .iter()
            .map(|content| {
                db.store(&StoreParams {
                    content,
                    memory_type: None,
                    projects: &[],
                    tags: &[],
                    links: &[],
                    embedding: &test_embedding(&emb, content),
                })
                .unwrap()
            })
            .collect();

        let reranker = MockReranker::new();
        let query = "sqlite concurrent access";
        let query_embedding = emb.embed_query(query).unwrap();

        // Threshold of 2.0 should exclude doc0 (0.0) and doc1 (1.0).
        let result = db
            .search(&SearchParams {
                query,
                query_embedding: &query_embedding,
                reranker: Some(&reranker),
                reranker_threshold: 2.0,
                ..Default::default()
            })
            .unwrap();

        // Only doc2 (score 3.0) should remain.
        assert_eq!(
            result.results.len(),
            1,
            "expected only 1 result above threshold"
        );
        assert_eq!(result.results[0].memory.id, ids[2]);
        // total should reflect the filtered count.
        assert_eq!(result.total, 1, "total should reflect filtered count");
    }

    #[test]
    fn search_reranker_pagination_applied_after_reranking() {
        use crate::embedding::MockReranker;

        let db = test_db();
        let emb = mock_embedder();

        // Query: "sqlite concurrent access"
        // Store 3 memories with different overlap.
        let contents = [
            "python is a dynamically typed language",     // score 0.0
            "sqlite database storage engine",             // score 1.0
            "sqlite uses wal mode for concurrent access", // score 3.0
        ];

        let ids: Vec<String> = contents
            .iter()
            .map(|content| {
                db.store(&StoreParams {
                    content,
                    memory_type: None,
                    projects: &[],
                    tags: &[],
                    links: &[],
                    embedding: &test_embedding(&emb, content),
                })
                .unwrap()
            })
            .collect();

        let reranker = MockReranker::new();
        let query = "sqlite concurrent access";
        let query_embedding = emb.embed_query(query).unwrap();

        // Page 1: limit=1, offset=0 -> should get the top reranked result (ids[2], score 3.0).
        let page1 = db
            .search(&SearchParams {
                query,
                query_embedding: &query_embedding,
                reranker: Some(&reranker),
                reranker_threshold: 0.0,
                limit: 1,
                offset: 0,
                ..Default::default()
            })
            .unwrap();

        assert_eq!(page1.results.len(), 1);
        assert_eq!(
            page1.results[0].memory.id, ids[2],
            "first page should have highest score"
        );
        assert_eq!(page1.total, 3, "total should reflect all reranked results");

        // Page 2: limit=1, offset=1 -> should get the second result (ids[1], score 1.0).
        let page2 = db
            .search(&SearchParams {
                query,
                query_embedding: &query_embedding,
                reranker: Some(&reranker),
                reranker_threshold: 0.0,
                limit: 1,
                offset: 1,
                ..Default::default()
            })
            .unwrap();

        assert_eq!(page2.results.len(), 1);
        assert_eq!(
            page2.results[0].memory.id, ids[1],
            "second page should have middle score"
        );
        assert_eq!(page2.total, 3);

        // Page 3: limit=1, offset=2 -> should get the lowest result (ids[0], score 0.0).
        let page3 = db
            .search(&SearchParams {
                query,
                query_embedding: &query_embedding,
                reranker: Some(&reranker),
                reranker_threshold: 0.0,
                limit: 1,
                offset: 2,
                ..Default::default()
            })
            .unwrap();

        assert_eq!(page3.results.len(), 1);
        assert_eq!(
            page3.results[0].memory.id, ids[0],
            "third page should have lowest score"
        );

        // No overlap between pages.
        assert_ne!(page1.results[0].memory.id, page2.results[0].memory.id);
        assert_ne!(page2.results[0].memory.id, page3.results[0].memory.id);
    }

    #[test]
    fn search_reranker_failure_falls_back_to_rrf_scores() {
        use crate::embedding::Reranker;

        /// A reranker that always fails — used to test graceful degradation.
        struct FailingReranker;
        impl Reranker for FailingReranker {
            fn rerank(&self, _query: &str, _documents: &[&str]) -> anyhow::Result<Vec<f32>> {
                anyhow::bail!("simulated reranker failure")
            }
        }

        let db = test_db();
        let emb = mock_embedder();

        db.store(&StoreParams {
            content: "sqlite uses wal mode for concurrent access",
            memory_type: None,
            projects: &[],
            tags: &[],
            links: &[],
            embedding: &test_embedding(&emb, "sqlite uses wal mode for concurrent access"),
        })
        .unwrap();

        let failing = FailingReranker;
        let query = "sqlite concurrent access";
        let query_embedding = emb.embed_query(query).unwrap();

        // Search should succeed despite reranker failure, returning RRF scores.
        let result = db
            .search(&SearchParams {
                query,
                query_embedding: &query_embedding,
                reranker: Some(&failing),
                reranker_threshold: 0.0,
                ..Default::default()
            })
            .unwrap();

        assert!(
            !result.results.is_empty(),
            "search should return RRF results when reranker fails"
        );
    }
}
