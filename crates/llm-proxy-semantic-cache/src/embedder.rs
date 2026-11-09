//! Prompt embeddings: the trait a semantic cache embeds through, and the
//! offline default.

use std::collections::HashMap;

use async_trait::async_trait;

/// One prompt's embedding (any fixed dimensionality; vectors are compared
/// by cosine similarity, so magnitude does not matter — the default
/// embedder normalizes).
pub type Embedding = Vec<f32>;

/// An embedder failure.
#[derive(Debug, thiserror::Error)]
#[error("embedding failed: {0}")]
pub struct EmbedError(pub String);

/// Turns prompts into vectors. The default [`NgramEmbedder`] is offline
/// and deterministic; an embeddings-model-backed implementation slots in
/// behind the same trait.
#[async_trait]
pub trait Embedder: Send + Sync + std::fmt::Debug {
    /// Embed each text; the result order matches the input order.
    ///
    /// # Errors
    ///
    /// [`EmbedError`] when the embedder cannot produce vectors.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, EmbedError>;
}

/// The offline default: a normalized bag of hashed character trigrams.
/// Deterministic (no randomness, no network), it measures lexical
/// similarity — a paraphrase keeps most of its trigrams, an unrelated
/// prompt shares few. Not a model embedding: semantic paraphrases that
/// share no wording will not match, and the threshold says how close is
/// close enough.
#[derive(Debug, Clone, Copy)]
pub struct NgramEmbedder {
    dimension: usize,
}

impl Default for NgramEmbedder {
    // Manual on purpose: a derived Default would leave `dimension` at 0
    // and `bucket_of` would divide by zero on the first embed. Default
    // must mean the same embedder as `new()`.
    fn default() -> Self {
        Self::new()
    }
}

/// The default vector width.
pub(crate) const DEFAULT_EMBEDDING_DIMENSION: usize = 512;

impl NgramEmbedder {
    /// The default 512-dimension embedder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dimension: DEFAULT_EMBEDDING_DIMENSION,
        }
    }

    /// An embedder with a custom vector width.
    #[must_use]
    pub fn with_dimension(dimension: usize) -> Self {
        Self {
            dimension: dimension.max(8),
        }
    }
}

fn trigrams(text: &str) -> Vec<String> {
    let padded = format!("^{text}$");
    let chars: Vec<char> = padded.chars().collect();
    if chars.len() < 3 {
        return vec![padded];
    }
    chars
        .windows(3)
        .map(|window| window.iter().collect())
        .collect()
}

fn bucket_of(trigram: &str, dimension: usize) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in trigram.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    (hash % dimension as u64) as usize
}

fn embed_one(text: &str, dimension: usize) -> Embedding {
    let mut counts: HashMap<usize, f32> = HashMap::new();
    for trigram in trigrams(text) {
        *counts.entry(bucket_of(&trigram, dimension)).or_insert(0.0) += 1.0;
    }
    let mut vector = vec![0.0_f32; dimension];
    let mut norm = 0.0_f32;
    for (bucket, count) in counts {
        // `bucket` is `hash % dimension`, so the slot always exists; the
        // checked form keeps the norm consistent with the vector should
        // that invariant ever break, instead of panicking.
        if let Some(component) = vector.get_mut(bucket) {
            *component = count;
            norm += count * count;
        }
    }
    let norm = norm.sqrt();
    if norm > f32::EPSILON {
        for component in &mut vector {
            *component /= norm;
        }
    }
    vector
}

#[async_trait]
impl Embedder for NgramEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, EmbedError> {
        Ok(texts
            .iter()
            .map(|text| embed_one(text, self.dimension))
            .collect())
    }
}

/// Cosine similarity of two vectors (both assumed non-zero).
#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denominator = norm_a.sqrt() * norm_b.sqrt();
    if denominator <= f32::EPSILON {
        return 0.0;
    }
    dot / denominator
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_prompts_score_one() {
        let embedder = NgramEmbedder::new();
        let a = embed_one("what is the capital of france", embedder.dimension);
        let b = embed_one("what is the capital of france", embedder.dimension);
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn paraphrases_score_high_unrelated_prompts_score_low() {
        let embedder = NgramEmbedder::new();
        let question = embed_one("what is the capital of france", embedder.dimension);
        let paraphrase = embed_one("what is france's capital city", embedder.dimension);
        let other = embed_one("write a rust function to sort a list", embedder.dimension);
        assert!(
            cosine_similarity(&question, &paraphrase) > cosine_similarity(&question, &other),
            "a paraphrase must score higher than an unrelated prompt"
        );
    }

    #[tokio::test]
    async fn embedding_is_deterministic_and_order_preserving() {
        let embedder = NgramEmbedder::new();
        let first = embedder
            .embed(&["alpha".to_owned(), "beta".to_owned()])
            .await
            .expect("embed");
        let second = embedder
            .embed(&["alpha".to_owned(), "beta".to_owned()])
            .await
            .expect("embed");
        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
    }

    #[tokio::test]
    async fn the_default_embedder_embeds_exactly_like_new() {
        // `NgramEmbedder::default()` used to carry dimension 0 and panic
        // on its first modulo; it must behave identically to `::new()`.
        let text = "what is the capital of france".to_owned();
        let from_default = NgramEmbedder::default()
            .embed(std::slice::from_ref(&text))
            .await
            .expect("default embeds");
        let from_new = NgramEmbedder::new()
            .embed(&[text])
            .await
            .expect("new embeds");
        assert_eq!(from_default, from_new);
    }
}
