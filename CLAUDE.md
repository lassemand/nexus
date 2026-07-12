# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`nexus` is a Rust workspace (edition 2021) for deriving insider trading signals from public market data.

## Crates

| Crate | Role |
|---|---|
| `model` | Common types and protobuf-generated messages shared across the workspace (`Bar`, `Asset`, `Price`, `MarketEvent`, `EarningsEvent`, etc.) |
| `alpha` | Library providing data-fetching interfaces (`BarProvider`, `PolygonBarProvider`, `PriceProvider`). Pure data access — no Kafka, no analysis. |
| `chronicle` | Ingestion binaries: `chronicle` (SEC EDGAR → `earnings.calendar`), `market` (Polygon EOD bars → `market.bars`), `earnings` (Polygon quarterly earnings → `earnings.calendar`). |
| `signal` | Consumes Kafka topics, derives trading signals, and persists results to Postgres. |
| `insight` | MCP server exposing derived results from `signal` to LLM clients. |

## Data flow

```
SEC EDGAR ──► chronicle ──► earnings.calendar (EarningsEvent)
                                                               ──► signal ──► Postgres ──► insight
Polygon ────► market ────► market.bars       (MarketEvent)
```

## Data sources

| Market | Source | Notes |
|---|---|---|
| US equities (XNAS/XNYS) | Polygon API | Used by `chronicle/market` and `alpha::PolygonBarProvider` |
| Nordic/First North (FNSE) | Saxo Bank OpenAPI | Planned — see `docs/adr/0001-saxo-openapi-nordic-data-source.md` |

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

The cluster is managed via ArgoCD (GitOps). All manifests live under `infra/`. The cluster runs on minikube (Docker driver) started with `--ports` bindings so all NodePorts are accessible at `localhost` with no tunnel or port-forward.

### Layout

```
infra/
  argocd/
    apps/
      nexus-appset.yaml      # ApplicationSet — Git directory generator over infra/charts/*/*
    overlays/minikube/
      io.nexus.argocd-port-forward.plist  # LaunchAgent: port-forward for ArgoCD UI (optional)
    root-app.yaml            # Bootstrap: apply once, then ArgoCD self-manages
    README.md                # Bootstrap steps and Kafka CLI reference
  charts/                    # Layout: infra/charts/<namespace>/<chart>/
    nexus/
      kafka/                 # Strimzi operator + Kafka cluster CRs (NodePort 32092 external)
      postgres-operator/     # Zalando Postgres Operator (umbrella Helm chart)
      postgres/              # nexus-postgres cluster CR
      vault/                 # HashiCorp Vault standalone (umbrella Helm chart)
      external-secrets/      # External Secrets Operator + ClusterSecretStore (Vault backend)
    signal/
      signal/                # nexus signal service (Helm chart, Deployment)
    chronicle/
      chronicle/             # chronicle, market and earnings CronJobs (shared Helm chart)
    market/
      market/                # legacy market CronJob chart (superseded by chronicle/)
```

### Adding a new infrastructure component

Drop a Helm chart directory under `infra/charts/<namespace>/<chart>/` containing at minimum `Chart.yaml` and `values.yaml`. The ApplicationSet picks it up automatically on the next ArgoCD sync — no Application CR required. The namespace is derived from the directory path (`infra/charts/<namespace>/`).

### Key addresses

| Service | In-cluster | Host |
|---|---|---|
| Postgres (primary) | `nexus-postgres.nexus.svc.cluster.local:5432` | — |
| Kafka bootstrap (internal) | `nexus-kafka-bootstrap.nexus.svc.cluster.local:9092` | — |
| Kafka bootstrap (external) | — | `localhost:32092` |
| Vault | `vault.nexus.svc.cluster.local:8200` | `localhost:32200` |
| Prometheus | — | `localhost:32090` |
| Grafana | — | `localhost:32300` |
| ArgoCD | — | `localhost:30443` |

### Postgres credential secret

Auto-created by the Zalando operator:
```
nexus.nexus-postgres.credentials.postgresql.acid.zalan.do
```
Keys: `username`, `password`.
