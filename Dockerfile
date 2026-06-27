# ─────────────────────────────────────────────────────────────────────────────
#  Stage 1: Dependency cache layer
#  Trick: build a dummy main.rs first so Cargo caches all deps separately.
#  Re-running after only src/ changes will skip this heavy layer entirely.
# ─────────────────────────────────────────────────────────────────────────────
FROM rust:1.79-slim-bookworm AS deps

WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
# Stub binary to force dep compilation without real source
RUN mkdir src && echo 'fn main(){}' > src/main.rs
RUN cargo build --release
RUN rm -rf src

# ─────────────────────────────────────────────────────────────────────────────
#  Stage 2: Real compile
# ─────────────────────────────────────────────────────────────────────────────
FROM deps AS builder

COPY src ./src
# Touch main.rs so Cargo knows it changed vs the stub
RUN touch src/main.rs
RUN cargo build --release

# ─────────────────────────────────────────────────────────────────────────────
#  Stage 3: Minimal runtime — distroless (no shell, no apt, ~8MB total image)
#  Using cc variant for glibc (required by reqwest's TLS stack)
# ─────────────────────────────────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12 AS runtime

WORKDIR /app

# Copy the compiled binary
COPY --from=builder /build/target/release/flipkart-price-bot .

# Copy CA certs (needed for HTTPS — distroless has them but explicit is safer)
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

# Run as non-root (UID 65532 = "nonroot" in distroless)
USER nonroot:nonroot

# No EXPOSE — this is a worker, not a server
ENTRYPOINT ["/app/flipkart-price-bot"]
