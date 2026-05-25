FROM rust:bookworm AS builder

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

# Download dependencies as a separate layer — only re-runs when Cargo.lock changes
RUN cargo fetch --locked

# Compile dependencies using placeholder sources — cached as long as manifests are unchanged
RUN mkdir -p \
      model/src alpha/src chronicle/src signal/src insight/src cli/src mcp/src \
    && touch model/src/lib.rs alpha/src/lib.rs \
    && printf 'fn main(){}' | tee \
        chronicle/src/main.rs \
        chronicle/src/market.rs \
        chronicle/src/earnings_bin.rs \
        signal/src/main.rs \
        insight/src/main.rs \
        cli/src/main.rs \
        mcp/src/main.rs \
    && cargo build --frozen --release

COPY . .
RUN touch \
      model/src/*.rs \
      alpha/src/*.rs \
      chronicle/src/*.rs \
      signal/src/*.rs \
      insight/src/*.rs \
      cli/src/*.rs \
      mcp/src/*.rs \
    && cargo build --frozen --release

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    libsasl2-2 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --shell /usr/sbin/nologin nexus

COPY --from=builder /build/target/release/chronicle /usr/local/bin/chronicle
COPY --from=builder /build/target/release/market    /usr/local/bin/market
COPY --from=builder /build/target/release/earnings  /usr/local/bin/earnings
COPY --from=builder /build/target/release/signal    /usr/local/bin/signal
COPY --from=builder /build/target/release/insight   /usr/local/bin/insight

USER nexus
