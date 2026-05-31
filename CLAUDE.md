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
      nexus-appset.yaml      # ApplicationSet — Git directory generator over infra/charts/*/*
    overlays/minikube/
      io.nexus.minikube-tunnel.plist   # LaunchDaemon: keeps minikube tunnel running as root
      io.nexus.argocd-port-forward.plist  # LaunchAgent: port-forward for ArgoCD UI (optional)
    root-app.yaml            # Bootstrap: apply once, then ArgoCD self-manages
    README.md                # Bootstrap steps, tunnel install, Kafka CLI reference
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
      chronicle/             # nexus chronicle binary (Helm chart, CronJob — EDGAR → earnings.calendar)
    market/
      market/                # nexus market binary (Helm chart, CronJob — Polygon → market.bars)
  vault/                     # Vault policies, roles, setup script, original values
  migrations/                # sqlx migration files (YYYYMMDDHHMMSS_description.sql)
```

### Adding a new infrastructure component

Drop a Helm chart directory under `infra/charts/<namespace>/<chart>/` containing
at minimum `Chart.yaml` and `values.yaml`. The ApplicationSet picks it up
automatically on the next ArgoCD sync — no Application CR required. The
namespace is derived from the directory path (`infra/charts/<namespace>/`).

For components that wrap an external Helm chart (e.g. an operator), declare it
as a dependency in `Chart.yaml` and prefix its values with the dependency name
in `values.yaml`.

### Key in-cluster addresses

| Service | In-cluster address | Host address (after tunnel) |
|---|---|---|
| Postgres (primary) | `nexus-postgres.nexus.svc.cluster.local:5432` | — |
| Kafka bootstrap (internal) | `nexus-kafka-bootstrap.nexus.svc.cluster.local:9092` | — |
| Kafka bootstrap (external) | — | `192.168.49.2:32092` |
| Vault | `vault.nexus.svc.cluster.local:8200` | — |

### Kafka CLI access from the host

The Kafka cluster exposes a fixed NodePort at `32092`. To reach it directly from
macOS you need `minikube tunnel` running as root. Install the LaunchDaemon once:

```bash
sudo cp infra/argocd/overlays/minikube/io.nexus.minikube-tunnel.plist \
        /Library/LaunchDaemons/
sudo launchctl load -w /Library/LaunchDaemons/io.nexus.minikube-tunnel.plist
```

With the daemon running `192.168.49.2` is permanently routable — no port-forward needed:

```bash
kafka-topics --bootstrap-server 192.168.49.2:32092 --list
kafka-console-consumer --bootstrap-server 192.168.49.2:32092 \
  --topic earnings.calendar --from-beginning
```

Install Kafka CLI tools: `brew install kafka`.

### Postgres credential secret

Auto-created by the Zalando operator:
```
nexus.nexus-postgres.credentials.postgresql.acid.zalan.do
```
Keys: `username`, `password`.
