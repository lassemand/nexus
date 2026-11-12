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
  runs and notes, applied by the service at startup.

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
