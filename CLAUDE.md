# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`nexus` is a Rust workspace (edition 2021) for deriving insider trading signals from public market data.

## Crates

| Crate | Role |
|---|---|
| `model` | Common types and protobuf-generated messages shared across the workspace (`Bar`, `Asset`, `Price`, `MarketEvent`, `EarningsEvent`, etc.) |
| `alpha` | Library providing data-fetching interfaces (`BarProvider`, `PolygonBarProvider`, `PriceProvider`). Pure data access — no Kafka, no analysis. |
| `chronicle` | Ingestion binaries: `chronicle` (SEC EDGAR → `earnings.calendar`), `market` (Polygon EOD bars → `market.bars`), `earnings` (Polygon quarterly earnings → `earnings.calendar`), `nordic-reports` (MFN JSON news feed `mfn.se/all/a/<slug>.json` → `special.events`: publishes every report and correction as a `SpecialEvent`, keyed `ticker:exchange_mic`, and every financial-calendar release as a `report_calendar_release` event, and every other release (not a report, correction, calendar notice or calendar release) as a `company_announcement` event carrying its plain `text`; Finnish companies' managers' transaction releases are parsed and published to `insider.transactions` (`FIN_MAR`), see `docs/guides/nordic-companies.md`; incremental from a per-company cursor in `mfn_ingest_state`, `--backfill` pages the full history; daily 22:00 Europe/Stockholm CronJob, after a calendar announcement it also refreshes that company's calendar), `nordic-live` (Deployment: reads MFN's unofficial WebSocket, publishes each new report and correction, and each `company_announcement`, to `special.events` within seconds (announcements through a bounded queue and a producer task, never blocking the socket reader) and posts results releases and company announcements to Slack (separate channels, same bot; live-only; only items from the last `SLACK_MAX_AGE_MINUTES`); after every (re)connect a catch-up reads MFN's HTTP feed back to a persisted watermark (`nordic_live_state`) and feeds the same path; the poll is the fallback and the backfill), and `nordic-calendar` (MFN company page "Kalender" → one `report_calendar` snapshot per company on `special.events`, only when changed; daily 06:30 Europe/Stockholm CronJob). Reports are stored only by signal, in `special_events`; see `docs/guides/nordic-companies.md`. Shared Nordic code lives in the `chronicle_lib` library target. |
| `signal` | Consumes Kafka topics, derives trading signals, and persists results to Postgres. |
| `insight` | MCP server exposing derived results from `signal` to LLM clients. |
| `telemetry` | Shared metrics setup: `serve(service, port)` exposes `/metrics` for long-running services, `batch(job)` pushes a CronJob's metrics to the Pushgateway at the end of the run. Conventions in `docs/guides/metrics.md`. |
| `analyst` | Kernel runtime for the analyst feature: loads a WASM kernel (compiled from the stored bytes, never a persisted `.cwasm`), validates it (size, no imports, exact `memory`/`verdict_input`/`verdict_run` exports), instantiates it with a 64 MiB memory cap and a 50 ms epoch deadline (a thread ticks the epoch every 1 ms of wall time), warms it, and runs it on a release text (`KernelRuntime`, `LoadedKernel::run`). After any error the instance is rebuilt. Lib, bin `analyst` (so far the runtime, `/metrics`, the database migrations, the scheduler and the notes consumer) and bin `analyst-bench` (feature `bench`; native against WASM on the `verdict/kernels/bench-*` kernels, results in `analyst/BENCHMARK.md`). It also owns the `analyst` database (`analyst/migrations/`: report targets, kernels that are never deleted, preparation runs, notes), applied at startup, and its settings (`Config`, `analyst/README.md`), and the **report scheduler** (every `SCHEDULER_TICK`: reads the winning scheduled reports of signal's `special_events` read-only, keeps one `report_targets` row per report, and starts a preparation `PREP_LEAD` (12 h) before it through a `PrepLauncher`, at most `PREP_MAX_CONCURRENT` at a time, never inside `PREP_MIN_LEAD`; the service uses a `NoopLauncher` until the Job launcher exists) and the consumer of `analyst.notes` (`ReportNote`, produced by `nexus expect`; a new note makes a `ready` target prepare again, and gives a `failed` one one more attempt). Its queries are plain `sqlx::query`/`query_as`, like every other crate. Metrics in `docs/guides/metrics.md`. |
| `verdict-sdk` (`verdict/sdk`) | SDK for analyst **kernels**: per-report Rust crates compiled to WASM that read a release and return `buy` / `no_buy` / `abstain` with the metrics found. Holds only the WASM ABI (`export_kernel!`, `run_into`), the types (`Lang`, `Report`, `Metric`, `Decision`, `Verdict`) and, behind the `harness` feature, a test and timing harness; no search or parsing: each kernel owns its lookup. `verdict/kernels/sample` (`verdict-sample`) is a skeleton kernel that reads nothing and assumes no language; `verdict/template` (excluded from the workspace) is what the agent edits; rules in `infra/agents/analyst.md`. `verdict/prep/` is what the preparation Job runs (entrypoint, `PROMPT.md`, smoke and entrypoint tests, README): the image `infra/Dockerfile.analyst-prep` (release workflow `release-analyst-prep.yml`, tags `analyst-prep-v*`, image `nexus-analyst-prep`) holds the pinned Claude Code, the toolchain, vendored crates and a pre-built target, and the entrypoint lets Claude Code (`--agent analyst`) write, test and build one kernel, then uploads it for acceptance. |
| `conductor` | Webhook router on the cluster: receives Linear/GitHub webhooks via in-pod Hookdeck sidecars and runs one Claude Code session per task set, each in its own worktree. |

Calendar sources: chronicle only emits source data (`report_calendar` for the MFN page, `report_calendar_release` for each calendar release); signal keeps each source's scheduled rows in `special_events` and resolves which wins, newest source (`calendar_fetched_at`) first (`docs/guides/nordic-companies.md`).

Every Nordic country fact (currency, time zone, venues and session hours, Yahoo/Saxo ids, holiday rules, insider-source kind, languages) lives in `model/src/nordic/` (one file per country, `COUNTRIES` is the list); see `docs/guides/adding-a-nordic-country.md`.

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
| Nordic (FNSE, XSTO, XCSE, FNDK, XHEL, FNFI) | Saxo Bank OpenAPI | Official daily bars from `saxo-stream` (below); the live price stream was removed (NEX-193). See `docs/adr/0001-saxo-openapi-nordic-data-source.md` |

### Nordic daily prices

`signal.daily_bars` (`ticker, exchange_mic, date`, `NUMERIC(20,6)` OHLC, volume) is the **source of truth for Nordic daily prices**. It is fed by `market.daily_bars` (`DailyBar` protobuf, prices as decimal strings), which `saxo-stream` publishes from Saxo's chart API (`/chart/v1/charts`, `Horizon=1440`): a full backfill to 2015-01-01 per ticker on first sight, then the last 10 weekdays again on startup and daily at 18:30 Europe/Stockholm (signal upserts, so corrections overwrite). `saxo-stream` covers every registered company on a MIC in `SAXO_MICS` (default `FNSE,XSTO,XCSE,FNDK,XHEL,FNFI`; the Helsinki ExchangeIds `HSE`/`HSE_FN-FI` are still unverified against `/ref/v1/exchanges`, override with `SAXO_EXCHANGE_IDS` if they are wrong). Instruments are resolved on Saxo by ISIN (else ticker text, also with `-` as a space) **and the expected ExchangeId** (`alpha::saxo::exchange_ids`, overridable with `SAXO_EXCHANGE_IDS=MIC=Id,...`; ambiguity is an error). It runs inside `saxo-stream` because Saxo refresh tokens rotate on use: `saxo-stream` owns the token refresh, and the chart client only reads its access token and never authenticates. Progress is kept in chronicle's `saxo_daily_bars_state` (delete a row to re-backfill that ticker). The bar date is the date of Saxo's sample `Time` in the exchange's own time zone (`model::exchange`, agreeing with `exchanges.timezone`). Days Saxo omits are absent, never filled forward. Metrics: `saxo_daily_bars_last_success_timestamp`, `saxo_daily_bars_failures_total`, and per instrument `saxo_daily_bars_latest_date_timestamp_seconds{ticker,exchange_mic}`; the rest in `docs/guides/metrics.md`, dashboard "Saxo".

### Nordic report reactions

`signal` also stores every `company_announcement` event as a `special_events` row (`event_type = 'company_announcement'`, the release text in `body_text`, language twins merged, English preferred; `docs/guides/nordic-companies.md`); their share-price reaction is computed like a report's, see below. `signal` stores each released Nordic report's and company announcement's share-price reaction **on its own `special_events` row** (`release_session`, `reaction_date`, `base_date`, `base_close`, `reaction_close`, `move_pct`, `reaction_volume`, `data_status`, `needs_review`, `reaction_computed_at`; NULL for every other event; qualifying rows are quarterly and annual reports and `company_announcement` rows, never corrections). Announcements use exactly the same definitions as reports. Several releases of one company on one reaction day (a report and an announcement, or two announcements) get the same reaction day and the same `move_pct`: it is the raw price change of that day and is not attributed to one release. A language twin merge keeps the earliest `occurred_at`, so the reaction comes from the first publish instant. Definitions (`signal/src/reactions.rs`): the release instant in the exchange's own time zone is `pre_open` (before the open), `intraday` (until the close), `after_close`, or `non_trading_day`; open, close, half-day close, time zone and country come from the `exchanges` table (`FNSE`/`XSTO`: Europe/Stockholm 09:00–17:30, half day 13:00; `XCSE`/`FNDK`: Europe/Copenhagen 09:00–17:00, no half days); the reaction day is the release day for the first two and the next trading day otherwise; the base is the last close on or before the previous trading day; `move_pct = (close[r] - close[b]) / close[b] * 100` in `NUMERIC`. Trading days come from `trading_holidays` for the exchange's country (`exchanges.country`: `FNSE`/`XSTO` -> `SE`, `XCSE`/`FNDK` -> `DK`); outside the years the table covers, or for a MIC missing from `exchanges`, the status is `calendar_unknown` (never guessed). `data_status` is `ok` or why there is no move (`missing_reaction_bar`, `missing_base_bar`, `future`, `zero_base`, `stale_base`, `calendar_unknown`). `needs_review` flags `|move| > 40` (usually a split or share issue in unadjusted prices). Recomputed when a report is stored (all the company's rows), when an announcement is stored (that row only, so a backfill of thousands stays linear), when daily bars arrive (debounced per instrument), and once at signal startup, where the sweep is bounded to rows never computed, not `ok`, or with a reaction day within 10 days of today.

Known limitations: `bars` (fed by `market.bars`) holds only the **last minute bar** of each day for Nordic tickers, so its OHLC and volume are not daily values; use `daily_bars`. It is **unverified** whether Saxo's daily samples are adjusted for splits and share issues (the docs are silent and `/chart/v1` is unavailable on SIM): treat them as unadjusted.

### Nordic live quotes

`saxo-stream` serves the system's only live-price read: `GET /quote?ticker=<TICKER>&mic=<MIC>` on `QUOTE_PORT` (8080, exposed by the chart's ClusterIP Service `saxo-quotes` on port 8080; the NetworkPolicy `saxo-quotes` admits only namespace `analyst` to it). `/health`, `/metrics` and `POST /tokens` are on `HEALTH_PORT` (8081, NodePort 32080). It calls Saxo `/trade/v1/infoprices` once (`alpha::saxo::quote`, own keep-alive client, no throttle, sleep or retry) and answers bid/ask/sizes as **decimal strings** (raw JSON digits, padded to `DisplayAndFormat.Decimals`, never via `f64`), Saxo's `PriceType*`/`MarketState` strings unchanged, `null` for anything Saxo omits. Without a valid token every request is 503 `{"retry_after_ms"}` (checked before the map, so bootstrap never reports a registered company missing); with one, 404 for a pair without a Uic in the in-memory map (warmed at startup from the registered companies before the first daily-bar sweep, then kept current by each sweep's company reload; an unknown pair is resolved in the background), 503 `{"retry_after_ms"}` for no token or a 429 (`Retry-After`, capped at 2000), 502 for any other Saxo error, 400 for missing parameters. The client can only build info-price URLs; no order, position or account endpoint may exist. Metrics `saxo_quote_*` (`docs/guides/metrics.md`).

### Companies are runtime data

Companies are registered at runtime (`nexus register` / `nexus unregister`) and
mapped in the `companies` tables. Never hard-code an individual company in code,
migrations, dashboards or docs: no seed `UPDATE`s, no per-company branches, no
per-company guide sections. Docs use placeholders (`<TICKER>`, `<slug>`); tests
may use real tickers as example fixtures. (Two early migrations seed GomSpace
and Novo Nordisk; they are applied and cannot be edited, and do nothing on a
database without those rows.)

### Recorded fixtures: do not read

Never read the files in `chronicle/tests/fixtures/mfn/e2e/`, neither the recorded
MFN feed pages (`*.json`) nor the captured events (`*.pb`). They are large raw
recordings (hundreds of KB each, protobuf is binary) and add nothing a task needs.
Everything about them (what they contain, how to re-record them, the expected
values) is in the doc comments of `chronicle/tests/announcements_e2e.rs` and
`signal/src/e2e_announcements.rs`; work from those, and regenerate the `*.pb`
files with `REFRESH_E2E_FIXTURES=1` instead of editing them.

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
    overlays/orbstack/
      argocd-server-service.yaml  # argocd-server as a LoadBalancer, pinned to 192.168.139.11
    root-app.yaml            # Bootstrap: apply once, then ArgoCD self-manages
    README.md                # Bootstrap steps and Kafka CLI reference
  agents/                    # Claude agent definitions (backend.md, analyst.md); the conductor chart's files/agents is a symlink to it
  charts/                    # Layout: infra/charts/<namespace>/<chart>/
    nexus/
      kafka/                 # Strimzi operator + Kafka cluster CRs (NodePort 32092 external)
      postgres-operator/     # Zalando Postgres Operator (umbrella Helm chart)
      postgres/              # nexus-postgres cluster CR
      vault/                 # HashiCorp Vault standalone (umbrella Helm chart)
      external-secrets/      # External Secrets Operator + ClusterSecretStore (Vault backend)
      prometheus/            # kube-prometheus-stack umbrella (operator, Prometheus, Alertmanager, node-exporter, kube-state-metrics, nexus alert rules; Grafana disabled)
      loki/                  # Grafana Loki (SingleBinary) umbrella on Backblaze B2, 30-day retention; see docs/guides/loki.md
      pushgateway/           # Prometheus Pushgateway umbrella (CronJob metrics, ServiceMonitor, persistent); see docs/guides/metrics.md
      alloy/                 # Grafana Alloy DaemonSet shipping all pod logs to Loki; see docs/guides/loki.md
      servicelb/             # label-only Node manifest that turns off k3s ServiceLB (MetalLB serves LoadBalancers)
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

Node metrics (`node_*` from node-exporter) describe the OrbStack Linux VM that hosts the cluster, not the Mac mini's macOS.

### Key addresses

| Service | In-cluster | Host |
|---|---|---|
| Postgres (primary) | `nexus-postgres.nexus.svc.cluster.local:5432` | — |
| Kafka bootstrap (internal) | `nexus-kafka-bootstrap.nexus.svc.cluster.local:9092` | — |
| Kafka bootstrap (external) | — | `localhost:32092` |
| Vault | `vault.nexus.svc.cluster.local:8200` | `localhost:32200` |
| Prometheus | `prometheus-kube-prometheus-prometheus.nexus.svc.cluster.local:9090` | `localhost:32090` |
| Alertmanager (ClusterIP, not exposed) | `prometheus-kube-prometheus-alertmanager.nexus.svc.cluster.local:9093` | — |
| Loki (ClusterIP, not exposed; Grafana datasource `Loki`) | `loki.nexus.svc.cluster.local:3100` | — |
| Pushgateway (ClusterIP, not exposed; CronJobs POST to `/metrics/job/<job>`) | `pushgateway.nexus.svc.cluster.local:9091` | — |
| Grafana | — | `localhost:32300` |
| ArgoCD | — | `localhost:30443` |

The `localhost:` addresses are what you use from a laptop, through a port
forward. On the Mac mini itself they are not listening; there, services are
reached through MetalLB LoadBalancer IPs in `192.168.139.10-50`. Pinned:
Kafka external bootstrap `192.168.139.10:9093` (use this for `KAFKA_BROKERS` on
the host), ArgoCD `192.168.139.11`, Grafana `192.168.139.12`, Kafka broker 0
`192.168.139.13`. The pinned addresses are their own MetalLB pool
(`orbstack-pinned`, `.10-.13`, never assigned automatically; chart
`infra/charts/metallb-system/metallb-pools/`), so a new pin must be in that
range and requested with `metallb.universe.tf/loadBalancerIPs`. The others come
from `orbstack-pool` (`.14-.50`) and are reassigned when the VM restarts, so
read them with `kubectl --context orbstack get svc -A | grep LoadBalancer`.

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
- The model is per set. `CONDUCTOR_CLAUDE_MODEL` (chart `claude.model`) is the default; a set overrides it with a `model: <alias-or-id>` line in its `session:<name>` label's description in Linear. It is read each time the set's session is started or resumed (a live session is never switched), validated like the global value, and any problem (no label, invalid value, Linear error) falls back to the default with a warning. Sets of one and PR sets always use the default. `sessions list` shows the effective model and whether it came from the set or the default.
- Put the same label on tasks that build on the same code, and a different or no label on unrelated work.
- Metrics (`conductor_*`, port 9464, own `PodMonitor`) and alerts are on the Grafana dashboard "Conductor" (folder Nexus).

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

## Metrics retention

Prometheus keeps 14 days of full-detail metrics (`retention: 14d`, plus `retentionSize: 9GB` as a hard cap) on a 20Gi `local-path` PVC; Alertmanager has a 1Gi PVC. A Thanos sidecar uploads every 2-hour block to the Backblaze B2 bucket `nexus-metrics`, where the compactor keeps raw data 14 days, 5-minute data 30 days and 1-hour data 90 days, then deletes it (`docs/guides/metrics-longterm.md`). Grafana datasource `Thanos` (`nexus-thanos`) queries both; `Prometheus` stays the default. Data survives pod restarts. **Deleting the PVC deletes the last 14 days of local history** (B2 keeps everything uploaded). `local-path` does not enforce PVC size, hence the size cap.
