//! The dual-layer, fully-local semantic cache.
//!
//! Lookup happens in two layers, cheapest first:
//!
//!   1. **Exact layer** — the normalized prompt (per model) is used as a hash-map
//!      key. An identical request is an O(1) hit with zero embedding work.
//!   2. **Semantic layer** — on an exact miss we embed the prompt and linearly
//!      scan stored entries for the highest cosine similarity. We only return it
//!      as a hit if it clears the configured threshold.
//!
//! Entries also carry a TTL and the store is capped; both are enforced lazily on
//! insert (and expired entries are never served).
//!
//! Concurrency: a single `RwLock<Inner>`. Our workload is read-heavy (most lookups
//! don't mutate) and the critical section is a fast in-memory scan, so many lookups
//! proceed concurrently and only misses take the write lock. A single lock also
//! gives the semantic scan a consistent snapshot. If write contention ever shows
//! up we can shard with `dashmap`; not worth the dependency yet.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use axum::http::{HeaderName, HeaderValue};
use bytes::Bytes;

use crate::embedder::{cosine_similarity, Embedder};

/// Tunable cache behavior.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Minimum cosine similarity for a semantic hit. **This floor is the whole
    /// safety story:** set it too low and we serve a stored answer to a question
    /// that only *looks* similar (e.g. capital-of-France vs capital-of-Japan); set
    /// it too high and we never get semantic hits. Tune against real traffic.
    pub threshold: f32,
    /// Entries older than this are ignored and lazily evicted.
    pub ttl: Duration,
    /// Hard cap on stored entries; the oldest is evicted when exceeded.
    pub max_entries: usize,
}

/// Whether a cached response is a single buffered body or a captured SSE stream.
/// Streaming and non-streaming answers to the same prompt are stored as *separate*
/// entries (the format is part of the cache key) and replayed in their own format —
/// we never convert a JSON blob into an SSE stream or vice-versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseFormat {
    NonStreaming,
    Streaming,
}

impl ResponseFormat {
    /// Short tag used in the cache key namespace.
    fn tag(self) -> char {
        match self {
            ResponseFormat::NonStreaming => 'n',
            ResponseFormat::Streaming => 's',
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ResponseFormat::NonStreaming => "non-streaming",
            ResponseFormat::Streaming => "streaming",
        }
    }
}

/// A response captured verbatim so we can replay it faithfully on a hit.
#[derive(Clone)]
pub struct CachedResponse {
    pub status: u16,
    /// End-to-end response headers (content-type, etc.) needed to reconstruct the
    /// reply. Stored as-is; `Content-Length` is intentionally left out so the
    /// server recomputes it from the replayed body.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// For non-streaming: the full body. For streaming: the raw captured SSE bytes
    /// (every `data:` line concatenated exactly as received).
    pub body: Bytes,
    pub format: ResponseFormat,
}

/// Which layer produced a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitLayer {
    Exact,
    Semantic,
}

impl HitLayer {
    pub fn as_str(self) -> &'static str {
        match self {
            HitLayer::Exact => "exact",
            HitLayer::Semantic => "semantic",
        }
    }
}

/// A successful cache lookup.
pub struct CacheHit {
    pub layer: HitLayer,
    pub similarity: f32,
    /// The normalized prompt of the entry that matched (handy in logs to see what
    /// a semantic hit actually matched against).
    pub matched_prompt: String,
    pub response: CachedResponse,
}

struct Entry {
    embedding: Vec<f32>,
    /// Normalized prompt text, kept for collision-safety on exact lookups + debugging.
    norm_prompt: String,
    model: String,
    response: CachedResponse,
    created: Instant,
}

struct Inner {
    /// Keyed by `exact_key(model, prompt)`. Doubles as the semantic-scan corpus.
    by_exact: HashMap<String, Entry>,
}

pub struct SemanticCache {
    embedder: Arc<dyn Embedder>,
    config: CacheConfig,
    inner: RwLock<Inner>,
}

impl SemanticCache {
    pub fn new(embedder: Arc<dyn Embedder>, config: CacheConfig) -> Self {
        Self {
            embedder,
            config,
            inner: RwLock::new(Inner {
                by_exact: HashMap::new(),
            }),
        }
    }

    /// Look up a cached response for `(model, canonical_prompt, format)`. Only
    /// entries of the requested `format` are considered, so a streaming request
    /// never matches a non-streaming entry or vice-versa.
    pub fn lookup(&self, model: &str, canonical: &str, format: ResponseFormat) -> Option<CacheHit> {
        let now = Instant::now();
        let key = exact_key(model, canonical, format);
        let guard = self.inner.read().expect("cache lock poisoned");

        // Layer 1: exact. Free — no embedding required.
        if let Some(entry) = guard.by_exact.get(&key) {
            if !self.is_expired(entry, now) {
                return Some(CacheHit {
                    layer: HitLayer::Exact,
                    similarity: 1.0,
                    matched_prompt: entry.norm_prompt.clone(),
                    response: entry.response.clone(),
                });
            }
        }

        // Layer 2: semantic. Embed the query, scan for the best live match in the
        // same model namespace AND format. Linear scan is fine at this scale
        // (≤ max_entries); swap in an ANN index here if the corpus ever gets large.
        let query = self.embedder.embed(canonical);
        let mut best: Option<(f32, &Entry)> = None;
        for entry in guard.by_exact.values() {
            if entry.model != model
                || entry.response.format != format
                || self.is_expired(entry, now)
            {
                continue;
            }
            let score = cosine_similarity(&query, &entry.embedding);
            if best.map_or(true, |(b, _)| score > b) {
                best = Some((score, entry));
            }
        }

        match best {
            Some((score, entry)) if score >= self.config.threshold => Some(CacheHit {
                layer: HitLayer::Semantic,
                similarity: score,
                matched_prompt: entry.norm_prompt.clone(),
                response: entry.response.clone(),
            }),
            _ => None,
        }
    }

    /// Store a response. Embeds once, sweeps expired entries, and enforces the cap.
    /// The entry's format (from `response.format`) is part of its key, so streaming
    /// and non-streaming answers to the same prompt coexist as distinct entries.
    pub fn insert(&self, model: &str, canonical: &str, response: CachedResponse) {
        let key = exact_key(model, canonical, response.format);
        let embedding = self.embedder.embed(canonical);
        let entry = Entry {
            embedding,
            norm_prompt: normalize_exact(canonical),
            model: model.to_string(),
            response,
            created: Instant::now(),
        };

        let mut guard = self.inner.write().expect("cache lock poisoned");
        let now = Instant::now();
        // Lazy TTL eviction: drop everything stale while we hold the write lock.
        guard
            .by_exact
            .retain(|_, e| now.duration_since(e.created) < self.config.ttl);
        guard.by_exact.insert(key, entry);

        // Enforce capacity by evicting the oldest entries.
        while guard.by_exact.len() > self.config.max_entries {
            let oldest = guard
                .by_exact
                .iter()
                .min_by_key(|(_, e)| e.created)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    guard.by_exact.remove(&k);
                }
                None => break,
            }
        }
    }

    /// Number of entries currently stored (surfaced in miss logs and tests).
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.inner.read().expect("cache lock poisoned").by_exact.len()
    }

    fn is_expired(&self, entry: &Entry, now: Instant) -> bool {
        now.duration_since(entry.created) >= self.config.ttl
    }
}

/// Light normalization for the exact layer: lowercase + collapse all whitespace.
/// (The embedder applies its own heavier normalization for the semantic layer.)
fn normalize_exact(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Cache key = format tag + model namespace + normalized prompt. The model keeps a
/// gpt-4o answer from being served to a gpt-3.5 request; the format tag keeps a
/// streamed answer from being served to a non-streaming request. `\u{1f}` (unit
/// separator) can't appear in a model id or prompt.
fn exact_key(model: &str, canonical: &str, format: ResponseFormat) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        format.tag(),
        model.to_lowercase(),
        normalize_exact(canonical)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::LocalEmbedder;

    use ResponseFormat::{NonStreaming, Streaming};

    fn resp(body: &str) -> CachedResponse {
        resp_fmt(body, NonStreaming)
    }

    fn resp_fmt(body: &str, format: ResponseFormat) -> CachedResponse {
        CachedResponse {
            status: 200,
            headers: vec![(
                HeaderName::from_static("content-type"),
                HeaderValue::from_static("application/json"),
            )],
            body: Bytes::from(body.to_owned()),
            format,
        }
    }

    fn cache_with(threshold: f32) -> SemanticCache {
        SemanticCache::new(
            Arc::new(LocalEmbedder::new()),
            CacheConfig {
                threshold,
                ttl: Duration::from_secs(3600),
                max_entries: 1000,
            },
        )
    }

    #[test]
    fn exact_hit_then_semantic_then_miss() {
        // Self-calibrate: measure the embedder, then pick a threshold that sits
        // between the rephrase and the different-country scores.
        let e = LocalEmbedder::new();
        let france = "user: What is the capital of France?";
        let rephrase = "user: What's France's capital city?";
        let japan = "user: What is the capital of Japan?";
        let ab = cosine_similarity(&e.embed(france), &e.embed(rephrase));
        let ac = cosine_similarity(&e.embed(france), &e.embed(japan));
        assert!(ab > ac);
        let threshold = (ab + ac) / 2.0;

        let cache = cache_with(threshold);
        cache.insert("gpt-4o", france, resp("PARIS"));

        // 1. Exact hit on the identical prompt.
        let hit = cache.lookup("gpt-4o", france, NonStreaming).expect("exact hit");
        assert_eq!(hit.layer, HitLayer::Exact);
        assert_eq!(hit.response.body, Bytes::from_static(b"PARIS"));

        // 2. Semantic hit on a rephrase.
        let hit = cache
            .lookup("gpt-4o", rephrase, NonStreaming)
            .expect("semantic hit");
        assert_eq!(hit.layer, HitLayer::Semantic);
        assert!(hit.similarity >= threshold);

        // 3. Miss on a genuinely different question.
        assert!(cache.lookup("gpt-4o", japan, NonStreaming).is_none());
    }

    #[test]
    fn model_namespacing() {
        let cache = cache_with(0.5);
        cache.insert("gpt-4o", "user: ping", resp("FOUR_O"));
        // Same prompt, different model => must not hit the other model's answer.
        assert!(cache.lookup("gpt-3.5", "user: ping", NonStreaming).is_none());
        assert!(cache.lookup("gpt-4o", "user: ping", NonStreaming).is_some());
    }

    #[test]
    fn format_separation() {
        let cache = cache_with(0.5);
        // Only a non-streaming entry exists for this prompt.
        cache.insert("gpt-4o", "user: ping", resp_fmt("BLOB", NonStreaming));
        // A streaming lookup of the same prompt must MISS (no streaming entry yet).
        assert!(cache.lookup("gpt-4o", "user: ping", Streaming).is_none());
        // The non-streaming lookup hits.
        assert!(cache.lookup("gpt-4o", "user: ping", NonStreaming).is_some());

        // Now add a streaming entry for the same prompt; both coexist.
        cache.insert(
            "gpt-4o",
            "user: ping",
            resp_fmt("data: tok\n\ndata: [DONE]\n\n", Streaming),
        );
        let s = cache.lookup("gpt-4o", "user: ping", Streaming).expect("stream hit");
        assert_eq!(s.response.format, Streaming);
        let n = cache
            .lookup("gpt-4o", "user: ping", NonStreaming)
            .expect("blob hit");
        assert_eq!(n.response.format, NonStreaming);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn ttl_expiry() {
        let cache = SemanticCache::new(
            Arc::new(LocalEmbedder::new()),
            CacheConfig {
                threshold: 0.5,
                ttl: Duration::from_millis(50),
                max_entries: 1000,
            },
        );
        cache.insert("m", "user: hello world", resp("HI"));
        assert!(cache.lookup("m", "user: hello world", NonStreaming).is_some());
        std::thread::sleep(Duration::from_millis(80));
        // Expired entries are not served.
        assert!(cache.lookup("m", "user: hello world", NonStreaming).is_none());
    }

    #[test]
    fn capacity_evicts_oldest() {
        let cache = SemanticCache::new(
            Arc::new(LocalEmbedder::new()),
            CacheConfig {
                threshold: 0.99,
                ttl: Duration::from_secs(3600),
                max_entries: 2,
            },
        );
        cache.insert("m", "user: first apple", resp("1"));
        cache.insert("m", "user: second banana", resp("2"));
        cache.insert("m", "user: third cherry", resp("3"));
        assert_eq!(cache.len(), 2);
        // Oldest ("first") evicted; newest two remain.
        assert!(cache.lookup("m", "user: first apple", NonStreaming).is_none());
        assert!(cache.lookup("m", "user: third cherry", NonStreaming).is_some());
    }
}
