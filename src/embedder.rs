//! Local, dependency-free semantic embedder — Cachet's whole wedge.
//!
//! There is deliberately **no ML model, no transformer, no API call** here. We
//! turn text into a fixed-size vector using two classic lexical signals combined
//! via the hashing trick:
//!
//!   1. **Word features** — each whitespace token is hashed into a bucket. This
//!      captures "do these prompts talk about the same things?"
//!   2. **Character 3-gram features** — each token is also broken into wrapped
//!      character trigrams (`#france#` -> `#fr`, `fra`, `ran`, ...). This makes
//!      "summarise"/"summarize", "france"/"france's", and minor typos land near
//!      each other even though the whole word never matches exactly.
//!
//! Both feature kinds are hashed into the *same* `EMBED_DIM`-sized space (with a
//! one-byte namespace tag so a word and a trigram spelled the same don't collide),
//! then the vector is L2-normalized so cosine similarity is just a dot product.
//!
//! We also drop English stopwords and chat-role labels before embedding. This is
//! essential, not cosmetic: without it, "capital of France" and "capital of Japan"
//! share five of six words and score as near-identical, while a genuine rephrase
//! ("France's capital city") shares fewer surface words and scores lower — exactly
//! backwards. Removing the function-word noise lets the *content* words dominate.

/// Embedding dimensionality. 512 is plenty of room for the hashing trick to keep
/// feature collisions rare at our text sizes, while staying tiny and cache-friendly.
pub const EMBED_DIM: usize = 512;

/// Relative weight of whole-word vs character-trigram features. Equal weighting
/// tested well (see `tests`); exposed as consts so they're trivial to tune.
const WORD_WEIGHT: f32 = 1.0;
const CHAR_WEIGHT: f32 = 1.0;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Common English function words + chat-role labels. These carry little meaning
/// but appear everywhere, so they inflate similarity between unrelated prompts.
const STOPWORDS: &[&str] = &[
    // articles / conjunctions / prepositions
    "a", "an", "the", "of", "to", "in", "on", "at", "for", "and", "or", "but", "if",
    "by", "with", "from", "into", "about", "as", "than", "then", "so",
    // be / do / have / aux
    "is", "are", "was", "were", "be", "been", "being", "am", "do", "does", "did",
    "have", "has", "had", "can", "could", "will", "would", "should", "may", "might",
    // wh-words and common pronouns / fillers
    "what", "whats", "which", "who", "whom", "whose", "when", "where", "why", "how",
    "this", "that", "these", "those", "it", "its", "i", "you", "he", "she", "we",
    "they", "me", "my", "your", "our", "their", "his", "her", "s", "t", "please",
    // chat-role labels, so "user:"/"assistant:" prefixes don't dilute the vector
    "user", "assistant", "system", "tool", "developer",
];

/// A local semantic embedder.
pub trait Embedder: Send + Sync {
    /// Embed `text` into a vector. Implementations should return an L2-normalized
    /// vector of length [`Embedder::dim`].
    fn embed(&self, text: &str) -> Vec<f32>;
    /// The dimensionality of vectors produced by [`Embedder::embed`].
    fn dim(&self) -> usize;
}

/// The default, fully-local lexical embedder. (A future `OpenAIEmbedder` can
/// implement the same trait without touching the cache or proxy.)
pub struct LocalEmbedder {
    dim: usize,
}

impl LocalEmbedder {
    pub fn new() -> Self {
        Self { dim: EMBED_DIM }
    }

    /// Hash one feature into the vector using signed feature hashing. `ns` is a
    /// namespace byte (`w` for words, `c` for char-grams) so identically-spelled
    /// features of different kinds don't collide.
    fn add_feature(&self, vec: &mut [f32], ns: u8, bytes: &[u8], weight: f32) {
        let mut h = FNV_OFFSET;
        h ^= ns as u64;
        h = h.wrapping_mul(FNV_PRIME);
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
        let idx = ((h >> 1) as usize) % self.dim;
        // Sign hashing (Weinberger et al.) keeps collisions from systematically
        // biasing the vector: colliding features cancel as often as they reinforce.
        let sign = if h & 1 == 0 { 1.0 } else { -1.0 };
        vec[idx] += weight * sign;
    }
}

impl Default for LocalEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl Embedder for LocalEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut vec = vec![0.0_f32; self.dim];
        let tokens = normalize_tokens(text);

        for tok in &tokens {
            // (a) whole-word feature
            self.add_feature(&mut vec, b'w', tok.as_bytes(), WORD_WEIGHT);

            // (b) wrapped character 3-gram features
            let wrapped: Vec<char> = std::iter::once('#')
                .chain(tok.chars())
                .chain(std::iter::once('#'))
                .collect();
            for window in wrapped.windows(3) {
                let trigram: String = window.iter().collect();
                self.add_feature(&mut vec, b'c', trigram.as_bytes(), CHAR_WEIGHT);
            }
        }

        l2_normalize(&mut vec);
        vec
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

/// Lowercase, split on non-alphanumerics, drop 1-char tokens (handles the dangling
/// "s"/"t" left by splitting "what's" -> "what","s"), and remove stopwords. Falls
/// back to the unfiltered tokens if a prompt is *all* stopwords, so we never embed
/// an all-zero vector.
fn normalize_tokens(text: &str) -> Vec<String> {
    let mut raw = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                cur.push(lc);
            }
        } else if !cur.is_empty() {
            raw.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        raw.push(cur);
    }
    raw.retain(|t| t.chars().count() >= 2);

    let content: Vec<String> = raw.iter().filter(|t| !is_stopword(t)).cloned().collect();
    if content.is_empty() {
        raw
    } else {
        content
    }
}

fn is_stopword(token: &str) -> bool {
    STOPWORDS.contains(&token)
}

fn l2_normalize(vec: &mut [f32]) {
    let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in vec.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine similarity of two vectors. For L2-normalized inputs this equals the dot
/// product; we compute it fully so it's correct for any input.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for i in 0..len {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sim(e: &LocalEmbedder, a: &str, b: &str) -> f32 {
        cosine_similarity(&e.embed(a), &e.embed(b))
    }

    #[test]
    fn france_triple_scores() {
        let e = LocalEmbedder::new();
        let a = "What is the capital of France?";
        let b = "What's France's capital city?";
        let c = "What is the capital of Japan?";

        let ab = sim(&e, a, b);
        let ac = sim(&e, a, c);
        let aa = sim(&e, a, a);

        println!("identical   A~A = {aa:.4}");
        println!("rephrase    A~B = {ab:.4}  (should be a semantic HIT)");
        println!("diff country A~C = {ac:.4}  (should be a MISS)");

        // Identical text is exactly 1.0.
        assert!((aa - 1.0).abs() < 1e-5);
        // The crucial invariant: a true rephrase must be MORE similar than a
        // different country, so a single threshold can separate them.
        assert!(ab > ac, "rephrase ({ab:.4}) must beat different-country ({ac:.4})");
    }

    #[test]
    fn typo_and_spelling_robustness() {
        let e = LocalEmbedder::new();
        // British/American spelling plus a typo: thanks to char-trigrams this stays
        // far closer than an unrelated sentence, which is the property the cache needs.
        let variant = sim(
            &e,
            "Please summarise this document",
            "Please summarize this docment",
        );
        let unrelated = sim(
            &e,
            "Please summarise this document",
            "How do I bake sourdough bread",
        );
        println!("spelling-variant = {variant:.4}, unrelated = {unrelated:.4}");
        assert!(variant > unrelated, "spelling variant must beat unrelated text");
        assert!(variant > 0.5);
    }

    #[test]
    fn dim_is_const() {
        assert_eq!(LocalEmbedder::new().dim(), EMBED_DIM);
    }
}
