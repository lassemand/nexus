# analyst

Prepares and runs per-report WASM kernels. Before each upcoming quarterly or annual
report of a registered Nordic company, a Claude agent writes a small kernel for that
one report; when the release arrives, the analyst runs it on the release text and
simulates a trade (nothing is traded for real).

This crate holds:

* the **kernel runtime** (`KernelRuntime`, `LoadedKernel`): loads a WASM kernel
  with no imports, a 64 MiB memory cap and a 50 ms deadline, and runs it. Results and
  how it was measured: `BENCHMARK.md`;
* the **database** (`migrations/`, `Store`): report targets, kernels, preparation
  runs and notes, applied by the service at startup;
* the **scheduler** (`Scheduler`): decides, per upcoming report, when to start
  preparing a kernel;
* the **notes consumer** (`NoteHandler`): stores the notes users add through
  `nexus expect` and wakes the scheduler.

Not here yet: the launcher that starts the real preparation Job and accepts kernels
(the service uses `NoopLauncher`), releases and trades.

## Configuration

All settings are environment variables. Durations use the syntax of the `humantime`
crate: `12h`, `1h30m`, `50ms`.

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | required | The analyst's own database (`analyst`). Migrations run at startup. |
| `SIGNAL_DATABASE_URL` | required | Signal's database. Opened read-only (`default_transaction_read_only = on`) and lazily. |
| `KAFKA_BROKERS` | required | Bootstrap servers for the notes consumer. |
| `PREP_LEAD` | `12h` | How long before a report its preparation starts. |
| `PREP_MIN_LEAD` | `1h` | A preparation that cannot finish this long before the report is not started, and a retry that would land inside it is not scheduled. |
| `PREP_MAX_CONCURRENT` | `2` | Most preparations running at once. |
| `PREP_MAX_ATTEMPTS` | `3` | Most preparations started for one report in one cycle. A failed run is retried after 5 minutes. |
| `SCHEDULER_TICK` | `60s` | How often the schedule is read (also right after a note arrives). |
| `ANALYST_TICKERS` | empty | Optional `TICKER:MIC,TICKER:MIC` allowlist for rollout; empty means every registered company. |
| `KERNEL_DEADLINE` | `50ms` | How long a kernel may run on a release. |
| `NOTES_TOPIC` | `analyst.notes` | Kafka topic of `ReportNote` events (consumer group `analyst`). |
| `SPECIAL_EVENTS_TOPIC` | `special.events` | Kafka topic report releases arrive on (consumer group `analyst-releases`, starting from the latest offset). |
| `DECISIONS_TOPIC` | `analyst.decisions` | Kafka topic the decisions (`AnalystDecision`) are produced on, keyed `ticker:mic`. |
| `MAX_RELEASE_AGE` | `15m` | A release published longer ago than this when it is consumed is recorded as `stale` and never traded. |
| `HOLD` | `30m` | How long a simulated trade is held, in trading minutes (above zero, at most 24h). |
| `OBS_HORIZONS` | `1m,5m,15m,30m,60m,close` | When after entry the bid is observed; ascending durations, `close` (the session cutoff) only last. |
| `CLOSE_BUFFER` | `5m` | Continuous trading ends this long before the close (the closing auction). |
| `ENTRY_MAX_WAIT` | `5m` | How long the entry quote is retried. |
| `QUOTE_CONCURRENCY` | `8` | The most quote requests in flight at once. |
| `SAXO_QUOTES_URL` | `http://saxo-quotes.chronicle.svc.cluster.local:8080` | `saxo-stream`'s internal quote endpoint; only `GET /quote` is ever called. |
| `TRADES_TOPIC` | `analyst.trades` | Where simulated trades are produced. |
| `METRICS_PORT` | `9464` | `/metrics` port. |

## How the scheduler works

Every tick, and right after a note is stored, one pass:

1. **Reads the schedule.** The winning scheduled reports of signal's `special_events`:
   `status = 'scheduled' AND superseded_by IS NULL AND source IN ('mfn','mfn_release')
   AND event_type IN ('quarterly_report','annual_report')`, looking ahead
   `PREP_LEAD + 7 days`. This is how signal marks the winner of the two calendar sources
   (`signal/src/calendar_sources.rs`, `docs/guides/nordic-companies.md`).
2. **Places each report in time.** With a time of day, `expected_at` is signal's
   `occurred_at` (the Stockholm-local time as an instant). Without one it is the start of
   `expected_date` in the exchange's own time zone (`exchanges.timezone`), the earliest the
   report can come. All instants are `timestamptz`/UTC; one `Clock` provides "now".
3. **Follows changes.** A target is matched to its report by the `special_events` row id,
   or, when the other calendar source starts to win and the row id changes, by ticker,
   exchange, kind and fiscal period (the period must be known). Any other report is a new
   target. A changed date or time updates the target and its `prep_due_at`
   (`expected_at − PREP_LEAD`). A report that left the schedule cancels a `waiting` or
   `preparing` target.
4. **Prepares again when it matters.** A `ready` target is prepared again (its old kernel
   is superseded only when the new one is accepted) when a note for its company and period
   (or with no period) arrived after the accepted kernel was created, or when its date moved
   later by more than `PREP_LEAD` past the date the kernel was prepared for. A date that
   moves earlier keeps the kernel. A `failed` target gets one more attempt when a new note
   arrives.
5. **Starts what is due.** A `waiting` target is due when `now ≥ prep_due_at` and
   `expected_at − now ≥ PREP_MIN_LEAD`; at most `PREP_MAX_CONCURRENT` run at once, the
   soonest report first. A `PrepLauncher` starts each, with the notes that apply to the report; the service uses
   `NoopLauncher`, which starts nothing.

## Notes

`nexus expect` produces a `ReportNote` event (`model/proto/report_note.proto`) on
`NOTES_TOPIC`; `analyst` consumes it in group `analyst`. Each note is stored once by its
`note_id` (a redelivery is a duplicate, not a second row), and the offset is committed only
after the note was handled, so a database outage holds the partition instead of losing a note.
A note is rejected (counted in `analyst_notes_total`, never stored) when the company is not
registered, the text is over 4 KiB, or the event is not valid. A note without a fiscal period
applies to the company's next report after it arrived. Storing a note wakes the scheduler.

## Database

`report_targets`, `kernels`, `prep_runs` and `notes` (`migrations/`). The database
enforces at most one `accepted` kernel per target, one live target per report, and that
kernels are never deleted: a kernel becomes `spent` once its report is decided. Queries are
plain `sqlx::query` / `query_as`.

## Tests

`cargo test -p analyst` needs a Postgres reachable through `DATABASE_URL` (as `#[sqlx::test]`
does elsewhere in the workspace). The scheduler and notes tests run signal's real migrations in a second
schema of the test database, so they read the real `special_events` and `exchanges` tables.
Never point `DATABASE_URL` at a shared database: each test creates and drops its own.

## Metrics

See `docs/guides/metrics.md` ("analyst kernel runtime", "analyst scheduler and notes").

## Decisions (releases)

Every report release the analyst acts on ends in one `decisions` row (`migrations/20261113000000_create_decisions.sql`), keyed by the release's MFN `news_id`:

| `decision` | Meaning | Target becomes | Kernel |
|---|---|---|---|
| `buy`, `no_buy`, `abstain` | the kernel's verdict | `decided` | `spent` |
| `kernel_error` | the kernel trapped, timed out or returned a bad output | `decided` | `spent` |
| `stale` | the release was older than `MAX_RELEASE_AGE`; never traded | `skipped` | `spent` |
| `skipped_no_kernel` | no prepared kernel was ready (or no target at all) | `skipped` | none |
| `twin_ignored` | another language version of the same report already decided | unchanged | unchanged |

`Store::record_decision` writes the row and applies those effects in one transaction. It is idempotent
(`ON CONFLICT (news_id) DO NOTHING`: a redelivery returns `Recorded::Duplicate` and changes nothing), and it
locks the target row first, so two language versions racing each other still give one decision per report: the
second is stored as `twin_ignored`.

**Matching a release to its target** (`matching::pick_target`, `Store::match_target`), among the company's
targets of the same kind that are not cancelled: first the same fiscal period (both set and equal); otherwise
the target whose scheduled date is nearest to the release's Stockholm date, within 5 days, skipping targets
whose period differs when both are set. Targets that are not yet settled are preferred to decided or skipped
ones; decided and skipped targets are still candidates, so that a twin finds the target already decided.
`Store::earlier_decision` finds the decision of an earlier language version: for the same target, or, for a
report with no target, for the same company published within 10 minutes.

`AnalystDecision` (`model/proto/analyst_decision.proto`) mirrors the row on `analyst.decisions`.

## The release path

`ReleaseProcessor::process` (`release.rs`) decides on one release; `run_release_consumer` (`releases.rs`)
feeds it from `special.events`.

**Which events.** Only a released `quarterly_report` or `annual_report` from `source = "mfn"` that is not a
correction and has text. Everything else is counted as `analyst_release_events_total{outcome="ignored_<reason>"}`
(`event_type`, `source`, `correction`, `no_text`, `no_news_id`, `invalid`, `undecodable`) and does nothing else.
The consumer starts from the **latest** offset on its first start: the poll fallback replays history, and replayed
reports must not open trades.

**Per release**, in its own task (one company's slow database write never holds another's):

1. a redelivery (the `news_id` already has a decision) stops here;
2. match the target (`matching.rs`) and find out whether this is a twin: a target that is already `decided` or
   `skipped`, or a claim another version of the report holds in memory (`claims.rs`), which is what keeps two
   language versions arriving at the same moment from both being traded;
3. a twin is `twin_ignored`; a release published more than `MAX_RELEASE_AGE` ago is `stale`; a release whose target
   has no ready, loaded kernel (or no target) is `skipped_no_kernel`; otherwise the kernel runs on the blocking
   pool, giving `buy`, `no_buy`, `abstain`, or `kernel_error` with the error kind;
4. **hand off** `buy`, `no_buy`, `abstain` and `skipped_no_kernel` (marked as a baseline) to the trade scheduler's
   in-memory channel (`TradeHandOff`, a no-op until the next task), before anything is written;
5. write the `decisions` row (`Store::record_decision`), retrying with backoff: a database that is unreachable is
   retried until it comes back, any other failure 5 times, after which the decision is logged in full;
6. emit `AnalystDecision` on `analyst.decisions`, retrying until it is accepted;
7. commit the Kafka offset, only once everything before it is done (`offsets.rs`): releases finish out of order.

The kernels are loaded and warmed ahead of the release (`KernelRegistry`): at startup `preload` loads every accepted
kernel of a report that is not yet decided; acceptance inserts a new one. A decided report's kernel instance is dropped
from memory an hour later (twins may still arrive).

A redelivery does **not** emit the event again: the decision row is the record, and a crash between the write and the
emit leaves the event unsent (the row can be replayed from the table).

**Tracing.** Every release that passes the filter is a `process special.events` span that continues the trace
from `nordic-live` (parent from the message's `traceparent`), with a child span per step above; the trace id is
stored in `decisions.trace_id`, set on `AnalystDecision.trace_id`, and the `traceparent` goes out on the decision
event and, as an OpenTelemetry context, with the `TradeSignal`. Spans: `docs/guides/tracing.md`.

Metrics: `analyst_release_to_decision_seconds` (consumed to handed on), `analyst_publish_to_decision_seconds` (MFN
publish to handed on; includes MFN's and `nordic-live`'s delay, for dashboards), `analyst_release_events_total{outcome}`,
`analyst_decisions_total{decision}`, `analyst_decision_retries_total{step}`. One `info` line per decision with ticker,
period, decision, reason and the three times is the operator's audit line.

## Trading sessions (simulated trades)

`trading.rs` plans the instants of a simulated trade from the exchange calendar, `model::session::Sessions` (the
loader of `exchanges` and `trading_holidays` was moved there from `chronicle`'s Slack code, so `nordic-live` and
`analyst` share one copy; `chronicle` keeps a thin wrapper that adds the Slack label). Continuous trading is
`[open, close - CLOSE_BUFFER)` in the exchange's own time zone, with the half-day close on a half day. `plan_entry`
gives `Now` inside it, the next open otherwise (the same day before the open, else the next trading day), and
`CalendarUnknown` for an unknown MIC or a year the holiday table does not cover: never guessed. `exit_target` is
`HOLD` after entry but never past the cutoff; `observation_slots` gives each horizon's instant, `beyond_close` past
the cutoff, and `close` is the cutoff.

## Simulated trades: storage, quotes, result

Migration `20261114000000_create_sim_trades.sql`: `sim_trades` (one row per traded decision, `decision_id` unique,
`shadow = decision <> 'buy'`, statuses `pending_entry | open | closed | no_entry_quote | no_exit_quote |
exit_missed | calendar_unknown | currency_mismatch | invalid_entry`, `version`, `trace_id`, `release_traceparent`,
`trade_trace_id`) and `sim_observations` (one row per trade and horizon: `1m`, ..., `close`, `hold`; `ok | missed |
beyond_close | no_quote`). Prices are `NUMERIC(20,6)`, `pnl_pct` `NUMERIC(20,4)`; **per share only: no fees, no
position size, no overnight holding**. Every state change bumps `version` (`Store::record_entry`, `record_observation`,
`record_exit`, `set_trade_status`), so a consumer of `analyst.trades` keeps the newest `AnalystTrade`
(`model/proto/analyst_trade.proto`: the full state and every observation so far, prices as decimal strings, times as
unix milliseconds). `create_trade` is idempotent per decision.

`quotes.rs` reads `GET /quote?ticker=&mic=` on `SAXO_QUOTES_URL` and nothing else (a test asserts the path; there is no
order endpoint). An entry quote is accepted when `ask_type = "Tradable"`, `delayed_minutes = 0` (a missing value is
not accepted) and `ask > 0`; an exit quote the same with the bid. A 503 gives the `retry_after_ms` it asked for.
`pnl.rs`: `pnl_per_share = exit_bid - entry_ask`, `pnl_pct = (exit_bid - entry_ask) / entry_ask * 100` rounded to 4
decimals (away from zero), exact `Decimal` arithmetic, no result for different currencies or a zero entry price.
Metrics: `analyst_entry_delay_seconds`, `analyst_quote_lateness_seconds`, `analyst_sim_trades_total{status,shadow}`,
`analyst_quotes_total{outcome}` (described now, recorded by the trade runner).

## The trade runner

`runner.rs` (`TradeRunner`, a `TradeHandOff`) simulates the trade of every decision the release path hands over
(`buy`, and the shadow decisions `no_buy`, `abstain`, `skipped_no_kernel`; never `stale`, `kernel_error`,
`twin_ignored`). The hand-off spawns one task per trade and returns; the task:

1. plans the entry (`trading.rs`). **Inside continuous trading the first `GET /quote` leaves before anything else:** no
   database read or write comes first (the task's target is 5 ms; the test asserts the request is sent at the
   decision's instant). Otherwise the trade row is created first (`pending_entry`), so a restart finds it, and the task
   sleeps to the session open;
2. the trade row needs the decision's row id, which is only known after the release path's database write, so the
   signal carries a `DecisionHandle` that resolves to it (`Persisted`). A decision that was stored as something else
   (it lost a twin race) or never stored gets no trade;
3. requests quotes at `entry target`, retrying every second (the `retry_after_ms` of a 503 instead) for
   `ENTRY_MAX_WAIT`, and accepts the first tradable, undelayed, positive ask: `entry_at` is the instant its answer
   arrived, `release_to_entry_ms = entry_at - published_at`. No acceptable quote: `no_entry_quote`;
4. sets `exit_target = min(entry_at + HOLD, cutoff)`, takes one bid per horizon (a horizon past the cutoff is
   `beyond_close`, `close` is the cutoff) and the exit bid at the exit target (retried every second for up to 30 s,
   never past the cutoff; stored as the observation `hold`). The result is per share, `closed`, or
   `currency_mismatch` / `no_exit_quote`;
5. emits the full `AnalystTrade` on `TRADES_TOPIC` after every state change, without waiting for the broker (up to 10
   attempts, 500 ms apart; the `version` lets a consumer keep the newest).

Timers: every quote is slept to with `tokio::time::sleep_until`, on a deadline computed from the wall clock when it is
scheduled; no loop polls whether it is time. Requests of different trades run concurrently, at most
`QUOTE_CONCURRENCY` in flight; a request that waited for a permit is flagged `late` like any fetch more than 2 s after
its target.

**Restart:** at startup `resume` loads every `pending_entry` and `open` trade. A quote whose target passed more than 2
s ago is `missed` (not fetched: a late price is a different price); a missed exit makes the trade `exit_missed`; a
pending entry whose target passed more than 2 s ago is `no_entry_quote`. Observations after the exit of a `closed`
trade are not rescheduled (only open trades are loaded), so a restart after the exit loses the remaining horizons.

The calendars (`SessionsCache`) are loaded from signal's database at startup and reloaded hourly; a failed reload
keeps the last one, and without any every trade is `calendar_unknown`.
