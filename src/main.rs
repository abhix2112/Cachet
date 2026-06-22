//! Cachet — a transparent, streaming-aware semantic cache for LLM APIs.
//!
//! Point your LLM client's base URL at this proxy and everything passes through
//! unchanged. On top of that, non-streaming chat completions are served from a
//! 100%-local semantic cache (no embedding API, no vector DB, no network calls
//! for caching) when a semantically equivalent request has been seen before.
//!
//! Module layout grows by phase: `embedder` + `cache` (this phase), with room
//! for a `dashboard` later.

mod cache;
mod config;
mod dashboard;
mod embedder;
mod error;
mod metrics;
mod pricing;
mod proxy;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::cache::SemanticCache;
use crate::config::Config;
use crate::embedder::{Embedder, LocalEmbedder};
use crate::metrics::Metrics;
use crate::pricing::Pricing;
use crate::proxy::AppState;

/// The product name, in one place. Used in logs and error bodies.
pub const PROJECT_NAME: &str = "Cachet";
/// Lowercase slug form (error bodies, header prefixes are derived by hand to stay
/// `&'static`).
pub const PROJECT_SLUG: &str = "cachet";

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config = Config::from_env().context("failed to load configuration")?;

    // One shared client for all upstream calls: pools connections and reuses TLS.
    // A connect timeout fails fast on an unreachable upstream (=> clean 502) while
    // leaving the overall request untimed, so long-lived token streams aren't cut off.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("failed to build upstream HTTP client")?;

    // The local embedder is the default and only embedder this phase. The trait
    // leaves room for an optional `OpenAIEmbedder` later without touching callers.
    let embedder: Arc<dyn Embedder> = Arc::new(LocalEmbedder::new());
    let embed_dim = embedder.dim();
    let cache = Arc::new(SemanticCache::new(embedder, config.cache.clone()));

    let state = AppState {
        config: Arc::new(config.clone()),
        client,
        cache,
        metrics: Arc::new(Metrics::new()),
        pricing: Arc::new(Pricing::from_env()),
    };

    // A single fallback handler forwards every method and path transparently.
    let app = axum::Router::new()
        .fallback(proxy::handle)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("failed to bind to {}", config.listen_addr))?;

    tracing::info!(
        name = PROJECT_NAME,
        listen = %config.listen_addr,
        upstream = %config.upstream_base,
        threshold = config.cache.threshold,
        ttl_secs = config.cache.ttl.as_secs(),
        max_entries = config.cache.max_entries,
        embed_dim,
        dashboard = %format!("http://{}/__cachet/", config.listen_addr),
        "{PROJECT_NAME} started"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    Ok(())
}

/// stdout structured logging. Verbosity via `RUST_LOG` (default `info`).
fn init_tracing() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();
}

/// Resolve on Ctrl-C so in-flight streams can finish on shutdown.
async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        tracing::error!(error = %err, "failed to install Ctrl-C handler");
    }
    tracing::info!("shutdown signal received, draining connections");
}
