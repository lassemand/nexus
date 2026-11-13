# Tracing

## Instrumenting a service

Every traced service initialises logging and tracing with the `telemetry` crate and keeps
the guard until it exits:

```rust
let _tracing = telemetry::trace::init("analyst")?;
// or, for a binary with its own --log-format flag:
let _tracing = telemetry::trace::init_with("nordic-live", telemetry::trace::LogFormat::Text)?;
```

This replaces `tracing_subscriber::fmt::init()`. Spans are still written with `tracing`
(`#[instrument]`, `info_span!`); `tracing-opentelemetry` turns them into OpenTelemetry spans.
`init` installs:

* a fmt layer on stdout, JSON by default (`LOG_FORMAT=text` for plain text), filtered by
  `RUST_LOG` (default `info`). JSON lines inside a span carry top-level `"trace_id"`
  (32 lowercase hex) and `"span_id"` (16 lowercase hex), so Grafana can jump from a log
  line in Loki to its trace; outside any span the fields are absent. The other fields are
  `tracing_subscriber`'s JSON format unchanged (Alloy reads `level`);
* an OpenTelemetry layer, always (so local logs carry trace ids too), filtered separately by
  `OTEL_TRACES_FILTER` (default `info,sqlx=warn,hyper=warn,h2=warn,tower=warn,reqwest=warn`)
  so dependency spans are never exported;
* an OTLP HTTP/protobuf exporter **only** when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Without
  it nothing leaves the process (local runs and tests stay offline);
* the W3C trace-context propagator as the global propagator.

Calling `init` twice returns an error. Dropping the guard flushes the last spans, waiting at
most 5 s.

### Configuration

Standard OpenTelemetry variables, read by the SDK where it supports them:

| Variable | Default | Meaning |
|---|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | unset: no export | Collector base URL, `/v1/traces` is appended. `http://` only: the exporter is built without TLS |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | unset | Full traces URL, used as is |
| `OTEL_SERVICE_NAME` | the name passed to `init` | `service.name` |
| `OTEL_RESOURCE_ATTRIBUTES` | none | `key=value,...`; overrides the defaults below |
| `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` | `parentbased_always_on` | Sampler |
| `OTEL_BSP_MAX_QUEUE_SIZE` | 4096 | Finished spans waiting for export; beyond it spans are dropped |
| `OTEL_BSP_MAX_EXPORT_BATCH_SIZE` | 512 | Spans per export request |
| `OTEL_BSP_SCHEDULE_DELAY` | 1000 ms | Interval between exports |
| `OTEL_BSP_EXPORT_TIMEOUT` | 5000 ms | HTTP timeout of one export request, unless `OTEL_EXPORTER_OTLP_TIMEOUT` is set |
| `OTEL_TRACES_FILTER` | see above | `EnvFilter` directives for exported spans (nexus-specific) |
| `RUST_LOG` | `info` | `EnvFilter` directives for log lines |
| `LOG_FORMAT` | `json` | `text` for human-readable logs |

Resource attributes: `service.name`, `service.version` (workspace version),
`deployment.environment=orbstack`, `host.name` from `HOSTNAME` (the pod name), and the SDK's
`telemetry.sdk.*`.

### Cost and failure behaviour

* Creating and closing a span with 5 attributes costs about 1.2 µs (p50) and 1.8 µs (p99) in a
  release build with the exporter installed (`cargo test -p telemetry --release --
  --ignored --nocapture span_overhead`).
* Export runs on the batch processor's own thread and never blocks the caller. When its queue
  is full, spans are dropped and counted in `telemetry_spans_dropped_total`.
* Export errors (collector down, timeout) are logged as one `warn` per minute at most, with
  the number of failures suppressed since. The exporter retries a failed request 3 times
  (100 ms to 1.6 s backoff) on its own thread before giving up on that batch.
* A collector that is down changes nothing but the missing traces; shutdown waits at most 5 s.

### Conventions

* **Span names:** `<service>.<step>` for internal steps (`analyst.kernel_run`). Kafka uses the
  OpenTelemetry messaging names: `send <topic>` with `otel.kind = "producer"` and
  `process <topic>` with `otel.kind = "consumer"`, with `messaging.system = "kafka"`,
  `messaging.destination.name`, `messaging.kafka.message.key`, `messaging.kafka.offset`,
  `messaging.kafka.partition` where known. A span name that is not a literal goes in
  `otel.name`.
* **Domain attributes:** `nexus.ticker`, `nexus.exchange_mic`, `nexus.event_type`,
  `mfn.news_id`, `nexus.lang`, `nexus.fiscal_period`, `nexus.decision`.
* **Companies are runtime data:** tickers only as attribute values, never in span names.
* **Errors:** declare `otel.status_code` and `otel.status_description` as
  `tracing::field::Empty`, record `"ERROR"` and the message on failure, and emit
  `tracing::error!(error = %e)` (an `error` field, no message) inside the span, which becomes
  an `exception` event with `exception.message`. `tracing-opentelemetry` reads
  `otel.status_description`, not `otel.status_message`.
* **Tasks:** a span that crosses `tokio::spawn` is passed with `.instrument(span)`.

### Kafka (feature `kafka`)

```rust
let headers = telemetry::kafka::inject_current(OwnedHeaders::new());     // producer
let span = tracing::info_span!("process special.events", otel.kind = "consumer");
if let Some(cx) = telemetry::kafka::extract(&message) {                  // consumer
    let _ = span.set_parent(cx);
}
```

`inject` / `inject_current` add `traceparent` (and `tracestate` when not empty); `extract`
returns `None` for a missing, malformed or non-UTF-8 header and never fails the message.

### Spans that start in the past

`telemetry::trace::span_from(name, start, parent, attributes)` returns a `tracing::Span` whose
OpenTelemetry span starts at `start` (the root of a release starts at MFN's publish instant),
and whose children are the spans created inside it. `tracing` spans always start "now", and
`tracing-opentelemetry` 0.34 keeps its span builder private, so the span carries a
`nexus.start_time_unix_nano` field that the tracer installed by `init` turns into the start
time. A start in the future (clock skew) is clamped to now and flagged with
`nexus.clock_skew_ms`.
