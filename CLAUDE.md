# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`nexus` is a Rust workspace (edition 2021) for deriving insider trading signals from public market data.

## Crates

| Crate | Role |
|---|---|
| `model` | Common types and protobuf-generated messages shared across the workspace (`Bar`, `Asset`, `Price`, `MarketEvent`, `EarningsEvent`, etc.) |
| `alpha` | Library providing data-fetching interfaces (`BarProvider`, `PolygonBarProvider`, `PriceProvider`). Pure data access — no Kafka, no analysis. |
| `chronicle` | Ingestion binaries that fetch raw data and publish it as protobuf to Kafka. Two binaries: `chronicle` (SEC EDGAR → `earnings.calendar`) and `market` (Polygon EOD bars → `market.bars`). |
| `signal` | Consumes Kafka topics, derives trading signals, and persists results to Postgres. |
| `insight` | MCP server exposing derived results from `signal` to LLM clients. |

## Data flow

```
SEC EDGAR ──► chronicle ──► earnings.calendar (EarningsEvent)
                                                               ──► signal ──► Postgres ──► insight
Polygon ────► market ────► market.bars       (MarketEvent)
```

## Commands

```bash
cargo build                  # compile all crates
cargo build -p <crate>       # compile a single crate
cargo run --bin <binary>     # run a binary
cargo test                   # run all tests
cargo test -p <crate>        # test a single crate
cargo clippy                 # lint
cargo fmt                    # format
```

## Infrastructure

The cluster is managed via ArgoCD (GitOps). All manifests live under `infra/`.

### Layout

```
infra/
  argocd/
    apps/
      nexus-appset.yaml      # ApplicationSet — Git directory generator over infra/charts/*
    root-app.yaml            # Bootstrap: apply once, then ArgoCD self-manages
    README.md                # Bootstrap steps and Helm repo registration
  charts/
    signal/                  # nexus signal service (Helm chart)
    postgres-operator/       # Zalando Postgres Operator (umbrella Helm chart)
    postgres/                # nexus postgresql CR (nexus-postgres cluster)
    kafka/                   # Strimzi operator + nexus Kafka cluster CRs
    vault/                   # HashiCorp Vault standalone (umbrella Helm chart)
  kafka/                     # Reference manifests and README (source superseded by charts/)
  vault/                     # Vault policies, roles, setup script, original values
  migrations/                # sqlx migration files (YYYYMMDDHHMMSS_description.sql)
```

### Adding a new infrastructure component

Drop a Helm chart directory under `infra/charts/<name>/` containing at minimum
`Chart.yaml` and `values.yaml`. The ApplicationSet picks it up automatically on
the next ArgoCD sync — no Application CR required.

For components that wrap an external Helm chart (e.g. an operator), declare it
as a dependency in `Chart.yaml` and prefix its values with the dependency name
in `values.yaml`.

### Key in-cluster addresses

| Service | Address |
|---|---|
| Postgres (primary) | `nexus-postgres.nexus.svc.cluster.local:5432` |
| Kafka bootstrap | `nexus-kafka-bootstrap.nexus.svc.cluster.local:9092` |
| Vault | `vault.nexus.svc.cluster.local:8200` |

### Postgres credential secret

Auto-created by the Zalando operator:
```
nexus.nexus-postgres.credentials.postgresql.acid.zalan.do
```
Keys: `username`, `password`.
