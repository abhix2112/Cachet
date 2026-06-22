//! Shared, low-overhead request metrics + the live event fan-out.
//!
//! Counters are plain atomics (no lock on the hot path); only the small bounded
//! ring buffer of recent events takes a short mutex. Each recorded request is also
//! published on a `tokio::sync::broadcast` channel, which the `/__cachet/events`
//! SSE endpoint forwards to connected dashboards. Recording never blocks or awaits,
//! so it adds negligible overhead to the proxy path.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::broadcast;

/// How many recent events a freshly-opened dashboard sees.
const RECENT_CAPACITY: usize = 60;
/// Broadcast backlog before a slow subscriber starts dropping (it just misses rows).
const BROADCAST_CAPACITY: usize = 512;

/// The outcome of a single request, as seen by the cache.
#[derive(Clone, Copy)]
pub enum Outcome {
    Hit {
        layer: &'static str,
        /// Present for semantic hits (the similarity score); `None` for exact hits.
        similarity: Option<f32>,
    },
    Miss,
    Bypass,
}

impl Outcome {
    fn status(&self) -> &'static str {
        match self {
            Outcome::Hit { .. } => "hit",
            Outcome::Miss => "miss",
            Outcome::Bypass => "bypass",
        }
    }
}

pub struct Metrics {
    total: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    bypasses: AtomicU64,
    tokens_saved: AtomicU64,
    saved_micros: AtomicU64,
    seq: AtomicU64,
    recent: Mutex<VecDeque<Value>>,
    tx: broadcast::Sender<String>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            total: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            bypasses: AtomicU64::new(0),
            tokens_saved: AtomicU64::new(0),
            saved_micros: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            recent: Mutex::new(VecDeque::with_capacity(RECENT_CAPACITY)),
            tx,
        }
    }

    /// Subscribe to the live event stream (one receiver per dashboard connection).
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }

    /// Record one request. `tokens_saved`/`saved_micros` are non-zero only for hits.
    pub fn record(&self, model: &str, outcome: Outcome, tokens_saved: u64, saved_micros: u64) {
        self.total.fetch_add(1, Ordering::Relaxed);
        match outcome {
            Outcome::Hit { .. } => self.hits.fetch_add(1, Ordering::Relaxed),
            Outcome::Miss => self.misses.fetch_add(1, Ordering::Relaxed),
            Outcome::Bypass => self.bypasses.fetch_add(1, Ordering::Relaxed),
        };
        if tokens_saved > 0 {
            self.tokens_saved.fetch_add(tokens_saved, Ordering::Relaxed);
        }
        if saved_micros > 0 {
            self.saved_micros.fetch_add(saved_micros, Ordering::Relaxed);
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);

        let (layer, similarity) = match outcome {
            Outcome::Hit { layer, similarity } => (Some(layer), similarity),
            _ => (None, None),
        };
        let event = json!({
            "seq": seq,
            "ts": now_ms(),
            "model": model,
            "status": outcome.status(),
            "layer": layer,
            "sim": similarity,
            "tokensSaved": tokens_saved,
            "savedMicros": saved_micros,
        });

        {
            let mut recent = self.recent.lock().expect("metrics lock poisoned");
            recent.push_front(event.clone());
            while recent.len() > RECENT_CAPACITY {
                recent.pop_back();
            }
        }

        // Best-effort fan-out; `Err` just means no dashboards are connected.
        let message = json!({ "type": "event", "stats": self.stats_value(), "event": event });
        let _ = self.tx.send(message.to_string());
    }

    /// Current aggregate stats as JSON (also embedded in every live event).
    pub fn stats_value(&self) -> Value {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let cacheable = hits + misses;
        // Hit rate is over *cacheable* traffic (hits + misses); bypasses aren't
        // candidates for caching, so counting them would unfairly depress the rate.
        let hit_rate = if cacheable > 0 {
            hits as f64 / cacheable as f64
        } else {
            0.0
        };
        json!({
            "requests": self.total.load(Ordering::Relaxed),
            "hits": hits,
            "misses": misses,
            "bypasses": self.bypasses.load(Ordering::Relaxed),
            "hitRate": hit_rate,
            "tokensSaved": self.tokens_saved.load(Ordering::Relaxed),
            "savedMicros": self.saved_micros.load(Ordering::Relaxed),
        })
    }

    /// The initial message a dashboard receives on connect: current stats + history.
    pub fn snapshot_message(&self) -> String {
        let events: Vec<Value> = self
            .recent
            .lock()
            .expect("metrics lock poisoned")
            .iter()
            .cloned()
            .collect();
        json!({ "type": "snapshot", "stats": self.stats_value(), "events": events }).to_string()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
