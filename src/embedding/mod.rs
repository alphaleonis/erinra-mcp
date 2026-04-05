//! Embedder and Reranker traits, fastembed wrappers, and mock implementations for tests.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

/// Trait abstracting over embedding generation, enabling mock substitution in tests.
///
/// All methods are synchronous. The caller (MCP handler) can use `spawn_blocking`
/// if needed to avoid blocking the async runtime during CPU-bound inference.
pub trait Embedder: Send + Sync {
    /// Embed texts intended for storage. Implementations prepend appropriate
    /// document prefixes (e.g. "search_document: " for Nomic models).
    fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Embed a single query for search. Implementations prepend appropriate
    /// query prefixes (e.g. "search_query: " for Nomic models).
    fn embed_query(&self, query: &str) -> Result<Vec<f32>>;

    /// Number of dimensions in the output vectors.
    fn dimensions(&self) -> u32;

    /// Embed a single document, returning exactly one vector.
    fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_documents(&[text])?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedder returned no vectors"))
    }
}

/// Production embedder wrapping fastembed's `TextEmbedding`.
///
/// `TextEmbedding::embed()` requires `&mut self`, so the model is wrapped in a
/// `Mutex` for safe concurrent access from multiple tool calls.
pub struct FastembedEmbedder {
    model: Mutex<TextEmbedding>,
    dims: u32,
}

impl FastembedEmbedder {
    /// Create a new embedder with the given model name and cache directory.
    ///
    /// Model names map to fastembed's `EmbeddingModel` enum variants (e.g.
    /// `"NomicEmbedTextV15Q"`, `"AllMiniLML6V2"`). Downloads the ONNX model
    /// on first use if not already cached.
    pub fn new(model_name: &str, cache_dir: PathBuf) -> Result<Self> {
        let model_enum = parse_model_name(model_name)?;
        let info =
            TextEmbedding::get_model_info(&model_enum).context("unsupported embedding model")?;
        let dims = info.dim as u32;

        let options = TextInitOptions::new(model_enum)
            .with_cache_dir(cache_dir)
            .with_show_download_progress(true);
        let model =
            TextEmbedding::try_new(options).context("failed to initialize embedding model")?;

        Ok(Self {
            model: Mutex::new(model),
            dims,
        })
    }
}

/// Supported embedding models: name, dimensions, and description.
pub const SUPPORTED_MODELS: &[(&str, u32, &str)] = &[
    ("NomicEmbedTextV1", 768, "Nomic Embed Text v1"),
    ("NomicEmbedTextV15", 768, "Nomic Embed Text v1.5"),
    (
        "NomicEmbedTextV15Q",
        768,
        "Nomic Embed Text v1.5 (quantized, default)",
    ),
    ("AllMiniLML6V2", 384, "All-MiniLM-L6-v2"),
    ("AllMiniLML6V2Q", 384, "All-MiniLM-L6-v2 (quantized)"),
    ("AllMiniLML12V2", 384, "All-MiniLM-L12-v2"),
    ("AllMiniLML12V2Q", 384, "All-MiniLM-L12-v2 (quantized)"),
    ("BGEBaseENV15", 768, "BGE Base EN v1.5"),
    ("BGEBaseENV15Q", 768, "BGE Base EN v1.5 (quantized)"),
    ("BGESmallENV15", 384, "BGE Small EN v1.5"),
    ("BGESmallENV15Q", 384, "BGE Small EN v1.5 (quantized)"),
    ("BGELargeENV15", 1024, "BGE Large EN v1.5"),
    ("BGELargeENV15Q", 1024, "BGE Large EN v1.5 (quantized)"),
];

/// Map a model name string to the fastembed `EmbeddingModel` enum.
fn parse_model_name(name: &str) -> Result<EmbeddingModel> {
    match name {
        "NomicEmbedTextV1" => Ok(EmbeddingModel::NomicEmbedTextV1),
        "NomicEmbedTextV15" => Ok(EmbeddingModel::NomicEmbedTextV15),
        "NomicEmbedTextV15Q" => Ok(EmbeddingModel::NomicEmbedTextV15Q),
        "AllMiniLML6V2" => Ok(EmbeddingModel::AllMiniLML6V2),
        "AllMiniLML6V2Q" => Ok(EmbeddingModel::AllMiniLML6V2Q),
        "AllMiniLML12V2" => Ok(EmbeddingModel::AllMiniLML12V2),
        "AllMiniLML12V2Q" => Ok(EmbeddingModel::AllMiniLML12V2Q),
        "BGEBaseENV15" => Ok(EmbeddingModel::BGEBaseENV15),
        "BGEBaseENV15Q" => Ok(EmbeddingModel::BGEBaseENV15Q),
        "BGESmallENV15" => Ok(EmbeddingModel::BGESmallENV15),
        "BGESmallENV15Q" => Ok(EmbeddingModel::BGESmallENV15Q),
        "BGELargeENV15" => Ok(EmbeddingModel::BGELargeENV15),
        "BGELargeENV15Q" => Ok(EmbeddingModel::BGELargeENV15Q),
        _ => bail!(
            "unknown embedding model: '{name}'. Run `erinra models` to list available models."
        ),
    }
}

impl Embedder for FastembedEmbedder {
    fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let prefixed: Vec<String> = texts
            .iter()
            .map(|t| format!("search_document: {t}"))
            .collect();
        let mut model = self.model.lock().expect("embedding model mutex poisoned");
        // batch_size=None required for dynamically quantized models (e.g. NomicEmbedTextV15Q).
        let embeddings = model
            .embed(prefixed, None)
            .context("embedding generation failed")?;
        Ok(embeddings)
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let prefixed = format!("search_query: {query}");
        let mut model = self.model.lock().expect("embedding model mutex poisoned");
        let embeddings = model
            .embed(vec![prefixed], None)
            .context("embedding generation failed")?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedding model returned no vectors for query"))
    }

    fn dimensions(&self) -> u32 {
        self.dims
    }
}

// ── Reranker ───────────────────────────────────────────────────────────

/// Supported reranker models: (name, description).
pub const SUPPORTED_RERANKER_MODELS: &[(&str, &str)] = &[
    (
        "JINARerankerV1TurboEn",
        "Jina v1 Turbo English (fast, 151 MB)",
    ),
    (
        "BGERerankerBase",
        "BGE Reranker Base (English + Chinese, 1.1 GB)",
    ),
    (
        "JINARerankerV2BaseMultiligual",
        "Jina v2 Base Multilingual (1.1 GB)",
    ),
    (
        "BGERerankerV2M3",
        "BGE Reranker v2 M3 Multilingual (2.3 GB)",
    ),
];

/// Map a reranker model name string to the fastembed `RerankerModel` enum.
pub fn parse_reranker_model_name(name: &str) -> Result<fastembed::RerankerModel> {
    match name {
        "JINARerankerV1TurboEn" => Ok(fastembed::RerankerModel::JINARerankerV1TurboEn),
        "BGERerankerBase" => Ok(fastembed::RerankerModel::BGERerankerBase),
        "JINARerankerV2BaseMultiligual" => {
            Ok(fastembed::RerankerModel::JINARerankerV2BaseMultiligual)
        }
        "BGERerankerV2M3" => Ok(fastembed::RerankerModel::BGERerankerV2M3),
        _ => {
            bail!("unknown reranker model: '{name}'. Run `erinra models` to list available models.")
        }
    }
}

// ── Reranker trait ─────────────────────────────────────────────────────

/// Reranker scores (query, document) pairs for relevance.
/// Higher scores = more relevant. Scores can be negative (irrelevant).
pub trait Reranker: Send + Sync {
    fn rerank(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>>;
}

/// Production reranker wrapping fastembed's cross-encoder models.
///
/// `TextRerank::rerank()` requires `&mut self`, so the model is wrapped in a
/// `Mutex` for safe concurrent access from multiple tool calls.
pub struct FastembedReranker {
    model: Mutex<fastembed::TextRerank>,
}

impl FastembedReranker {
    /// Create a new reranker with the given model name and cache directory.
    ///
    /// Model names map to fastembed's `RerankerModel` enum variants (e.g.
    /// `"JINARerankerV1TurboEn"`, `"BGERerankerBase"`). Downloads the ONNX model
    /// on first use if not already cached.
    pub fn new(model_name: &str, cache_dir: PathBuf) -> Result<Self> {
        let model_enum = parse_reranker_model_name(model_name)?;
        let options = fastembed::RerankInitOptions::new(model_enum)
            .with_cache_dir(cache_dir)
            .with_show_download_progress(true);
        let model = fastembed::TextRerank::try_new(options)
            .context("failed to initialize reranker model")?;
        Ok(Self {
            model: Mutex::new(model),
        })
    }
}

impl Reranker for FastembedReranker {
    fn rerank(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>> {
        let mut model = self.model.lock().expect("reranker model mutex poisoned");
        let results = model
            .rerank(query, documents, false, None)
            .context("reranking failed")?;
        // fastembed returns results sorted by score descending.
        // We need scores in ORIGINAL document order (by index).
        let mut scores = vec![0.0f32; documents.len()];
        for result in results {
            scores[result.index] = result.score;
        }
        Ok(scores)
    }
}

/// Test reranker that scores based on word overlap between query and document.
/// Deterministic and fast — no model download needed.
#[cfg(any(test, feature = "test-utils"))]
pub struct MockReranker;

#[cfg(any(test, feature = "test-utils"))]
impl MockReranker {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl Reranker for MockReranker {
    fn rerank(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>> {
        let query_words: std::collections::HashSet<String> = query
            .split_whitespace()
            .map(|w| {
                w.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
            })
            .filter(|w| !w.is_empty())
            .collect();
        Ok(documents
            .iter()
            .map(|doc| {
                let doc_words: std::collections::HashSet<String> = doc
                    .split_whitespace()
                    .map(|w| {
                        w.trim_matches(|c: char| !c.is_alphanumeric())
                            .to_lowercase()
                    })
                    .filter(|w| !w.is_empty())
                    .collect();
                query_words.intersection(&doc_words).count() as f32
            })
            .collect())
    }
}

/// Deterministic mock embedder for tests. Produces hash-seeded unit vectors
/// so the same text always yields the same embedding, without downloading models.
#[cfg(any(test, feature = "test-utils"))]
pub struct MockEmbedder {
    dims: u32,
}

#[cfg(any(test, feature = "test-utils"))]
impl MockEmbedder {
    pub fn new(dims: u32) -> Self {
        Self { dims }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl Embedder for MockEmbedder {
    fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| hash_to_vector(&format!("search_document: {t}"), self.dims))
            .collect())
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        Ok(hash_to_vector(&format!("search_query: {query}"), self.dims))
    }

    fn dimensions(&self) -> u32 {
        self.dims
    }
}

/// Generate a deterministic unit vector from text via hashing.
/// Uses FNV-1a for stability across Rust versions (unlike DefaultHasher).
#[cfg(any(test, feature = "test-utils"))]
fn hash_to_vector(text: &str, dims: u32) -> Vec<f32> {
    // FNV-1a 64-bit hash — stable, simple, no external dependency.
    let mut seed: u64 = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        seed ^= *byte as u64;
        seed = seed.wrapping_mul(0x100000001b3);
    }

    let mut vec = Vec::with_capacity(dims as usize);
    for _ in 0..dims {
        // xorshift64 PRNG
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        vec.push((seed as f64 / u64::MAX as f64 * 2.0 - 1.0) as f32);
    }

    // Normalize to unit vector so cosine similarity works correctly.
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut vec {
            *v /= norm;
        }
    }

    vec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_dimensions() {
        let embedder = MockEmbedder::new(768);
        assert_eq!(embedder.dimensions(), 768);
    }

    #[test]
    fn mock_embed_documents_correct_shape() {
        let embedder = MockEmbedder::new(768);
        let results = embedder
            .embed_documents(&["hello world", "another text"])
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].len(), 768);
        assert_eq!(results[1].len(), 768);
    }

    #[test]
    fn mock_embed_query_correct_shape() {
        let embedder = MockEmbedder::new(768);
        let result = embedder.embed_query("test query").unwrap();
        assert_eq!(result.len(), 768);
    }

    #[test]
    fn mock_deterministic() {
        let embedder = MockEmbedder::new(128);
        let a = embedder.embed_query("same text").unwrap();
        let b = embedder.embed_query("same text").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn mock_different_texts_different_vectors() {
        let embedder = MockEmbedder::new(128);
        let a = embedder.embed_query("text one").unwrap();
        let b = embedder.embed_query("text two").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn mock_produces_unit_vectors() {
        let embedder = MockEmbedder::new(768);
        let vec = embedder.embed_query("normalize me").unwrap();
        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "expected unit vector, got norm={norm}"
        );
    }

    #[test]
    fn mock_empty_input() {
        let embedder = MockEmbedder::new(768);
        let results = embedder.embed_documents(&[]).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn mock_usable_as_trait_object() {
        let embedder: Box<dyn Embedder> = Box::new(MockEmbedder::new(768));
        assert_eq!(embedder.dimensions(), 768);
        let vec = embedder.embed_query("trait object test").unwrap();
        assert_eq!(vec.len(), 768);
    }

    /// Embedder that always returns an empty vec from embed_documents.
    struct EmptyEmbedder;
    impl Embedder for EmptyEmbedder {
        fn embed_documents(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(vec![])
        }
        fn embed_query(&self, _query: &str) -> Result<Vec<f32>> {
            Ok(vec![])
        }
        fn dimensions(&self) -> u32 {
            128
        }
    }

    #[test]
    fn embed_one_error_on_empty_response() {
        let embedder = EmptyEmbedder;
        let err = embedder.embed_one("anything").unwrap_err();
        assert!(
            err.to_string().contains("no vectors"),
            "expected 'no vectors' in error, got: {err}"
        );
    }

    #[test]
    fn embed_one_returns_same_vector_as_embed_documents() {
        let embedder = MockEmbedder::new(128);
        let text = "hello world";
        let one = embedder.embed_one(text).unwrap();
        let batch = embedder.embed_documents(&[text]).unwrap();
        assert_eq!(one, batch[0]);
    }

    #[test]
    fn mock_reranker_scores_by_word_overlap() {
        let reranker = MockReranker::new();
        let query = "sqlite concurrent access";
        let docs = &[
            "sqlite uses wal mode for concurrent access",
            "python is a dynamically typed language",
            "sqlite database storage",
        ];
        let scores = reranker.rerank(query, docs).unwrap();
        assert_eq!(scores.len(), 3);
        // doc0 shares "sqlite", "concurrent", "access" => highest
        // doc2 shares "sqlite" => middle
        // doc1 shares nothing => lowest
        assert!(
            scores[0] > scores[2],
            "doc with 3 shared words should score higher than doc with 1: {} vs {}",
            scores[0],
            scores[2]
        );
        assert!(
            scores[2] > scores[1],
            "doc with 1 shared word should score higher than doc with 0: {} vs {}",
            scores[2],
            scores[1]
        );
    }

    #[test]
    fn mock_reranker_empty_documents() {
        let reranker = MockReranker::new();
        let scores = reranker.rerank("some query", &[]).unwrap();
        assert!(scores.is_empty());
    }

    #[test]
    fn mock_reranker_usable_as_trait_object() {
        let reranker: Box<dyn Reranker> = Box::new(MockReranker::new());
        let scores = reranker.rerank("test", &["test document"]).unwrap();
        assert_eq!(scores.len(), 1);
    }

    #[test]
    fn parse_reranker_model_name_all_supported() {
        // Every model in SUPPORTED_RERANKER_MODELS must parse to a valid enum variant.
        assert!(!SUPPORTED_RERANKER_MODELS.is_empty());
        for &(name, _) in SUPPORTED_RERANKER_MODELS {
            parse_reranker_model_name(name).unwrap_or_else(|_| {
                panic!("SUPPORTED_RERANKER_MODELS entry '{name}' should parse")
            });
        }
    }

    #[test]
    fn parse_reranker_model_name_unknown() {
        let err = parse_reranker_model_name("NonExistentModel").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown reranker model"),
            "error should mention 'unknown reranker model', got: {msg}"
        );
    }
}
