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
