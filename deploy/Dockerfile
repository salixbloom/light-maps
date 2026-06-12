# ── stage 1: build ────────────────────────────────────────────────────────────
FROM rust:1.80-slim-bookworm AS builder

WORKDIR /build

# Cache dependency compilation separately from application code.
COPY Cargo.toml Cargo.lock ./
COPY crates/lm-core/Cargo.toml  crates/lm-core/Cargo.toml
COPY crates/lm-bake/Cargo.toml  crates/lm-bake/Cargo.toml
COPY crates/lm-serve/Cargo.toml crates/lm-serve/Cargo.toml

# Create stub lib/main files so cargo can resolve deps without full source.
RUN mkdir -p crates/lm-core/src  crates/lm-bake/src  crates/lm-serve/src \
 && echo "fn main(){}" > crates/lm-serve/src/main.rs \
 && touch crates/lm-core/src/lib.rs \
 && touch crates/lm-bake/src/lib.rs \
 && cargo build --release -p lm-serve 2>/dev/null || true

# Copy real source and build for real.
COPY crates/ crates/
RUN touch crates/lm-serve/src/main.rs && cargo build --release -p lm-serve

# ── stage 2: runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update -qq \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

RUN useradd -r -s /bin/false light-maps

COPY --from=builder /build/target/release/lm-serve /usr/local/bin/lm-serve

# Mount your .pmtiles files here at runtime.
VOLUME ["/data"]

USER light-maps
EXPOSE 3000

# Override CMD to pass your own .pmtiles paths and flags.
# Example: docker run ... -v ./tiles.pmtiles:/data/tiles.pmtiles
CMD ["/usr/local/bin/lm-serve", "/data/tiles.pmtiles", "--addr", "0.0.0.0:3000"]
