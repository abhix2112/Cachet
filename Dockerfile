# syntax=docker/dockerfile:1

# ---- build stage --------------------------------------------------------------
# Full Rust image (Debian bookworm) so the C compiler `ring` needs is present.
FROM rust:1-bookworm AS build
WORKDIR /app

# Copy manifests + sources and build the optimized, stripped binary.
# (The dashboard HTML is include_str!'d at compile time, so it's baked into the
# binary — there are no static assets to ship.)
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

# ---- runtime stage ------------------------------------------------------------
# distroless/cc provides glibc + libgcc (all this binary links) and nothing else —
# no shell, no package manager, no OpenSSL. TLS uses rustls with CA roots compiled
# into the binary (webpki-roots), so no `ca-certificates` package is required.
FROM gcr.io/distroless/cc-debian12 AS runtime
COPY --from=build /app/target/release/cachet /usr/local/bin/cachet

# Bind on all interfaces inside the container so `-p 8080:8080` works. Every other
# knob is overridable at runtime with `-e CACHET_*=...`.
ENV CACHET_HOST=0.0.0.0 \
    CACHET_PORT=8080
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/cachet"]
