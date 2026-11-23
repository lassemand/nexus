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
special.events (release) ─► run kernel ─► decision ─► streamed quotes from saxo-stream ─► simulated trade
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
   observations at `OBS_HORIZONS`. Prices are read from memory: the analyst keeps the latest quote of every watched
   instrument from `saxo-stream`'s `/quotes/stream` WebSocket. `GET /quote` is only the fallback while the stream is not
   healthy, and a fallback quote older than `QUOTE_MAX_AGE` is refused (`quote_too_old`). Each trade and observation
   records its `quote_source` (`stream` or `request`), Saxo's timestamp on the quote and its age. Decisions and trades go to `analyst.decisions` / `analyst.trades`;
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
| `HOLD` | `2h` | Holding period of a simulated trade. |
| `OBS_HORIZONS` | `1m,5m,15m,30m,60m,close` | Observation points after entry. |
| `CLOSE_BUFFER` | `5m` | Margin before the close. |
| `ENTRY_MAX_WAIT` | `5m` | Longest wait for a tradable quote. |
| `QUOTE_MAX_AGE` | `1h` | Oldest Saxo timestamp a fallback quote may have; applies to `GET /quote` only. |
| `WATCH_LEAD` | `12h` | Targets are watched on the stream from this long before their expected time; trades in progress stay watched. |
| `QUOTE_STREAM_URL` | `ws://saxo-quotes.chronicle.svc.cluster.local:8080/quotes/stream` | `saxo-stream`'s quote WebSocket (same Service and port as `GET /quote`). |
| `QUOTE_STREAM_HEARTBEAT_TIMEOUT` | `3s` | The stream is unhealthy without a heartbeat for this long, or when the heartbeat says Saxo is not connected. |
| `MAX_RELEASE_AGE` | `15m` | Older releases are not traded. |
| `HOLD`, `OBS_HORIZONS`, `CLOSE_BUFFER`, `QUOTE_CONCURRENCY`, `SAXO_QUOTES_URL`, `TRADES_TOPIC` | see `analyst/README.md` | Simulated trades. |
| `KERNEL_DEADLINE` | `50ms` | Kernel run limit. |
| `ANALYST_TICKERS` | empty | `TICKER:MIC,...` allowlist; empty is every registered company. |
| `NOTES_TOPIC` / `ANALYST_DECISIONS_TOPIC` / `ANALYST_TRADES_TOPIC` | `analyst.notes` / `analyst.decisions` / `analyst.trades` | |
| `SPECIAL_EVENTS_TOPIC` | `special.events` | Releases. |
| `SAXO_QUOTES_URL` | `http://saxo-quotes.chronicle.svc.cluster.local:8080` | Only namespace `analyst` may reach it; the fallback `GET /quote`. |
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

## The backtest

`signal` consumes `analyst.decisions` and `analyst.trades` (env `ANALYST_DECISIONS_TOPIC`, `ANALYST_TRADES_TOPIC`)
into `analyst_decisions` (by `news_id`), `simulated_trades` (by `trade_id`) and `simulated_trade_observations`. A
trade is written only when its `version` is higher than the stored one, and its observations are replaced in the
same transaction, so reordered or redelivered messages never overwrite newer state. A trade and its decision come from
different topics and may arrive in either order, so they are joined on `news_id` (indexed, no foreign key) when read.
`AnalystTrade.news_id` (field 32) carries the key. The quote-source columns (`entry_quote_source`, `*_quote_saxo_time`,
`*_quote_age_ms`, observation `quote_source`/`quote_age_ms`) are stored from NEX-271's fields; a source other than
`stream` or `request` rejects the message. Prices are `NUMERIC(20,6)`, `pnl_pct` `NUMERIC(12,4)`. A decimal that does not parse rejects the whole message.

`insight` reads them with `fetch_analyst_trades(ticker?, decision?, since?, include_shadow = true, limit = 200)` and
`summarise_analyst_trades(since?, ticker?)`. The results are forward-only and simulated (no look-ahead, no backfill),
per share, without fees or position sizing; a release before the open measures the move after the open; shadow trades
(`no_buy`, `abstain`, `skipped_no_kernel`) show what would have happened and are not actions taken. The summary takes
P&L only from `closed` trades and observations with status `ok` that were not late (`(bid(horizon) - entry_ask) / entry_ask * 100`, in SQL `NUMERIC`; late observations are counted separately) and reports how many
trades were excluded and why. The release session (pre-open or intraday) is found by the same company and an `occurred_at` within 120 s of the trade's publish time, because signal replaces a stored release's `source_id` with the English copy's id when it merges language twins. `trade_results` belongs to the old US earnings strategy and is untouched.

## Dashboards

Two Grafana dashboards (folder Nexus) read the analyst; each links to the other.

- **Analyst** (`nexus-analyst`): operations. Preparation runs, kernels, latency p50/p99, quote stream health, recent decisions. It answers "is the analyst running and fast?".
- **Analyst results** (`nexus-analyst-results`, signal's `backtest` database, default range 90 days, refresh 1 min): it answers "does it make money?", "do the kernel's choices beat buying blindly?" and "how fast is each report processed?". Rows: *Results* (time filter on `exit_at`: tiles, gain per report, closed-trade table, compounded curve, gain by holding time), *Speed per report* (on `decided_at`: stage timings per decision, with a Tempo link, and publish to decision on a log scale) and *Coverage* (on `decided_at`: decisions per ISO week by outcome, excluded trades by status). Variables `ticker` and `exchange_mic` (multi, All) come from `simulated_trades`; `include_shadow` (default false) adds shadow trades to the gain-per-report bars.

Only `closed` trades with `-100 < pnl_pct` count toward any gain; every other trade is counted as excluded, never as 0 %. A gain is per share, spread included, no fees; a release before the open is bought at the open.

**Reading the compounded curve.** Each line is `exp(sum(ln(1 + pnl_pct/100))) - 1` over the closed trades in exit order, starting at the first trade inside the chosen time range (+10 % then -10 % gives -1 %, not 0 %). It assumes all money is reinvested in every trade, so trades that overlap on the same morning make it optimistic. *Strategy* is the closed `buy` trades, *Buy every report* all closed trades (shadow included) and *Not bought* the closed `no_buy` shadow trades: the strategy beats buying blindly only when its line ends above *Buy every report*.

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
