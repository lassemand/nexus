ARG VERSION=0.0.0
ARG CARGO_CHEF_VERSION=0.1.78

# cargo-chef reads the real workspace to list its members and targets, so no
# crate has to be named here. chef, planner and builder share one toolchain,
# which is what lets the cooked dependency layer survive source changes.
FROM rust:bookworm AS chef

ARG CARGO_CHEF_VERSION

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
    libssl-dev \
    pkg-config \
    libsasl2-dev \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY rust-toolchain.toml ./
RUN rustup toolchain install \
    && cargo install cargo-chef --locked --version "${CARGO_CHEF_VERSION}"

FROM chef AS planner

COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

ARG VERSION

COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json

COPY . .
RUN sed -i "s/version = \"0\.0\.0\"/version = \"${VERSION}\"/" Cargo.toml \
    && cargo update --workspace \
    && find . -path ./target -prune -o -name '*.rs' -exec touch {} + \
    && cargo build --frozen --release

# sqlx-cli for initContainer-driven migrations (kept off the runtime image's
# critical path — only the compiled binary is copied out below). Matches the
# app's own sqlx feature set (rustls + postgres) to avoid pulling in a
# different TLS stack than what the services themselves use.
RUN cargo install sqlx-cli --version ^0.8 --no-default-features --features rustls,postgres

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    libsasl2-2 \
    procps \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --shell /usr/sbin/nologin nexus

COPY --from=builder /build/target/release/chronicle    /usr/local/bin/chronicle
COPY --from=builder /build/target/release/market      /usr/local/bin/market
COPY --from=builder /build/target/release/earnings    /usr/local/bin/earnings
COPY --from=builder /build/target/release/filings     /usr/local/bin/filings
COPY --from=builder /build/target/release/pdmr        /usr/local/bin/pdmr
COPY --from=builder /build/target/release/nordic-reports /usr/local/bin/nordic-reports
COPY --from=builder /build/target/release/nordic-calendar /usr/local/bin/nordic-calendar
COPY --from=builder /build/target/release/nordic-live /usr/local/bin/nordic-live
COPY --from=builder /build/target/release/saxo_stream /usr/local/bin/saxo_stream
COPY --from=builder /build/target/release/signal      /usr/local/bin/signal
COPY --from=builder /build/target/release/insight     /usr/local/bin/insight
COPY --from=builder /build/target/release/analyst     /usr/local/bin/analyst

# sqlx-cli + chronicle migration files for the saxo-stream initContainer —
# runs chronicle migrations before the app container starts to ensure schema
# is up to date (see deployment-saxo-stream.yaml). Application binaries still
# embed and run their own migrations via sqlx::migrate! at startup.
COPY --from=builder /usr/local/cargo/bin/sqlx /usr/local/bin/sqlx
COPY chronicle/migrations /app/migrations/chronicle

USER nexus
