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
