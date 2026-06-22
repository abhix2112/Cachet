# Security

Cachet sits in your request path, forwards your API credentials upstream, and caches
responses. This document states what it does and does not protect, so you can deploy
it safely.

## Trust model — run one Cachet per trust boundary

**Cachet's cache is shared across all callers of a given Cachet instance.** The cache
key is `model + normalized-prompt + format` — it does **not** include the API key or
any caller identity. Two requests with the same model and prompt get the same cached
answer, regardless of which API key sent them.

This is correct and intended for the primary use case: **one app (or one team) putting
Cachet in front of its own LLM usage.** It is what makes the cache useful.

It also means a single Cachet instance is **not** a multi-tenant boundary:

- **Cross-caller sharing.** If users A and B send the identical prompt, B may be served
  the response generated for A. Fine when A and B are the same trust domain; not fine if
  they aren't.
- **Cache probing (timing/oracle).** Anyone who can send requests can learn whether a
  specific prompt was previously sent through this instance (a hit is fast and carries
  `X-Cachet: hit`). Don't expose a shared instance to untrusted users if the mere fact
  that a prompt was asked is sensitive.
- **Hits are not re-authorized.** A cache hit is served from local memory **without
  contacting the upstream**, so the caller's API key is not validated upstream on a hit.

**Recommendation:** run a separate Cachet instance per trust boundary (per app, per
tenant, per user-group). Do not place a single shared instance in front of mutually
distrusting callers. If you need per-key cache isolation in one instance, that's a
reasonable feature request (a key-scoped cache mode) — it isn't built yet.

## Network exposure

- Cachet binds `127.0.0.1` by default (loopback only). The Docker image binds `0.0.0.0`
  so published ports work — when you run the container, **only publish the port to a
  trusted network.**
- **The dashboard (`/__cachet/`) and its event feed have no authentication.** They
  expose aggregate metrics and **model names** (which come from requests). They do
  **not** expose prompt text, response bodies, API keys, or headers. Still, treat the
  listening port as privileged: don't expose Cachet to the public internet without your
  own authentication/network controls in front of it. A publicly reachable instance is
  effectively an open proxy to your upstream and a cache-probing oracle.

## Credentials

- The inbound `Authorization` header (and all other request headers except hop-by-hop /
  `Host` / `Content-Length`) is forwarded to the upstream unchanged.
- The `Authorization` header is **never** logged, never stored in the cache, and never
  placed in the dashboard event feed. The cache stores upstream **response** headers
  only (and strips `Set-Cookie`), never request headers.
- Logs at the default level never include the request query string, the full upstream
  URL, or raw upstream-error strings — because some providers carry the API key as a
  query parameter and an error's URL would leak it. Only the path (without query) and
  the configured upstream host are logged.
- Prompt text is logged only at `debug` level (opt-in via `RUST_LOG=debug`), never by
  default.

## What the savings number is

The dashboard's "$ saved" is an **estimate** (`chars ÷ 4` tokens × approximate list
prices from an editable table), not a bill. See the README.

## Caveats worth knowing

- **Semantic false positives.** The semantic layer serves a cached answer when a prompt
  is similar enough (cosine ≥ `CACHET_THRESHOLD`, default `0.82`). A wrong-but-similar
  prompt can in principle get a wrong cached answer; the threshold is the control. Raise
  it if your prompts are easily confusable.
- **TLS.** Upstream connections use rustls with verification on. There is no option to
  disable certificate verification.
- **Resource bounds.** Request bodies are capped at 100 MiB; responses over 8 MiB are
  forwarded but not cached; prompts over 128 KiB are forwarded but not cached; the cache
  is bounded by `CACHET_MAX_ENTRIES` and `CACHET_TTL_SECS`.

## Reporting a vulnerability

Please report security issues privately via the repository's security advisory feature
(or the contact in the repository metadata) rather than opening a public issue. We aim
to acknowledge reports promptly.
