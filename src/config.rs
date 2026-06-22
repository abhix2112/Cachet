//! Runtime configuration, loaded from the environment.
//!
//! All knobs are environment variables so the proxy stays 12-factor friendly and
//! container-ready. The canonical prefix is `CACHET_*`; the old `LLMCACHE_*` names
//! still work as fallback aliases so existing setups don't break on the rename.

use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::cache::CacheConfig;

/// Default upstream when no upstream var is set.
const DEFAULT_UPSTREAM: &str = "https://api.openai.com";
/// Default bind host. Loopback by default so the proxy isn't world-exposed unless asked.
const DEFAULT_HOST: &str = "127.0.0.1";
/// Default listen port.
const DEFAULT_PORT: u16 = 8080;
/// Default semantic-hit similarity floor. See [`CacheConfig::threshold`].
/// Tuned down from 0.92: with the lexical embedder a genuine rephrase scores
/// ~0.85–0.88 while a different question scores ~0.55, so 0.82 captures rephrases
/// with comfortable margin above the noise floor.
const DEFAULT_THRESHOLD: f32 = 0.82;
/// Default entry time-to-live.
const DEFAULT_TTL_SECS: u64 = 3600;
/// Default maximum number of cached entries.
const DEFAULT_MAX_ENTRIES: usize = 10_000;

#[derive(Debug, Clone)]
pub struct Config {
    /// Address the proxy binds to (host + port).
    pub listen_addr: SocketAddr,
    /// Upstream base URL, e.g. `https://api.openai.com` (no trailing slash).
    pub upstream_base: String,
    /// Semantic cache tuning.
    pub cache: CacheConfig,
}

impl Config {
    /// Build configuration from environment variables.
    ///
    /// | Variable             | Legacy alias        | Default                  |
    /// |----------------------|---------------------|--------------------------|
    /// | `CACHET_UPSTREAM`    | `LLMCACHE_UPSTREAM` | `https://api.openai.com` |
    /// | `CACHET_HOST`        | `LLMCACHE_HOST`     | `127.0.0.1`              |
    /// | `CACHET_PORT`        | `LLMCACHE_PORT`     | `8080`                   |
    /// | `CACHET_THRESHOLD`   | —                   | `0.82`                   |
    /// | `CACHET_TTL_SECS`    | —                   | `3600`                   |
    /// | `CACHET_MAX_ENTRIES` | —                   | `10000`                  |
    pub fn from_env() -> Result<Self> {
        let host = env_with_fallback("CACHET_HOST", "LLMCACHE_HOST")
            .unwrap_or_else(|| DEFAULT_HOST.to_string());

        let port = match env_with_fallback("CACHET_PORT", "LLMCACHE_PORT") {
            Some(raw) => raw
                .parse::<u16>()
                .with_context(|| format!("port must be a valid number, got {raw:?}"))?,
            None => DEFAULT_PORT,
        };

        let listen_addr = format!("{host}:{port}")
            .parse::<SocketAddr>()
            .with_context(|| format!("invalid bind address {host}:{port}"))?;

        let upstream_base = env_with_fallback("CACHET_UPSTREAM", "LLMCACHE_UPSTREAM")
            .unwrap_or_else(|| DEFAULT_UPSTREAM.to_string())
            .trim_end_matches('/')
            .to_string();

        if !(upstream_base.starts_with("http://") || upstream_base.starts_with("https://")) {
            anyhow::bail!("upstream must start with http:// or https://, got {upstream_base:?}");
        }

        let threshold = match env::var("CACHET_THRESHOLD") {
            Ok(raw) => raw
                .parse::<f32>()
                .with_context(|| format!("CACHET_THRESHOLD must be a float, got {raw:?}"))?,
            Err(_) => DEFAULT_THRESHOLD,
        };
        if !(0.0..=1.0).contains(&threshold) {
            anyhow::bail!("CACHET_THRESHOLD must be between 0.0 and 1.0, got {threshold}");
        }

        let ttl_secs = match env::var("CACHET_TTL_SECS") {
            Ok(raw) => raw
                .parse::<u64>()
                .with_context(|| format!("CACHET_TTL_SECS must be an integer, got {raw:?}"))?,
            Err(_) => DEFAULT_TTL_SECS,
        };

        let max_entries = match env::var("CACHET_MAX_ENTRIES") {
            Ok(raw) => raw
                .parse::<usize>()
                .with_context(|| format!("CACHET_MAX_ENTRIES must be an integer, got {raw:?}"))?,
            Err(_) => DEFAULT_MAX_ENTRIES,
        };

        Ok(Config {
            listen_addr,
            upstream_base,
            cache: CacheConfig {
                threshold,
                ttl: Duration::from_secs(ttl_secs),
                max_entries,
            },
        })
    }
}

/// Read `primary`, falling back to the legacy `LLMCACHE_*` alias if unset.
fn env_with_fallback(primary: &str, legacy: &str) -> Option<String> {
    env::var(primary).ok().or_else(|| env::var(legacy).ok())
}
