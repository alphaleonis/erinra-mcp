//! Seed a database for the Playwright e2e suite: `seed_test_db <data-dir>`.
//!
//! Embeddings come from `MockEmbedder`, so search hits rely on FTS5; the stored
//! model name and dimensions match the default config the e2e daemon runs with.

use anyhow::{Context, Result};
use erinra::db::types::StoreParams;
use erinra::db::{Database, DbConfig};
use erinra::embedding::{Embedder, MockEmbedder};

struct Seed {
    content: &'static str,
    memory_type: &'static str,
    projects: &'static [&'static str],
    tags: &'static [&'static str],
}

const SEEDS: &[Seed] = &[
    Seed {
        content: "Erinra stores memories in SQLite with WAL mode so several sessions can share one database.",
        memory_type: "decision",
        projects: &["erinra"],
        tags: &["sqlite", "architecture"],
    },
    Seed {
        content: "Hybrid search merges sqlite-vec embedding similarity with FTS5 keyword matches using reciprocal rank fusion.",
        memory_type: "pattern",
        projects: &["erinra"],
        tags: &["search"],
    },
    Seed {
        content: "The local embedding model is Nomic Embed Text v1.5, loaded through fastembed on first start.",
        memory_type: "fact",
        projects: &["erinra"],
        tags: &["embedding"],
    },
    Seed {
        content: "Vestige keeps its task queue in a separate SQLite file to avoid lock contention with the main store.",
        memory_type: "decision",
        projects: &["vestige"],
        tags: &["sqlite"],
    },
    Seed {
        content: "Prefer archiving memories over deleting them; archive is reversible and keeps sync history intact.",
        memory_type: "preference",
        projects: &["erinra", "vestige"],
        tags: &["workflow"],
    },
    Seed {
        content: "BUG FIX: daemon advertised Ready before binding its port | Root cause: publish ran before bind | Solution: bind first.",
        memory_type: "bug-fix",
        projects: &["erinra"],
        tags: &["bug-fix", "daemon"],
    },
];

fn main() -> Result<()> {
    let data_dir = std::env::args()
        .nth(1)
        .context("usage: seed_test_db <data-dir>")?;
    let data_dir = std::path::Path::new(&data_dir);
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create {}", data_dir.display()))?;

    let config = DbConfig::default();
    let embedder = MockEmbedder::new(config.embedding_dimensions);
    let db = Database::open(&data_dir.join("db.sqlite"), &config)?;

    let mut ids = Vec::with_capacity(SEEDS.len());
    for seed in SEEDS {
        let embedding = embedder
            .embed_documents(&[seed.content])?
            .pop()
            .context("embedder returned no vector")?;
        let id = db.store(&StoreParams {
            content: seed.content,
            memory_type: Some(seed.memory_type),
            projects: seed.projects,
            tags: seed.tags,
            links: &[],
            embedding: &embedding,
        })?;
        ids.push(id);
    }

    db.link(&ids[1], &ids[0], "related_to")?;
    db.link(&ids[1], &ids[2], "context_for")?;
    db.link(&ids[5], &ids[0], "related_to")?;

    println!("seeded {} memories into {}", ids.len(), data_dir.display());
    Ok(())
}
