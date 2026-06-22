# Cachet

**A transparent, 100%-local semantic cache for LLM APIs.** Drop it in front of any
app with a one-line change, cut the cost of repeated and rephrased calls, and watch
the savings add up live.

![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)
![built with Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)
![deploy](https://img.shields.io/badge/deploy-single%20binary-success)
![cache](https://img.shields.io/badge/cache-100%25%20local-34e0a1)

<!-- DEMO GIF HERE -->
> _Demo: the dashboard at `/__cachet/` — the **$ saved** counter ticking up and green
> "hit" rows streaming in as repeated/rephrased calls are served from cache._

---

## Why

Apps hit LLM APIs with the same and nearly-the-same prompts over and over — retries,
shared questions, re-runs, slightly reworded queries — and pay full price for every
one. The usual answers are either a caching library you wire into your code, or a
heavyweight gateway/managed service you stand up and route through.

Cachet is neither. It's a single Rust binary you put in front of any OpenAI- or
Anthropic-compatible API by changing one line — the base URL. Requests are forwarded
transparently; when a semantically equivalent request has been seen before, the
cached answer is served locally and the upstream call is skipped. The semantic
matching runs entirely on your machine (no embedding API, no vector database), and a
built-in dashboard shows the hit rate and estimated dollars saved in real time.

## Quickstart

```bash
# 1. Build (single binary, no system deps beyond libc — pure-Rust TLS)
cargo build --release

# 2. Run with zero config: listens on 127.0.0.1:8080, forwards to https://api.openai.com
./target/release/cachet
```

Now point your app at it with **one line**:

**OpenAI Python SDK**
```python
from openai import OpenAI

client = OpenAI(
    base_url="http://localhost:8080/v1",   # ← the only change
    api_key="sk-...",
)
# everything else is exactly the same
```

**curl** — just swap the host:
```bash
# before:  https://api.openai.com/v1/chat/completions
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"What is the capital of France?"}]}'
```

Send that twice — the second response comes back with `X-Cachet: hit` and never
touches OpenAI. Then open **<http://localhost:8080/__cachet/>** to watch it work.

Your API key, headers, body, status, and streaming all pass through unchanged. On a
cache miss the request goes to your configured provider exactly as it would without
Cachet; on a hit it's served from local memory.

### Run with Docker

No Rust toolchain needed. The image is a ~47 MB distroless build; TLS is pure-Rust
(rustls with CA roots compiled in), so the binary needs no system OpenSSL.

```bash
docker build -t cachet .

# zero config: forwards to https://api.openai.com, dashboard at /__cachet/
docker run -p 8080:8080 cachet

# with overrides — e.g. point at Anthropic and loosen the match threshold
docker run -p 8080:8080 \
  -e CACHET_UPSTREAM=https://api.anthropic.com \
  -e CACHET_THRESHOLD=0.85 \
  cachet
```

(The image binds `0.0.0.0` inside the container so `-p` works; every other
`CACHET_*` var is overridable with `-e`.)

## How it works

```
     your app                      Cachet  (one local binary, :8080)                  upstream
  ┌────────────┐   one-line   ┌──────────────────────────────────────────┐      ┌───────────────┐
  │ base_url = │  base_url    │  /v1/* request                            │      │  api.openai   │
  │ localhost  │ ───swap────▶ │    │                                      │      │  .com  /  any │
  │   :8080    │              │    ▼   ① exact hash    ② local embedding  │      │  OpenAI-/     │
  └────────────┘              │  cache lookup ──────────────┐            │       │  Anthropic-   │
                              │    │ hit                 miss│ forward    │ ────▶ │  compatible   │
                              │    ▼                         ▼ + tee      │       │  HTTP API     │
                              │  served locally        stream to client  │       └───────────────┘
                              │  ($ saved ++)          & capture; cache   │
                              │                        on clean finish    │
                              └──────────────────────────────────────────┘
                                      dashboard:  GET /__cachet/
```

- **Dual-layer cache.** Every cacheable request is keyed by model + normalized
  prompt. First an **exact** layer (an O(1) hash of the prompt) — free, instant. On a
  miss, a **semantic** layer embeds the prompt and finds the nearest stored entry by
  cosine similarity, returning it only if it clears a configurable threshold.

- **Local lexical embedder.** The default embedder is *not* a neural model. It turns a
  prompt into a vector from word-level and character-trigram features (the hashing
  trick), with stopword removal so content words dominate. It's fast, free, private,
  and dependency-free — and it has a real ceiling: it catches rephrasings that share
  vocabulary or word-shape ("What is the capital of France?" ≈ "What's France's
  capital city?") but **not** zero-overlap paraphrases ("Which city houses the
  Élysée Palace?"). The `Embedder` trait is a single swap-in point for a neural/API
  embedder when you want sharper matching.

- **Streaming tee + replay.** Streaming (SSE) responses are cached too. On a miss the
  response is streamed to your client chunk-by-chunk *and* captured in parallel; it's
  written to the cache only if the stream finishes cleanly — a dropped or errored
  stream is never cached. On a hit, the stored SSE is replayed as a well-framed event
  stream.

- **Live savings dashboard.** A self-contained page at `/__cachet/` (no external/CDN
  requests) streams metrics over SSE: a running **estimated $ saved** counter, hit
  rate, tokens saved, and a color-coded live request feed.

## Configuration

All configuration is environment variables; all are optional.

| Variable             | Default                  | Description                                                        |
|----------------------|--------------------------|--------------------------------------------------------------------|
| `CACHET_UPSTREAM`    | `https://api.openai.com` | Upstream API base URL (host only; the client's `/v1/...` path is forwarded as-is) |
| `CACHET_HOST`        | `127.0.0.1`              | Interface to bind                                                  |
| `CACHET_PORT`        | `8080`                   | Port to listen on                                                  |
| `CACHET_THRESHOLD`   | `0.82`                   | Minimum cosine similarity (0–1) for a semantic hit                 |
| `CACHET_TTL_SECS`    | `3600`                   | Cache entry time-to-live, in seconds                               |
| `CACHET_MAX_ENTRIES` | `10000`                  | Max entries; the oldest is evicted when full                       |
| `CACHET_PRICING`     | _(built-in table)_       | Override per-model prices, e.g. `gpt-4o=2.5/10,my-model=1/2` (USD per 1M in/out) |
| `RUST_LOG`           | `info`                   | Log verbosity                                                      |

## What it is — and isn't

Honesty up front, because it determines whether Cachet fits your use case:

- **The default embedder is lexical, not neural.** It's fast, free, and fully local,
  but it matches on shared words and character shape — great for rephrasings with
  lexical overlap, blind to paraphrases with none. If you need true semantic matching,
  swap in a neural embedder via the `Embedder` trait (see Roadmap).
- **The "$ saved" number is an estimate, not a bill.** Tokens are approximated as
  `chars ÷ 4` and priced against an editable table of approximate public list prices.
  We count only the served answer's text (not JSON/SSE framing), so it errs
  conservative — but it is an estimate. Edit the prices to match your plan.
- **Caching helps deterministic-ish workloads most.** Shared Q&A, docs/support bots,
  evals, and retries benefit a lot. Highly personalized, per-user, or
  high-temperature prompts that are rarely repeated benefit little.
- **The cache is in-memory.** It's bounded by TTL and a max-entry cap, and it's empty
  on restart (no on-disk persistence yet — see Roadmap).
- **On a miss, your prompt goes to your configured provider** — exactly as it would
  without Cachet. "100% local" refers to the caching and embedding: Cachet adds no
  third-party embedding API or vector database, and cache hits are served entirely
  from local memory.
- **Not yet a multi-tenant gateway.** No auth, rate limiting, or per-key quotas — it's
  a drop-in cache, not an API management platform.

## Security

Cachet forwards your API key upstream and caches responses, so a few properties matter:
the `Authorization` header is **never logged, stored, or shown** in the dashboard; the
cache is keyed by `model + prompt + format` and is **shared across all callers of an
instance** (run one per trust boundary); and the dashboard has **no auth** (don't expose
the port publicly). The full trust model — including cross-caller cache sharing,
credential handling, and network exposure — is in **[SECURITY.md](SECURITY.md)**. Please
read it before deploying Cachet anywhere other than in front of your own app.

## Roadmap

Contributions welcome — these are good places to start:

- Optional **neural/API embedder** behind the existing `Embedder` trait, for sharper
  semantic matching (zero-overlap paraphrases).
- **Persistent / on-disk cache** that survives restarts.
- **ANN index** to replace the linear similarity scan for large caches.
- More **providers / response shapes** and richer token accounting.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your
option.

Contributions are welcome — open an issue to discuss substantial changes first. By
contributing you agree your work is dual-licensed under the same terms.
