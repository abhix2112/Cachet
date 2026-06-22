# Contributing to Cachet

Thanks for your interest — contributions are welcome, from typo fixes to new
embedders.

## Build & run

Rust stable (1.80+).

```bash
cargo build --release          # binary at target/release/cachet
./target/release/cachet        # zero-config; see README for CACHET_* env vars
cargo test                     # unit tests
cargo clippy --all-targets     # keep this warning-free
cargo fmt                      # before you push
```

## Before a big change

Open an issue first to discuss substantial changes — a new embedder, on-disk
persistence, protocol or cache-key changes — so we agree on the approach before you
invest the time. Small fixes, tests, and docs improvements can go straight to a PR.

## PR checklist

- `cargo build`, `cargo clippy --all-targets`, and `cargo test` are all clean.
- New behavior has a test where practical.
- No secrets, no personal paths, no committed `target/`.
- Match the surrounding style — comment the non-obvious, not the obvious.

## Code map

- `src/proxy.rs` — request handling, transparent forwarding, the streaming tee + replay.
- `src/cache.rs` — the dual-layer (exact + semantic) cache, TTL, eviction.
- `src/embedder.rs` — the local lexical embedder (hashing trick + character n-grams).
- `src/pricing.rs`, `src/metrics.rs` — cost estimate and the live dashboard feed.
- `src/dashboard.rs`, `src/dashboard.html` — the `/__cachet/` dashboard.
- `src/config.rs`, `src/error.rs`, `src/main.rs` — config, errors, wiring.

## Roadmap — good places to start

- An optional neural/API embedder behind the existing `Embedder` trait (sharper
  semantic matching, including zero-overlap paraphrases).
- A persistent / on-disk cache that survives restarts.
- An ANN index to replace the linear similarity scan at scale.
- More providers and response shapes.

## Security

Please report vulnerabilities privately — see [SECURITY.md](SECURITY.md). Don't open
a public issue for a security problem.

## License

By contributing, you agree that your work is dual-licensed under
[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE), matching the project.
