# Image: <DOCKERHUB_USER>/nexus:<tag>
# All 5 service binaries are shipped in one image; each Kubernetes workload
# overrides `command` to select the correct binary:
#   chronicle  – SEC EDGAR earnings calendar ingestion
#   market     – Polygon EOD bar ingestion
#   earnings   – earnings binary
#   signal     – Kafka consumer / signal derivation
#   insight    – MCP server exposing derived results

# ── Builder ───────────────────────────────────────────────────────────────────
FROM rust:bookworm AS builder

# cmake       – required by rdkafka's cmake-build feature to compile librdkafka
# libssl-dev  – TLS support at build time
# pkg-config  – lets the build system locate system libraries
# libsasl2-dev – SASL authentication (Kafka broker auth)
# protobuf-compiler – provides `protoc`, required by prost-build 0.13+
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
    libssl-dev \
    pkg-config \
    libsasl2-dev \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY . .

# Builds all workspace binaries in release mode.
# Cargo resolves all workspace members automatically.
RUN cargo build --release

# ── Runtime ───────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# ca-certificates – TLS certificate verification at runtime
# libssl3         – OpenSSL runtime library
# libsasl2-2      – SASL runtime library (Kafka broker auth)
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    libsasl2-2 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/chronicle /usr/local/bin/chronicle
COPY --from=builder /build/target/release/market    /usr/local/bin/market
COPY --from=builder /build/target/release/earnings  /usr/local/bin/earnings
COPY --from=builder /build/target/release/signal    /usr/local/bin/signal
COPY --from=builder /build/target/release/insight   /usr/local/bin/insight
