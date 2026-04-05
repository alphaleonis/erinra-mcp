# RFC: Decouple Sync Import from Embedder via Closure

## Problem

The `import()` function in `src/sync/mod.rs` takes `embedder: &dyn Embedder` as a parameter, creating a direct dependency from the sync module to the embedding module. The embedder is used at exactly one point (line 147): `embedder.embed_documents(&[&m.content])` to compute a vector before passing it to `db.import_memory()`.

This coupling means:
- The sync module imports `crate::embedding::Embedder`, creating an unnecessary module dependency
- Tests require `MockEmbedder` even when testing reconciliation logic (upsert, conflict resolution, tombstones)
- `background.rs` propagates the coupling: `SyncHandle` holds `Arc<dyn Embedder>` and threads it through `restore_from_peers`, `import_peer_file`, etc.

The coupling is narrow (one call site) but structurally real. Decoupling it is a ~20-line change.

## Proposed Interface

Replace `embedder: &dyn Embedder` with a generic closure parameter:

```rust
pub fn import<F>(
    db: &Database,
    embed: F,
    reader: &mut dyn Read,
) -> Result<ImportStats>
where
    F: Fn(&str) -> Result<Vec<f32>>,
```

Inside the function, line 147-151 changes from:
```rust
let embedding = embedder
    .embed_documents(&[&m.content])?
    .into_iter()
    .next()
    .context("embedder returned no vectors")?;
```
to:
```rust
let embedding = embed(&m.content)?;
```

The `use crate::embedding::Embedder;` import is removed from `sync/mod.rs`.

### Caller changes

A private helper in `background.rs` avoids repeating the closure body:

```rust
fn embed_one(embedder: &dyn Embedder, content: &str) -> Result<Vec<f32>> {
    embedder.embed_documents(&[content])?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("embedder returned no vectors"))
}
```

Call sites become: `import(&db, |c| embed_one(embedder.as_ref(), c), &mut reader)`

Tests become: `import(&db, |c| Ok(emb.embed_documents(&[c])?.remove(0)), &mut reader)`

## Dependency Strategy

**Category: In-process.** The closure is a pure `Fn(&str) -> Result<Vec<f32>>` — no I/O adapters, no trait objects crossing module boundaries. The `Embedder` trait stays in the embedding module; the sync module no longer references it.

`background.rs` retains its `Arc<dyn Embedder>` (it needs it for the closure). The coupling moves from sync's public API to background's private implementation — a net improvement since background is the integration layer.

## Testing Strategy

### New boundary tests to write

None needed — the existing tests continue to work with the closure syntax. The behavior is identical.

### Old tests to update (not delete)

All 7 `import()` call sites in `sync/mod.rs` tests change from `import(&db, &emb, ...)` to `import(&db, |c| Ok(emb.embed_documents(&[c])?.remove(0)), ...)`.

### Test environment needs

No change. Same `MockEmbedder::new(768)` + `Database::open_in_memory()`.

## Implementation Recommendations

### Changes in `src/sync/mod.rs`
- Remove `use crate::embedding::Embedder;` (line 12)
- Change `import()` signature to generic closure (line 127-131)
- Replace lines 147-151 with `let embedding = embed(&m.content)?;`
- Update 7 test call sites to closure syntax

### Changes in `src/sync/background.rs`
- Add private `embed_one()` helper function
- Update `import_from_file()`, `restore_from_peers()` to pass closure instead of `&dyn Embedder`
- `SyncHandle` keeps `Arc<dyn Embedder>` — it builds the closure when calling import functions

### What does NOT change
- `ImportMemoryParams.embedding` stays required (`&[f32]`, non-optional)
- One-at-a-time embedding behavior (no batching — see follow-up)
- `background.rs` still holds `Arc<dyn Embedder>` for constructing the closure
- All test assertions remain identical

### Follow-up: Two-Phase Import with Batch Embedding
This closure refactor enables but does not implement batch embedding. A future "two-phase import" would:
1. Parse + reconcile without embedding (phase 1)
2. Batch-embed all pending memories in one `embed_documents(&[N items])` call (phase 2)
3. Write all pending records to DB

This requires extracting a `Database::reconcile_memory()` read-only method from `import_memory()`. Created as a separate deferred nib.
