//! Shared helper functions used across the ops_* modules.

use anyhow::Result;
use rusqlite::types::Value;

use super::types::*;

/// Convert f32 embedding slice to little-endian bytes for SQLite storage.
pub(super) fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// SQL WHERE clause with its bound parameters, kept as a cohesive unit.
pub(super) struct SearchFilter {
    pub(super) sql: String,
    pub(super) params: Vec<Value>,
}

impl SearchFilter {
    /// Append a single-parameter predicate with AND-joining.
    /// The predicate must contain exactly one `?` placeholder.
    /// No longer used in production (time filters moved to `build_base_filter`).
    #[cfg(test)]
    pub(super) fn push(&mut self, predicate: impl Into<String>, param: Value) {
        if !self.sql.is_empty() {
            self.sql.push_str(" AND ");
        }
        self.sql.push_str(&predicate.into());
        self.params.push(param);
    }
}

/// Map a database row to a Memory struct.
///
/// Projects and tags are left empty — callers fill them via `fill_projects_and_tags`.
/// Truncation is handled post-query via `Memory::truncate()`.
///
/// Expected column order: id, content, type, created_at, updated_at,
///     archived_at, last_accessed_at, access_count
pub(super) fn map_memory_row(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        content: row.get(1)?,
        memory_type: row.get(2)?,
        projects: vec![],
        tags: vec![],
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        archived_at: row.get(5)?,
        last_accessed_at: row.get(6)?,
        access_count: row.get(7)?,
        truncated: false,
    })
}

/// Build the shared WHERE clause from common filter fields.
pub(super) fn build_base_filter(f: &FilterParams) -> SearchFilter {
    let mut parts: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    if !f.include_archived {
        parts.push("m.archived_at IS NULL".into());
    }

    if let Some(projects) = f.projects
        && !projects.is_empty()
    {
        let ph = push_text_params(projects, &mut params);
        let mut clause =
            format!("m.id IN (SELECT memory_id FROM memory_projects WHERE project IN ({ph}))");
        if f.include_global {
            clause = format!(
                "({clause} OR NOT EXISTS \
                     (SELECT 1 FROM memory_projects WHERE memory_id = m.id))"
            );
        }
        parts.push(clause);
    }

    if let Some(memory_type) = f.memory_type {
        params.push(Value::Text(memory_type.to_string()));
        parts.push("m.type = ?".into());
    }

    if let Some(tags) = f.tags
        && !tags.is_empty()
    {
        let tag_count = tags.len();
        let ph = push_text_params(tags, &mut params);
        params.push(Value::Integer(tag_count as i64));
        parts.push(format!(
            "m.id IN (SELECT memory_id FROM tags WHERE tag IN ({ph}) \
                 GROUP BY memory_id HAVING COUNT(DISTINCT tag) = ?)"
        ));
    }

    // Time filters.
    if let Some(after) = f.time.created_after {
        params.push(Value::Text(after.to_string()));
        parts.push("m.created_at >= ?".into());
    }
    if let Some(before) = f.time.created_before {
        params.push(Value::Text(before.to_string()));
        parts.push("m.created_at < ?".into());
    }
    if let Some(after) = f.time.updated_after {
        params.push(Value::Text(after.to_string()));
        parts.push("m.updated_at >= ?".into());
    }
    if let Some(before) = f.time.updated_before {
        params.push(Value::Text(before.to_string()));
        parts.push("m.updated_at < ?".into());
    }

    SearchFilter {
        sql: parts.join(" AND "),
        params,
    }
}

/// Build a comma-separated list of `?` placeholders and push values into the params vec.
fn push_text_params(values: &[&str], params: &mut Vec<Value>) -> String {
    values
        .iter()
        .map(|v| {
            params.push(Value::Text(v.to_string()));
            "?".to_string()
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Insert project associations for a memory.
pub(super) fn insert_projects(
    conn: &rusqlite::Connection,
    memory_id: &str,
    projects: &[&str],
) -> Result<()> {
    let mut stmt =
        conn.prepare_cached("INSERT INTO memory_projects (memory_id, project) VALUES (?1, ?2)")?;
    for project in projects {
        stmt.execute(rusqlite::params![memory_id, project])?;
    }
    Ok(())
}

/// Insert tag associations for a memory.
pub(super) fn insert_tags(
    conn: &rusqlite::Connection,
    memory_id: &str,
    tags: &[&str],
) -> Result<()> {
    let mut stmt = conn.prepare_cached("INSERT INTO tags (memory_id, tag) VALUES (?1, ?2)")?;
    for tag in tags {
        stmt.execute(rusqlite::params![memory_id, tag])?;
    }
    Ok(())
}

/// Map a database row to a Link struct.
pub(super) fn map_link(row: &rusqlite::Row) -> rusqlite::Result<Link> {
    Ok(Link {
        id: row.get(0)?,
        source_id: row.get(1)?,
        target_id: row.get(2)?,
        relation: row.get(3)?,
        created_at: row.get(4)?,
        content: None,
    })
}

/// Fill in the `projects` and `tags` fields for a slice of memories.
/// Accepts a `&rusqlite::Connection` so callers can pass either `self.conn()` or a `&Transaction`.
pub(super) fn fill_projects_and_tags(
    conn: &rusqlite::Connection,
    memories: &mut [Memory],
    ids: &[&str],
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }

    let placeholders = vec!["?"; ids.len()].join(",");
    let id_values: Vec<Value> = ids.iter().map(|id| Value::Text(id.to_string())).collect();

    // Projects.
    let sql = format!(
        "SELECT memory_id, project FROM memory_projects \
         WHERE memory_id IN ({placeholders}) ORDER BY project"
    );
    let mut stmt = conn.prepare(&sql)?;
    let project_rows: Vec<(String, String)> = stmt
        .query_map(
            rusqlite::params_from_iter(&id_values),
            |row: &rusqlite::Row<'_>| Ok((row.get(0)?, row.get(1)?)),
        )?
        .collect::<Result<_, _>>()?;

    // Tags.
    let sql = format!(
        "SELECT memory_id, tag FROM tags \
         WHERE memory_id IN ({placeholders}) ORDER BY tag"
    );
    let mut stmt = conn.prepare(&sql)?;
    let tag_rows: Vec<(String, String)> = stmt
        .query_map(
            rusqlite::params_from_iter(&id_values),
            |row: &rusqlite::Row<'_>| Ok((row.get(0)?, row.get(1)?)),
        )?
        .collect::<Result<_, _>>()?;

    for mem in memories.iter_mut() {
        mem.projects = project_rows
            .iter()
            .filter(|(mid, _)| mid == &mem.id)
            .map(|(_, p)| p.clone())
            .collect();
        mem.tags = tag_rows
            .iter()
            .filter(|(mid, _)| mid == &mem.id)
            .map(|(_, t)| t.clone())
            .collect();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_base_filter_empty_params_produces_empty_sql() {
        let f = FilterParams {
            include_archived: true, // skip the default archived_at IS NULL
            ..Default::default()
        };
        let filter = build_base_filter(&f);
        assert!(
            filter.sql.is_empty(),
            "expected empty SQL, got: {:?}",
            filter.sql
        );
        assert!(filter.params.is_empty(), "expected no params");
    }

    #[test]
    fn build_base_filter_archived_false_produces_is_null() {
        let f = FilterParams {
            include_archived: false,
            ..Default::default()
        };
        let filter = build_base_filter(&f);
        assert_eq!(filter.sql, "m.archived_at IS NULL");
        assert!(filter.params.is_empty(), "archived filter needs no params");
    }

    #[test]
    fn build_base_filter_projects_no_global() {
        let f = FilterParams {
            projects: Some(&["proj-a", "proj-b"]),
            include_global: false,
            include_archived: true,
            ..Default::default()
        };
        let filter = build_base_filter(&f);
        assert!(
            filter
                .sql
                .contains("SELECT memory_id FROM memory_projects WHERE project IN (?,?)"),
            "expected IN-subquery, got: {:?}",
            filter.sql
        );
        // Should NOT contain OR NOT EXISTS (no include_global).
        assert!(
            !filter.sql.contains("OR NOT EXISTS"),
            "should not include global clause"
        );
        assert_eq!(filter.params.len(), 2);
        assert_eq!(filter.params[0], Value::Text("proj-a".into()));
        assert_eq!(filter.params[1], Value::Text("proj-b".into()));
    }

    #[test]
    fn build_base_filter_projects_with_global() {
        let f = FilterParams {
            projects: Some(&["proj-a"]),
            include_global: true,
            include_archived: true,
            ..Default::default()
        };
        let filter = build_base_filter(&f);
        assert!(
            filter.sql.contains("OR NOT EXISTS"),
            "expected global inclusion clause, got: {:?}",
            filter.sql
        );
        assert!(
            filter
                .sql
                .contains("SELECT memory_id FROM memory_projects WHERE project IN (?)"),
            "expected project IN clause, got: {:?}",
            filter.sql
        );
        assert_eq!(filter.params.len(), 1);
        assert_eq!(filter.params[0], Value::Text("proj-a".into()));
    }

    #[test]
    fn build_base_filter_tags_and_semantics() {
        let f = FilterParams {
            tags: Some(&["rust", "testing"]),
            include_archived: true,
            ..Default::default()
        };
        let filter = build_base_filter(&f);
        assert!(
            filter
                .sql
                .contains("SELECT memory_id FROM tags WHERE tag IN (?,?)"),
            "expected tags IN-subquery, got: {:?}",
            filter.sql
        );
        assert!(
            filter.sql.contains("HAVING COUNT(DISTINCT tag) = ?"),
            "expected AND-semantics HAVING clause, got: {:?}",
            filter.sql
        );
        // 2 tag values + 1 count param = 3 total.
        assert_eq!(filter.params.len(), 3, "params: {:?}", filter.params);
        assert_eq!(filter.params[0], Value::Text("rust".into()));
        assert_eq!(filter.params[1], Value::Text("testing".into()));
        assert_eq!(filter.params[2], Value::Integer(2));
    }

    #[test]
    fn build_base_filter_memory_type() {
        let f = FilterParams {
            memory_type: Some("pattern"),
            include_archived: true,
            ..Default::default()
        };
        let filter = build_base_filter(&f);
        assert_eq!(filter.sql, "m.type = ?");
        assert_eq!(filter.params.len(), 1);
        assert_eq!(filter.params[0], Value::Text("pattern".into()));
    }

    #[test]
    fn build_base_filter_combined_fields() {
        let f = FilterParams {
            projects: Some(&["proj-a"]),
            memory_type: Some("pattern"),
            tags: Some(&["rust"]),
            include_global: false,
            include_archived: false,
            time: TimeFilters::default(),
        };
        let filter = build_base_filter(&f);
        // 4 AND-joined clauses: archived, projects, type, tags
        assert_eq!(filter.sql.matches(" AND ").count(), 3);
        // project param + type param + tag param + tag count = 4 total
        assert_eq!(filter.params.len(), 4);
    }

    #[test]
    fn build_base_filter_time_filters() {
        let f = FilterParams {
            include_archived: true,
            time: TimeFilters {
                created_after: Some("2025-01-01T00:00:00Z"),
                created_before: Some("2026-01-01T00:00:00Z"),
                updated_after: Some("2025-06-01T00:00:00Z"),
                updated_before: None,
            },
            ..Default::default()
        };
        let filter = build_base_filter(&f);
        assert!(
            filter.sql.contains("m.created_at >= ?"),
            "should have created_after clause: {}",
            filter.sql
        );
        assert!(
            filter.sql.contains("m.created_at < ?"),
            "should have created_before clause: {}",
            filter.sql
        );
        assert!(
            filter.sql.contains("m.updated_at >= ?"),
            "should have updated_after clause: {}",
            filter.sql
        );
        assert!(
            !filter.sql.contains("m.updated_at < ?"),
            "should not have updated_before clause: {}",
            filter.sql
        );
        // 3 time params
        assert_eq!(filter.params.len(), 3);
        assert_eq!(filter.params[0], Value::Text("2025-01-01T00:00:00Z".into()));
        assert_eq!(filter.params[1], Value::Text("2026-01-01T00:00:00Z".into()));
        assert_eq!(filter.params[2], Value::Text("2025-06-01T00:00:00Z".into()));
    }

    #[test]
    fn search_filter_push_joins_with_and() {
        let mut filter = SearchFilter {
            sql: String::new(),
            params: Vec::new(),
        };

        // First push — no AND prefix.
        filter.push("m.created_at >= ?", Value::Text("2025-01-01".into()));
        assert_eq!(filter.sql, "m.created_at >= ?");
        assert_eq!(filter.params.len(), 1);

        // Second push — AND-joined.
        filter.push("m.updated_at >= ?", Value::Text("2025-06-01".into()));
        assert_eq!(filter.sql, "m.created_at >= ? AND m.updated_at >= ?");
        assert_eq!(filter.params.len(), 2);
    }

    #[test]
    fn search_filter_push_appends_to_existing_base() {
        let f = FilterParams {
            include_archived: false,
            ..Default::default()
        };
        let mut filter = build_base_filter(&f);
        assert_eq!(filter.sql, "m.archived_at IS NULL");

        filter.push("m.created_at >= ?", Value::Text("2025-01-01".into()));
        assert_eq!(filter.sql, "m.archived_at IS NULL AND m.created_at >= ?");
        assert_eq!(filter.params.len(), 1);
    }
}
