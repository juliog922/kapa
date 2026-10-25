# =============================================================================
#  Kapa core service (Rust)
#    target tools   : toolchain for make format/lint/check-docs/test/bench (not shipped)
#    target runtime : the core binary on distroless (what docker compose runs)
#  Builder is bookworm so the binary's glibc matches distroless debian12.
# =============================================================================
ARG RUST_VERSION=1.98.1

# ---- tools -------------------------------------------------------------------
FROM rust:${RUST_VERSION}-bookworm AS tools
ARG LIZARD_VERSION=1.24.2
RUN rustup component add rustfmt clippy \
 && apt-get update \
 && apt-get install -y --no-install-recommends linux-perf python3-pip \
 && rm -rf /var/lib/apt/lists/* \
 && pip install --no-cache-dir --break-system-packages lizard==${LIZARD_VERSION}
# Caches live in a per-user volume mounted at /cache (world-writable so any UID works).
RUN mkdir -m 1777 /cache
ENV CARGO_HOME=/cache/cargo CARGO_TARGET_DIR=/cache/target HOME=/tmp

# ---- build -------------------------------------------------------------------
FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches
RUN cargo build --release --locked --bin core

# ---- runtime -----------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=build /src/target/release/core /core
USER nonroot:nonroot
HEALTHCHECK --interval=10s --timeout=5s --start-period=20s --retries=3 CMD ["/core", "--healthcheck"]
ENTRYPOINT ["/core"]
