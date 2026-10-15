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
| `conductor` | Webhook router on the cluster: receives Linear/GitHub webhooks via in-pod Hookdeck sidecars and runs one Claude Code session per task set, each in its own worktree. |

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

The cluster is managed via ArgoCD (GitOps). All manifests live under `infra/`. The cluster runs on OrbStack — use the `orbstack` kubectl context.

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
    conductor/
      conductor/             # conductor webhook router + Hookdeck sidecars (namespace `conductor`)
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

## Session sets

`conductor` (namespace `conductor`) turns Linear and GitHub webhooks into Claude Code work. Tasks are grouped into **sets**; each set has one Claude session (resumed with `--resume`) and one git worktree.

- Issues with the same Linear label `session:<name>` form a set. An issue without a session label is a set of one.
- Names must match `^[a-z0-9][a-z0-9-]{0,62}$`. An issue with more than one `session:` label runs on its own, and a warning is logged.
- Moving an issue into Todo dispatches it to its set's session. Runs within a set are serial; different sets run in parallel, up to `CONDUCTOR_MAX_SESSIONS` (5).
- When every issue in a set is Done or Canceled, conductor closes the set automatically: the worktree is removed and the registry row deleted; branches and transcripts are kept. It never closes on a failed or empty Linear answer, and never forces a dirty worktree (shown as `close blocked`).
- A task added to a set after it closed starts a fresh session.
- Put the same label on tasks that build on the same code, and a different or no label on unrelated work.

### Live sessions

Each set is an interactive Claude running in a tmux session inside the pod.

- Any number of people can attach at once. Attach is read-write by default; use `--read-only` to watch.
- Detach with `Ctrl-a d` (the pod's tmux prefix is `Ctrl-a`; press it twice inside a local tmux that also uses `Ctrl-a`).
- A prompt you type is your turn: the set becomes busy, and conductor waits for it to finish before pasting the next task.
- `delivery blocked` means a pasted task was not confirmed. Attach, check the input box, then `release`.

### Operator commands

Conductor only runs in the pod, so use the wrapper, which `kubectl exec`s into it:

```bash
scripts/conductor sessions list [--json]       # live sets and their state
scripts/conductor sessions list --closed       # recently closed sets (audit trail)
scripts/conductor sessions attach <set> [--read-only]
scripts/conductor sessions interrupt <set>     # Escape: interrupt the current turn, session stays alive
scripts/conductor sessions release <set>       # clear delivery blocked / a stuck state
scripts/conductor sessions kill <set> [--drain]  # kill the tmux session; --drain also discards queued events
scripts/conductor sessions reset <set>         # fresh Claude conversation, keeps the worktree
scripts/conductor sessions close <set> [--force] # remove worktree and forget the set; --force detaches clients, kills a running turn, discards queued events and uncommitted changes
```

## GitHub PR comment handling

GitHub PR comments arrive as channel events (`pr_comment` or `pr_review_comment`) with fields: `repo`, `pr_number`, `comment_id`, `body`, and (for review comments) `file_path`.

When a comment arrives, apply this decision process:

1. **Clear and actionable** — implement the fix on the PR branch, push it, reply on the comment describing what was changed and why, then resolve the thread using `pull_request_review_write` with `method: resolve_thread`.
2. **Unclear or ambiguous** — reply asking a focused question about what specifically should change. Do not guess or make changes.
3. **Does not make sense** — reply explaining why the current code is correct and the comment does not apply.

Comments are routed to the set whose worktree produced the PR branch. A PR that no set owns gets a `gh-<repo>-pr-<n>` set, which is closed manually (`scripts/conductor sessions close`).

Never silently ignore a comment. Always reply.
