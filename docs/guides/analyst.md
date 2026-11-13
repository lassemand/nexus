# analyst

The analyst prepares and runs per-report WASM kernels. Before each scheduled quarterly or annual
report of a registered Nordic company it starts a Kubernetes Job in which Claude Code writes a kernel
for that one report. When the release arrives on `special.events`, it runs the kernel
(`buy` / `no_buy` / `abstain` with the metrics found) and simulates a trade. Nothing is traded for real.
Crate: `analyst/README.md`. Chart: `infra/charts/analyst/analyst/README.md`.

## Architecture

```
signal special_events (scheduled) ─► scheduler ─► preparation Job (Claude Code, analyst-prep image)
                                         ▲                         │ uploads kernel (POST :8081/kernels)
nexus expect ─► analyst.notes ───────────┘                         ▼
                                                            accepted kernel (analyst DB)
special.events (release) ─► run kernel ─► decision ─► quotes from saxo-stream ─► simulated trade
                                              │                                      │
                                       analyst.decisions                       analyst.trades ─► signal ─► backtest DB
```

1. **Schedule.** Every `SCHEDULER_TICK` the scheduler reads signal's winning scheduled reports
   (read-only) and keeps a `report_targets` row each. `PREP_LEAD` (12 h) before a report it starts a
   preparation, at most `PREP_MAX_CONCURRENT` at a time, never inside `PREP_MIN_LEAD`.
2. **Prepare.** The Job (`analyst-prep` image, network-isolated) writes, tests and builds a kernel and
   uploads it; the analyst validates, loads and warms it.
3. **Decide.** The release path runs entirely inside the analyst pod and never waits on a database
   write or an external call it does not need.
4. **Simulate.** Entry at the ask of the first tradable quote, exit at the bid after `HOLD`, plus
   observations at `OBS_HORIZONS`. Decisions and trades go to `analyst.decisions` / `analyst.trades`;
   signal stores them for the backtest.

## Settings

Environment variables of the Deployment (chart `values.yaml`, `analyst.*`). Durations use `humantime`.

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL`, `SIGNAL_DATABASE_URL` | secret | Own database; signal's, opened read-only. |
| `KAFKA_BROKERS` | `nexus-kafka-bootstrap.nexus.svc.cluster.local:9092` | |
| `PREP_LEAD` | `12h` | Preparation starts this long before a report. |
| `PREP_MIN_LEAD` | `1h` | Latest start before a report. |
| `PREP_MAX_CONCURRENT` | `2` | Concurrent preparations. |
| `PREP_MAX_ATTEMPTS` | `3` | Preparations per report per cycle. |
| `PREP_TIMEOUT` | `1800` | Seconds a preparation Job may run. |
| `SCHEDULER_TICK` | `60s` | |
| `HOLD` | `30m` | Holding period of a simulated trade. |
| `OBS_HORIZONS` | `1m,5m,15m,30m,60m,close` | Observation points after entry. |
| `CLOSE_BUFFER` | `5m` | Margin before the close. |
| `ENTRY_MAX_WAIT` | `5m` | Longest wait for a tradable quote. |
| `MAX_RELEASE_AGE` | `15m` | Older releases are not traded. |
| `KERNEL_DEADLINE` | `50ms` | Kernel run limit. |
| `ANALYST_TICKERS` | empty | `TICKER:MIC,...` allowlist; empty is every registered company. |
| `NOTES_TOPIC` / `ANALYST_DECISIONS_TOPIC` / `ANALYST_TRADES_TOPIC` | `analyst.notes` / `analyst.decisions` / `analyst.trades` | |
| `SPECIAL_EVENTS_TOPIC` | `special.events` | Releases. |
| `SAXO_QUOTES_URL` | `http://saxo-quotes.chronicle.svc.cluster.local:8080` | Only namespace `analyst` may reach it. |
| `PREP_UPLOAD_URL`, `PREP_IMAGE`, `PREP_NAMESPACE` | see values | Where Jobs upload; their image; their namespace. |
| `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME` | Alloy `:4318`, `analyst` | Empty endpoint disables export. |

Settings the crate reads but the chart leaves at their defaults: `METRICS_PORT` (`9464`).

## Notes

`nexus expect` publishes a `ReportNote` on `analyst.notes`; the analyst stores it and, for a `ready`
target, prepares again (a `failed` target gets one more attempt). The next preparation receives the notes.

## Pausing

Set `analyst.tickers` to a ticker that does not exist (for example `NONE:XSTO`) to schedule nothing, or
`replicaCount: 0` to stop the service. Replicas 0 does not alert.

## Where things are kept

Kernels (never deleted; `spent` once their report is decided), preparation runs and notes: the `analyst`
database. Preparation transcripts: the Job logs while the Job exists (`kubectl -n analyst logs job/<name>`)
and the run record in `prep_runs`. Decisions and simulated trades: Kafka (`analyst.decisions`, 30 days;
`analyst.trades`, 90 days) and signal's `backtest` database.

## Tracing

Each release is one trace of the `analyst` service, root span `process special.events`, continuing the
trace nordic-live started at MFN's publish instant. Find it by ticker (Grafana → Explore → Tempo,
`{resource.service.name="analyst" && name="process special.events"}`), from the "Release traces" table on
the Analyst dashboard, or by the `trace_id` of a row in "Recent decisions". The holding period is its own
trace, linked to the release trace. Span details: `docs/guides/tracing.md`.

## Limitations

Forward-only (no history is replayed); simulated, with no fees, spread beyond the quotes or market impact;
a release before the open is entered at the first tradable quote of the session, so the entry includes the
opening move; every preparation costs LLM tokens (see the cost panel).
