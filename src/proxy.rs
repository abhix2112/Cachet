//! The proxy handler: transparent forwarding + the semantic-cache fast path,
//! now including **streaming capture & replay**.
//!
//! For every request we buffer the request body (small for LLM calls, and the
//! cache needs it anyway), canonicalize it, then decide:
//!
//!   * **Cacheable, non-streaming** → check the cache; a hit is served locally, a
//!     miss is forwarded, the full response buffered, stored if 200, and returned.
//!   * **Cacheable, streaming** → check the cache; a hit is *replayed as SSE*; a
//!     miss is *teed* — streamed to the client chunk-by-chunk while simultaneously
//!     captured, and committed to the cache only on clean completion.
//!   * **Bypass** (anything we can't parse as a chat completion) → forwarded with
//!     streaming intact, never cached.
//!
//! Every response carries an `X-Cachet` header (`hit` / `miss` / `bypass`).

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Instant;

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, Response};
use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use serde_json::Value;

use crate::cache::{CacheHit, CachedResponse, ResponseFormat, SemanticCache};
use crate::config::Config;
use crate::error::ProxyError;
use crate::metrics::{Metrics, Outcome};
use crate::pricing::Pricing;

/// Upper bound on a buffered request body (100 MiB). LLM requests are tiny by
/// comparison; this is purely a guard against a client exhausting our memory.
const MAX_REQUEST_BODY_BYTES: usize = 100 * 1024 * 1024;

/// Responses larger than this are still forwarded to the client but NOT cached, so
/// a single entry can't blow up the heap and an oversized/runaway stream can't
/// accumulate without bound (we drop the in-flight capture once it crosses this).
const MAX_CACHEABLE_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Prompts whose canonical form exceeds this are forwarded but not cached — bounds
/// embedding CPU and cache-key/entry size against a giant-prompt flood.
const MAX_CACHEABLE_PROMPT_BYTES: usize = 128 * 1024;

/// `model` is attacker-controlled; cap its length so it can't bloat cache keys or
/// the dashboard event feed. Real model ids are well under this.
const MAX_MODEL_LEN: usize = 96;

/// Cache-status header names. Lowercased per HTTP/2 convention.
const HDR_CACHE_STATUS: &str = "x-cachet";
const HDR_CACHE_SCORE: &str = "x-cachet-score";
const HDR_CACHE_LAYER: &str = "x-cachet-layer";

/// Hop-by-hop headers (RFC 9110 §7.6.1). These describe a single transport hop
/// and must not be forwarded by a proxy.
const HOP_BY_HOP_HEADERS: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Shared application state handed to every request.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// reqwest's client is internally reference-counted and pools connections,
    /// so cloning it per request is cheap and correct.
    pub client: reqwest::Client,
    /// The local semantic cache (owns the embedder).
    pub cache: Arc<SemanticCache>,
    /// Live metrics + dashboard event fan-out.
    pub metrics: Arc<Metrics>,
    /// Model pricing used to estimate dollars saved per hit.
    pub pricing: Arc<Pricing>,
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP_HEADERS
        .iter()
        .any(|h| name.as_str().eq_ignore_ascii_case(h))
}

/// Handle one request: serve from cache if possible, otherwise forward.
pub async fn handle(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Result<Response<Body>, ProxyError> {
    let start = Instant::now();

    let method = req.method().clone();

    // Path WITHOUT the query string. Used both for routing the reserved namespace
    // and for logging: we deliberately keep the query string out of all logs, since
    // some providers carry the API key as a query parameter and we must never log it.
    let path = req.uri().path().to_owned();

    // Reserved namespace: intercept the dashboard + metrics feed BEFORE any
    // forwarding logic. These are the one carve-out from transparency and must
    // never reach the upstream.
    if path == "/__cachet" || path.starts_with("/__cachet/") {
        return crate::dashboard::handle(&state, &method, &path).await;
    }

    // The full path+query is forwarded verbatim to the upstream (correctness), but
    // never logged (the query may contain a credential — see `path` above).
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    let url = format!("{}{}", state.config.upstream_base, path_and_query);

    let (parts, body) = req.into_parts();
    let body_bytes = to_bytes(body, MAX_REQUEST_BODY_BYTES)
        .await
        .map_err(|e| ProxyError::InvalidRequestBody(e.to_string()))?;

    let fwd_headers = forward_request_headers(&parts.headers);

    match analyze_request(&body_bytes) {
        CachePlan::Cacheable {
            model,
            canonical,
            format,
        } => {
            // ---- Cache fast path ---------------------------------------------
            if let Some(hit) = state.cache.lookup(&model, &canonical, format) {
                // A hit avoided an upstream call: estimate the tokens + dollars saved
                // from the prompt (input) and the *assistant text* of the served
                // response (output) — never the JSON/SSE framing, so we never inflate.
                let output_chars = assistant_text_chars(&hit.response.body, format);
                let (tokens_saved, saved_micros) =
                    state
                        .pricing
                        .saved(&model, canonical.chars().count(), output_chars);
                let similarity = match hit.layer {
                    crate::cache::HitLayer::Semantic => Some(hit.similarity),
                    crate::cache::HitLayer::Exact => None,
                };
                state.metrics.record(
                    &model,
                    Outcome::Hit {
                        layer: hit.layer.as_str(),
                        similarity,
                    },
                    tokens_saved,
                    saved_micros,
                );

                let replay = match format {
                    ResponseFormat::Streaming => "streamed",
                    ResponseFormat::NonStreaming => "buffered",
                };
                tracing::info!(
                    method = %method,
                    path = %path,
                    status = hit.response.status,
                    latency_ms = start.elapsed().as_millis(),
                    cache = "hit",
                    layer = hit.layer.as_str(),
                    similarity = hit.similarity,
                    format = format.as_str(),
                    replay,
                    "served from cache"
                );
                // Prompt text only at debug level (opt-in) — never in default logs.
                tracing::debug!(matched = %hit.matched_prompt, "cache hit matched prompt");
                return match format {
                    ResponseFormat::Streaming => build_streamed_replay(hit),
                    ResponseFormat::NonStreaming => build_cached_response(hit),
                };
            }

            // ---- Miss: forward ----------------------------------------------
            let upstream = dispatch_upstream(
                &state,
                &method,
                &url,
                fwd_headers,
                body_bytes,
                start,
                &path,
            )
            .await?;
            let status = upstream.status();
            let headers = collect_response_headers(upstream.headers());

            match format {
                ResponseFormat::NonStreaming => {
                    // Buffer the whole response, store if 200 and not too big, return it.
                    let body = upstream.bytes().await.map_err(ProxyError::Upstream)?;
                    if status.as_u16() == 200 && body.len() <= MAX_CACHEABLE_RESPONSE_BYTES {
                        state.cache.insert(
                            &model,
                            &canonical,
                            CachedResponse {
                                status: status.as_u16(),
                                headers: cacheable_headers(&headers),
                                body: body.clone(),
                                format: ResponseFormat::NonStreaming,
                            },
                        );
                    }
                    state.metrics.record(&model, Outcome::Miss, 0, 0);
                    tracing::info!(
                        method = %method,
                        path = %path,
                        upstream = %state.config.upstream_base,
                        status = status.as_u16(),
                        latency_ms = start.elapsed().as_millis(),
                        cache = "miss",
                        format = "non-streaming",
                        entries = state.cache.len(),
                        "proxied"
                    );
                    build_replay_response(status.as_u16(), &headers, body, CacheTag::Miss)
                }

                ResponseFormat::Streaming => {
                    // TEE: stream to the client now, capture in parallel, commit
                    // to the cache only on a clean end-of-stream (see StreamingBody).
                    // We only arm capture for cacheable (200) responses.
                    let capture = if status.as_u16() == 200 {
                        Some(Capture {
                            chunks: Vec::new(),
                            total: 0,
                            cache: state.cache.clone(),
                            model: model.clone(),
                            canonical: canonical.clone(),
                            status: status.as_u16(),
                            headers: cacheable_headers(&headers),
                        })
                    } else {
                        None
                    };

                    // The request outcome is a miss now (whether or not the capture
                    // later commits cleanly); record it before handing back the tee.
                    state.metrics.record(&model, Outcome::Miss, 0, 0);

                    let log = RequestLog {
                        method: method.clone(),
                        path: path.clone(),
                        upstream: state.config.upstream_base.clone(),
                        status: status.as_u16(),
                        start,
                        cache_kind: "miss",
                    };

                    let teed = StreamingBody {
                        inner: Box::pin(upstream.bytes_stream()),
                        capture,
                        log: Some(log),
                        errored: false,
                    };

                    let mut builder = Response::builder().status(status);
                    if let Some(out) = builder.headers_mut() {
                        for (name, value) in &headers {
                            out.append(name.clone(), value.clone());
                        }
                        set_cache_header(out, CacheTag::Miss);
                    }
                    builder
                        .body(Body::from_stream(teed))
                        .map_err(|e| ProxyError::Internal(e.to_string()))
                }
            }
        }

        CachePlan::Bypass => {
            // ---- Non-cacheable passthrough (streaming intact, never cached) ---
            let upstream = dispatch_upstream(
                &state,
                &method,
                &url,
                fwd_headers,
                body_bytes,
                start,
                &path,
            )
            .await?;
            let status = upstream.status();
            let resp_headers = upstream.headers().clone();

            state.metrics.record("", Outcome::Bypass, 0, 0);
            tracing::debug!(
                method = %method,
                path = %path,
                upstream = %state.config.upstream_base,
                status = status.as_u16(),
                ttfb_ms = start.elapsed().as_millis(),
                "upstream responded (headers received), streaming body"
            );

            let log = RequestLog {
                method,
                path,
                upstream: state.config.upstream_base.clone(),
                status: status.as_u16(),
                start,
                cache_kind: "bypass",
            };
            let passthrough = StreamingBody {
                inner: Box::pin(upstream.bytes_stream()),
                capture: None,
                log: Some(log),
                errored: false,
            };

            let mut builder = Response::builder().status(status);
            if let Some(out) = builder.headers_mut() {
                copy_response_headers(&resp_headers, out);
                set_cache_header(out, CacheTag::Bypass);
            }
            builder
                .body(Body::from_stream(passthrough))
                .map_err(|e| ProxyError::Internal(e.to_string()))
        }
    }
}

/// What to do with an incoming request.
enum CachePlan {
    /// A chat completion we can cache, with its model namespace, canonicalized
    /// prompt, and response format (streaming vs not).
    Cacheable {
        model: String,
        canonical: String,
        format: ResponseFormat,
    },
    /// Forward without caching (not a parseable chat completion).
    Bypass,
}

/// Inspect the request body and decide whether it's cacheable. Anything we can't
/// confidently parse as a chat completion is bypassed unchanged. A `"stream": true`
/// request is still cacheable — it just maps to the streaming response format.
fn analyze_request(body: &Bytes) -> CachePlan {
    let json: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return CachePlan::Bypass,
    };

    let messages = match json.get("messages").and_then(Value::as_array) {
        Some(m) if !m.is_empty() => m,
        _ => return CachePlan::Bypass,
    };

    let format = if json.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        ResponseFormat::Streaming
    } else {
        ResponseFormat::NonStreaming
    };

    // `model` is attacker-controlled; cap its length (it's part of the cache key and
    // the dashboard feed). `take(MAX_MODEL_LEN)` is over chars, so it's UTF-8 safe.
    let model: String = json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(MAX_MODEL_LEN)
        .collect();

    // Canonicalize the conversation into one string: "role: content" per message.
    let mut canonical = String::new();
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = extract_content(msg.get("content"));
        if !canonical.is_empty() {
            canonical.push('\n');
        }
        canonical.push_str(role);
        canonical.push_str(": ");
        canonical.push_str(&content);
    }

    // Don't cache absurdly large prompts: bypass (forward only) to bound embedding
    // CPU and cache-entry/key size.
    if canonical.len() > MAX_CACHEABLE_PROMPT_BYTES {
        return CachePlan::Bypass;
    }

    CachePlan::Cacheable {
        model,
        canonical,
        format,
    }
}

/// Extract text from a message `content`, which may be a plain string or an array
/// of multimodal parts (we keep the `text` parts).
fn extract_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(text);
                }
            }
            out
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Estimate the assistant-text length (in chars) of a cached response, for cost
/// estimation. We count ONLY the model's output text — not the JSON keys or the
/// `data:`/`[DONE]` SSE framing — so the "$ saved" headline is never inflated.
/// If the payload can't be parsed (an unknown response shape), fall back to the
/// raw body length and note it.
fn assistant_text_chars(body: &Bytes, format: ResponseFormat) -> usize {
    match extract_assistant_text(body, format) {
        Some(text) => text.chars().count(),
        None => {
            tracing::debug!(
                format = format.as_str(),
                "could not parse assistant text; using raw body length for cost estimate"
            );
            body.len()
        }
    }
}

/// Pull the assistant's text out of a cached response body. Returns `None` if the
/// body doesn't look like a (non-)streaming chat completion we recognize.
fn extract_assistant_text(body: &Bytes, format: ResponseFormat) -> Option<String> {
    match format {
        // Non-streaming: choices[].message.content
        ResponseFormat::NonStreaming => {
            let json: Value = serde_json::from_slice(body).ok()?;
            let choices = json.get("choices")?.as_array()?;
            let mut text = String::new();
            for choice in choices {
                if let Some(content) = choice.pointer("/message/content").and_then(Value::as_str) {
                    text.push_str(content);
                }
            }
            Some(text)
        }
        // Streaming SSE: concatenate choices[].delta.content across all data: events.
        ResponseFormat::Streaming => {
            let raw = std::str::from_utf8(body).ok()?;
            let mut text = String::new();
            let mut parsed_any = false;
            for line in raw.lines() {
                let Some(payload) = line.trim_start().strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() || payload == "[DONE]" {
                    continue;
                }
                let Ok(json) = serde_json::from_str::<Value>(payload) else {
                    continue;
                };
                let Some(choices) = json.get("choices").and_then(Value::as_array) else {
                    continue;
                };
                parsed_any = true;
                for choice in choices {
                    if let Some(content) = choice.pointer("/delta/content").and_then(Value::as_str) {
                        text.push_str(content);
                    }
                }
            }
            // Only trust the parse if we recognized at least one SSE chat chunk.
            parsed_any.then_some(text)
        }
    }
}

/// Send the request upstream, logging + converting a connection failure to a 502.
async fn dispatch_upstream(
    state: &AppState,
    method: &Method,
    url: &str,
    headers: HeaderMap,
    body: Bytes,
    start: Instant,
    path: &str,
) -> Result<reqwest::Response, ProxyError> {
    match state
        .client
        .request(method.clone(), url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(resp) => Ok(resp),
        Err(err) => {
            // Log a host + failure classification, NOT the raw reqwest error: its
            // Display embeds the full URL, which can contain a query-string API key.
            tracing::error!(
                method = %method,
                path = %path,
                upstream = %state.config.upstream_base,
                latency_ms = start.elapsed().as_millis(),
                timeout = err.is_timeout(),
                connect = err.is_connect(),
                "upstream request failed"
            );
            Err(ProxyError::Upstream(err))
        }
    }
}

/// Build the header set to send upstream: copy everything end-to-end, but drop
/// hop-by-hop headers, the inbound `Host` (reqwest sets the correct upstream
/// host), and `Content-Length` (reqwest recomputes it from the buffered body).
fn forward_request_headers(incoming: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(incoming.len());
    for (name, value) in incoming.iter() {
        if is_hop_by_hop(name) || name == header::HOST || name == header::CONTENT_LENGTH {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Copy upstream response headers onto a streamed client response, dropping only
/// hop-by-hop headers (used on the bypass path).
fn copy_response_headers(upstream: &HeaderMap, out: &mut HeaderMap) {
    for (name, value) in upstream.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
}

/// Collect the response headers we forward to the *immediate* requester on a miss.
/// Drops hop-by-hop and `Content-Length` — the latter is recomputed from the body
/// when we replay, so it can never drift out of sync with the (re-chunked) body.
fn collect_response_headers(upstream: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    upstream
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name) && *name != header::CONTENT_LENGTH)
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Per-response headers that must never be *stored and replayed to a different
/// requester*. `Set-Cookie` is the important one: a CDN/WAF in front of the upstream
/// (e.g. Cloudflare's `__cf_bm`) sets per-client cookies, and replaying one client's
/// cookie to another from cache would be a cross-requester leak. The immediate
/// requester on a miss still gets the upstream's headers verbatim — this filter only
/// applies to what we persist for later cache hits.
fn is_uncacheable_response_header(name: &HeaderName) -> bool {
    matches!(name.as_str(), "set-cookie" | "set-cookie2")
}

/// The subset of response headers that is safe to store and replay on a cache hit.
fn cacheable_headers(headers: &[(HeaderName, HeaderValue)]) -> Vec<(HeaderName, HeaderValue)> {
    headers
        .iter()
        .filter(|(name, _)| !is_uncacheable_response_header(name))
        .cloned()
        .collect()
}

/// Which `X-Cachet` status to stamp on a response.
#[derive(Clone, Copy)]
enum CacheTag {
    Hit,
    Miss,
    Bypass,
}

impl CacheTag {
    fn as_str(self) -> &'static str {
        match self {
            CacheTag::Hit => "hit",
            CacheTag::Miss => "miss",
            CacheTag::Bypass => "bypass",
        }
    }
}

fn set_cache_header(headers: &mut HeaderMap, tag: CacheTag) {
    headers.insert(
        HeaderName::from_static(HDR_CACHE_STATUS),
        HeaderValue::from_static(tag.as_str()),
    );
}

/// Build a fully-buffered response (cache hit on a non-streaming entry, or a
/// forwarded non-streaming miss).
fn build_replay_response(
    status: u16,
    headers: &[(HeaderName, HeaderValue)],
    body: Bytes,
    tag: CacheTag,
) -> Result<Response<Body>, ProxyError> {
    let mut builder = Response::builder().status(status);
    if let Some(out) = builder.headers_mut() {
        for (name, value) in headers {
            out.append(name.clone(), value.clone());
        }
        set_cache_header(out, tag);
    }
    builder
        .body(Body::from(body))
        .map_err(|e| ProxyError::Internal(e.to_string()))
}

/// Build the response for a non-streaming cache hit, adding score + layer headers.
fn build_cached_response(hit: CacheHit) -> Result<Response<Body>, ProxyError> {
    let response = hit.response;
    let mut builder = Response::builder().status(response.status);
    if let Some(out) = builder.headers_mut() {
        for (name, value) in &response.headers {
            out.append(name.clone(), value.clone());
        }
        set_cache_header(out, CacheTag::Hit);
        add_hit_diagnostics(out, hit.similarity, hit.layer.as_str());
    }
    builder
        .body(Body::from(response.body))
        .map_err(|e| ProxyError::Internal(e.to_string()))
}

/// Build the response for a *streaming* cache hit: replay the captured SSE bytes as
/// a real, well-framed SSE stream (event-by-event) rather than one blob. Replay is
/// near-instant (no artificial delay) but preserves the exact `data:` framing and
/// terminal marker, byte-for-byte.
fn build_streamed_replay(hit: CacheHit) -> Result<Response<Body>, ProxyError> {
    let response = hit.response;
    let events = split_sse_events(&response.body);
    // Zero-copy slices of the stored body; their concatenation equals it exactly.
    let stream = futures_util::stream::iter(
        events
            .into_iter()
            .map(Ok::<Bytes, std::convert::Infallible>),
    );

    let mut builder = Response::builder().status(response.status);
    if let Some(out) = builder.headers_mut() {
        for (name, value) in &response.headers {
            out.append(name.clone(), value.clone());
        }
        set_cache_header(out, CacheTag::Hit);
        add_hit_diagnostics(out, hit.similarity, hit.layer.as_str());
    }
    builder
        .body(Body::from_stream(stream))
        .map_err(|e| ProxyError::Internal(e.to_string()))
}

fn add_hit_diagnostics(out: &mut HeaderMap, similarity: f32, layer: &'static str) {
    if let Ok(score) = HeaderValue::from_str(&format!("{similarity:.4}")) {
        out.insert(HeaderName::from_static(HDR_CACHE_SCORE), score);
    }
    out.insert(
        HeaderName::from_static(HDR_CACHE_LAYER),
        HeaderValue::from_static(layer),
    );
}

/// Split raw SSE bytes into per-event slices at `\n\n` boundaries. Slices are
/// zero-copy views into the same buffer, and concatenating them reproduces the
/// input exactly (so replay is byte-identical to the originally streamed response).
fn split_sse_events(data: &Bytes) -> Vec<Bytes> {
    let mut events = Vec::new();
    let bytes = data.as_ref();
    let mut start = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\n' && bytes[i + 1] == b'\n' {
            let end = i + 2;
            events.push(data.slice(start..end));
            start = end;
            i = end;
        } else {
            i += 1;
        }
    }
    if start < bytes.len() {
        events.push(data.slice(start..));
    }
    events
}

/// Whether the captured SSE contains an OpenAI-style terminal sentinel. Used only
/// as a *logged confirmation* signal — see `StreamingBody`'s completion rule.
fn has_sse_terminator(body: &[u8]) -> bool {
    body.windows(b"[DONE]".len()).any(|w| w == b"[DONE]")
}

/// State carried by a streaming miss so its capture can be committed to the cache
/// once (and only once) the stream finishes cleanly.
struct Capture {
    /// Captured chunks, kept as cheap ref-counted `Bytes` clones (no copy on the
    /// hot path); concatenated into one buffer only at commit time.
    chunks: Vec<Bytes>,
    total: usize,
    cache: Arc<SemanticCache>,
    model: String,
    canonical: String,
    status: u16,
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl Capture {
    fn commit(self) {
        let mut buf = BytesMut::with_capacity(self.total);
        for chunk in &self.chunks {
            buf.extend_from_slice(chunk);
        }
        let body = buf.freeze();
        let terminator = has_sse_terminator(&body);

        self.cache.insert(
            &self.model,
            &self.canonical,
            CachedResponse {
                status: self.status,
                headers: self.headers,
                body,
                format: ResponseFormat::Streaming,
            },
        );

        tracing::info!(
            model = %self.model,
            bytes = self.total,
            terminator,
            entries = self.cache.len(),
            "captured streamed response committed to cache"
        );
    }
}

/// Captured fields for the single per-request completion log line on streamed paths.
struct RequestLog {
    method: Method,
    path: String,
    upstream: String,
    status: u16,
    start: Instant,
    cache_kind: &'static str,
}

impl RequestLog {
    fn emit(&self, note: &'static str) {
        tracing::info!(
            method = %self.method,
            path = %self.path,
            upstream = %self.upstream,
            status = self.status,
            latency_ms = self.start.elapsed().as_millis(),
            cache = self.cache_kind,
            note,
            "proxied"
        );
    }
}

/// The streaming response body wrapper. It does three jobs at once:
///
///   1. **Pass through** every upstream chunk to the client *immediately* — the
///      client sees tokens at the same pace as if Cachet weren't here.
///   2. **Tee** each chunk into `capture` (when armed) as a cheap `Bytes` clone,
///      adding no copy and no backpressure on the client's critical path.
///   3. **Commit-or-discard**: on a clean end-of-stream it commits the capture to
///      the cache; on any transport error or an early client disconnect it discards
///      the capture so a partial answer is never cached.
///
/// **Clean-completion rule:** commit iff the upstream byte stream yields `None`
/// (graceful end of the HTTP body) with *no* prior `Err`. A `Some(Err(_))` poisons
/// the capture; being dropped before reaching `None` (client went away) also
/// prevents commit. The OpenAI `[DONE]` sentinel is recorded as a confirmation
/// signal in the commit log but is not itself required, since other providers
/// terminate their streams differently.
struct StreamingBody {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    capture: Option<Capture>,
    log: Option<RequestLog>,
    errored: bool,
}

impl Stream for StreamingBody {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        // `Pin<Box<_>>` is `Unpin`, so `get_mut` is safe and macro-free.
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let mut over_limit = false;
                if let Some(capture) = this.capture.as_mut() {
                    capture.total += chunk.len();
                    capture.chunks.push(chunk.clone()); // ref-count bump, no data copy
                    over_limit = capture.total > MAX_CACHEABLE_RESPONSE_BYTES;
                }
                if over_limit {
                    // Too big to cache: drop the capture (frees what we accumulated)
                    // and keep forwarding to the client uninterrupted.
                    this.capture = None;
                    tracing::debug!("streamed response exceeded cache size limit; not caching");
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => {
                // Transport error mid-stream => poison. Never commit; drop capture.
                this.errored = true;
                this.capture = None;
                if let Some(log) = this.log.take() {
                    log.emit("stream_error");
                }
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                // Graceful end-of-body: commit iff we captured and saw no error.
                if !this.errored {
                    if let Some(capture) = this.capture.take() {
                        capture.commit();
                    }
                }
                if let Some(log) = this.log.take() {
                    log.emit("completed");
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for StreamingBody {
    fn drop(&mut self) {
        // If `log` is still set we never reached a terminal poll (None/Err): the
        // client disconnected mid-stream. Discard the capture — committing a
        // partial response would poison the cache with a truncated answer.
        if let Some(log) = self.log.take() {
            log.emit("client_disconnected");
        }
        // self.capture is dropped here without committing.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_streaming_assistant_text_only() {
        // Raw SSE (with framing) is much longer than the actual answer text.
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n\
                    data: [DONE]\n\n";
        let body = Bytes::from_static(sse);
        let text = extract_assistant_text(&body, ResponseFormat::Streaming).unwrap();
        assert_eq!(text, "Hello world");
        // The text-only char count is far below the raw framed byte count.
        assert!(assistant_text_chars(&body, ResponseFormat::Streaming) < body.len());
        assert_eq!(assistant_text_chars(&body, ResponseFormat::Streaming), 11);
    }

    #[test]
    fn extracts_non_streaming_assistant_text_only() {
        let json = br#"{"id":"x","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"Paris"}}]}"#;
        let body = Bytes::from_static(json);
        assert_eq!(
            extract_assistant_text(&body, ResponseFormat::NonStreaming).unwrap(),
            "Paris"
        );
        assert_eq!(assistant_text_chars(&body, ResponseFormat::NonStreaming), 5);
    }

    #[test]
    fn unknown_payload_falls_back_to_body_len() {
        let body = Bytes::from_static(b"not json at all");
        assert!(extract_assistant_text(&body, ResponseFormat::NonStreaming).is_none());
        assert_eq!(
            assistant_text_chars(&body, ResponseFormat::NonStreaming),
            body.len()
        );
    }

    fn cacheable(body: &[u8]) -> Option<(String, String)> {
        match analyze_request(&Bytes::copy_from_slice(body)) {
            CachePlan::Cacheable {
                model, canonical, ..
            } => Some((model, canonical)),
            CachePlan::Bypass => None,
        }
    }

    #[test]
    fn malformed_input_never_panics_and_bypasses() {
        // None of these should panic; all should bypass (not cacheable).
        let inputs: &[&[u8]] = &[
            b"",
            b"not json",
            b"{",
            b"[]",
            b"null",
            b"123",
            b"\xff\xfe\x00 not utf8 ish",
            br#"{"messages":[]}"#,                 // empty messages
            br#"{"messages":"notarray"}"#,         // wrong type
            br#"{"model":123,"messages":[{"role":"user","content":"hi"}]}"#, // non-string model -> ""
            br#"{"messages":[{"role":5,"content":{"x":1}}]}"#, // weird role/content
            br#"{"model":null,"messages":[{"content":null}]}"#,
        ];
        for inp in inputs {
            // Just must not panic; result type is irrelevant here.
            let _ = analyze_request(&Bytes::copy_from_slice(inp));
        }
        // A non-string model is tolerated and becomes the empty namespace.
        let (model, _) =
            cacheable(br#"{"model":123,"messages":[{"role":"user","content":"hi"}]}"#).unwrap();
        assert_eq!(model, "");
    }

    #[test]
    fn model_is_length_capped() {
        let huge = "x".repeat(10_000);
        let body = format!(
            r#"{{"model":"{huge}","messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        let (model, _) = cacheable(body.as_bytes()).unwrap();
        assert_eq!(model.chars().count(), MAX_MODEL_LEN);
    }

    #[test]
    fn oversized_prompt_bypasses_cache() {
        let huge = "word ".repeat(MAX_CACHEABLE_PROMPT_BYTES / 5 + 100);
        let body = format!(
            r#"{{"model":"gpt-4o","messages":[{{"role":"user","content":"{huge}"}}]}}"#
        );
        assert!(cacheable(body.as_bytes()).is_none(), "huge prompt must bypass");
    }

    #[test]
    fn set_cookie_is_not_cached() {
        let headers = vec![
            (
                HeaderName::from_static("content-type"),
                HeaderValue::from_static("application/json"),
            ),
            (
                HeaderName::from_static("set-cookie"),
                HeaderValue::from_static("session=secret; HttpOnly"),
            ),
        ];
        let kept = cacheable_headers(&headers);
        assert!(kept.iter().all(|(n, _)| n.as_str() != "set-cookie"));
        assert!(kept.iter().any(|(n, _)| n.as_str() == "content-type"));
    }
}
