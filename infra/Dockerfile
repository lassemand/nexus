ARG VERSION=0.0.0

FROM rust:bookworm AS builder

ARG VERSION

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
    libssl-dev \
    pkg-config \
    libsasl2-dev \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY model/Cargo.toml model/Cargo.toml
COPY model/build.rs  model/build.rs
COPY model/proto/    model/proto/
COPY alpha/Cargo.toml    alpha/Cargo.toml
COPY chronicle/Cargo.toml chronicle/Cargo.toml
COPY signal/Cargo.toml   signal/Cargo.toml
COPY insight/Cargo.toml  insight/Cargo.toml
COPY cli/Cargo.toml      cli/Cargo.toml
COPY mcp/Cargo.toml      mcp/Cargo.toml

# Create placeholder sources so Cargo can parse the workspace
RUN mkdir -p \
      model/src alpha/src chronicle/src signal/src insight/src cli/src mcp/src \
    && touch model/src/lib.rs alpha/src/lib.rs \
    && printf 'fn main() {\n    println!("Hello, world!");\n}\n' | tee \
        chronicle/src/main.rs \
        chronicle/src/market.rs \
        chronicle/src/earnings_bin.rs \
        chronicle/src/filings.rs \
        signal/src/main.rs \
        signal/src/scanner.rs \
        insight/src/main.rs \
        cli/src/main.rs \
        mcp/src/main.rs

# Download dependencies as a separate layer — only re-runs when Cargo.lock changes
RUN cargo fetch --locked

# Compile dependencies — cached as long as manifests are unchanged
RUN cargo build --frozen --release

COPY . .
RUN sed -i "s/version = \"0\.0\.0\"/version = \"${VERSION}\"/" Cargo.toml \
    && cargo update --workspace \
    && touch \
        model/src/*.rs \
        alpha/src/*.rs \
        chronicle/src/*.rs \
        signal/src/*.rs \
        insight/src/*.rs \
        cli/src/*.rs \
        mcp/src/*.rs \
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

COPY --from=builder /build/target/release/chronicle /usr/local/bin/chronicle
COPY --from=builder /build/target/release/market    /usr/local/bin/market
COPY --from=builder /build/target/release/earnings  /usr/local/bin/earnings
COPY --from=builder /build/target/release/filings   /usr/local/bin/filings
COPY --from=builder /build/target/release/signal    /usr/local/bin/signal
COPY --from=builder /build/target/release/insight   /usr/local/bin/insight

# sqlx-cli + raw migration files, for initContainer-driven migrations only —
# the application binaries above still embed and run their own migrations
# via sqlx::migrate! at startup; this is strictly for the separate
# signal/migrations set that saxo-stream's initContainer applies before the
# app container starts (see infra/charts/chronicle/chronicle/templates/
# deployment-saxo-stream.yaml).
COPY --from=builder /usr/local/cargo/bin/sqlx /usr/local/bin/sqlx
COPY signal/migrations /app/migrations/signal

USER nexus
