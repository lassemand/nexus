# Tracing

Traces are stored by Grafana Tempo (single binary, namespace `nexus`, chart `infra/charts/nexus/tempo/`, wrapping `grafana/tempo`). Services export spans over OTLP to Grafana Alloy (chart `infra/charts/nexus/alloy/`), which forwards them to Tempo. Grafana has Tempo as the provisioned datasource **Tempo** (`uid: nexus-tempo`). The first part of this guide is the backend and Grafana; the second part, "Instrumenting a service", is the Rust side.

```
service ──OTLP/HTTP──► alloy:4318 ──OTLP/gRPC──► tempo:4317 ──► B2 bucket nexus-tempo
                                                    └─ metrics generator ──remote write──► Prometheus
```

Everything here is ClusterIP: neither Tempo nor Alloy's OTLP ports are exposed outside the cluster.

## Prerequisites (by hand, once)

Tempo starts without these but cannot flush blocks to storage.

1. Create a **private** B2 bucket `nexus-tempo` in region **EU Central (`eu-central-003`)**, with default encryption (SSE-B2) on.
2. **Lifecycle rule: "Keep only the last version of the file"** (`daysFromHidingToDeleting = 1`). **This is critical.** B2 keeps every old version by default, so the blocks Tempo's compactor deletes for retention would stay stored and billed for ever.
3. Create an **application key** restricted to bucket `nexus-tempo`, with read and write. Note the `keyID` and `applicationKey`.
4. Store it in Vault from the Mac mini:
   `vault kv put secret/nexus/tempo AWS_ACCESS_KEY_ID=<keyID> AWS_SECRET_ACCESS_KEY=<applicationKey>`
   **Never paste keys into a PR, an issue or chat.**

The `tempo-s3` ExternalSecret creates the Kubernetes secret `tempo-s3`, injected as env into the Tempo pod. The Tempo config holds only `${AWS_ACCESS_KEY_ID}` / `${AWS_SECRET_ACCESS_KEY}` placeholders (`-config.expand-env=true`). The Vault policy `nexus-read` grants `secret/data/nexus/*`, which covers `nexus/tempo`.

## Where traces are kept, and for how long

- Blocks are stored in the B2 bucket `nexus-tempo` (S3 API, virtual-hosted style).
- Retention is **30 days** (compactor `block_retention: 720h`). The B2 lifecycle rule above makes B2 purge deleted blocks.
- A 2Gi `local-path` PVC holds the WAL (`/var/tempo/wal`), blocks not yet flushed, and the metrics generator's working directories, so a pod restart loses no traces. `local-path` does not enforce the size, so local data is kept small: the ingester cuts a block at 30 minutes or 100 MB (whichever first) and flushes it to B2, and completed blocks are removed locally after 15 minutes. A normal day is a few MB.
- Ingestion limit: `max_bytes_per_trace` is 10 MB (a release trace is ~20-40 spans, a few tens of KB).
- Resources: requests 100m / 256Mi, memory limit 1Gi, the chart's memory ballast is switched off so the limit is not eaten by it.

**Deleting the bucket deletes the trace history.** Deleting the PVC only loses spans not yet flushed (up to ~30 minutes).

## Late spans and clock skew

A span may arrive hours after the rest of its trace (a report released before the market opens, whose trade entry span ends at the open). Tempo's defaults allow this and none are overridden: there is no limit on how late a span may be, and the ingester's `trace_idle_period` does not reject anything, it only decides when a block is cut. Late spans end up in a later block; a query by trace ID reads every block holding a piece of the trace and returns the merged whole. Only a trace over `max_bytes_per_trace` loses spans.

Span times come from the sender. A span that starts at MFN's publish instant uses **MFN's clock**, the rest use the cluster's, so a few hundred milliseconds of skew between them is normal (the instrumentation clamps a start in the future and flags it with `nexus.clock_skew_ms`).

## How a service sends traces

Set two environment variables on the workload, nothing else is needed:

```
OTEL_EXPORTER_OTLP_ENDPOINT=http://alloy.nexus.svc.cluster.local:4318
OTEL_SERVICE_NAME=<binary>
```

The endpoint is the OTLP/HTTP port of Alloy's Service `alloy` (namespace `nexus`; the Service name is the chart's, checked with `helm template`). Alloy is a DaemonSet and the Service may hand a pod's spans to Alloy on any node; with one node that is the same one. Alloy adds `k8s.namespace.name`, `k8s.pod.name`, `k8s.deployment.name` and `k8s.node.name` (it has extra read-only RBAC for ReplicaSets and nodes in `infra/charts/nexus/alloy/templates/otel-rbac.yaml`).

Sending is asynchronous and off every hot path. If Alloy or Tempo is down, Alloy queues up to 1000 batches and then drops, and services drop spans from their own full queue; nothing waits.

## Browsing

Grafana → Explore → datasource **Tempo**.

- By trace ID: query type "TraceQL" or "Search" with the ID pasted in.
- TraceQL examples:

```traceql
{resource.service.name="analyst" && name="process special.events"}
{span.nexus.ticker="<TICKER>"}
{resource.service.name="nordic-live" && duration > 2s}
{status=error}
```

  Slowest traces: `{duration > 5s}`, or sort the Search results by duration.
- **Trace → logs:** the "Logs" button on a span opens Loki with `{namespace=~".+"} | json | trace_id="<id>"`, ±5 minutes around the span.
- **Logs → trace:** in Loki, a line with `"trace_id":"<32 hex>"` has a "View trace" link (derived field `trace_id` on the Loki datasource).
- **Trace → metrics:** on a span, "Span rate" and "Span duration p95" query the span-metrics series in Prometheus.
- **Service graph:** the "Service Graph" tab of the Tempo datasource (backed by `traces_service_graph_*` series from Prometheus). It fills a few minutes after the first traces.

## Metrics generated from traces

Tempo's metrics generator (processors `service-graphs` and `span-metrics`) remote-writes to Prometheus (`/api/v1/write`, which needs `prometheusSpec.enableRemoteWriteReceiver: true`). The samples land in the same TSDB blocks, so the Thanos sidecar uploads them like scraped ones. Span-metrics dimensions are only `service`, `span_name`, `span_kind` and `status_code`. **Never add ticker, news id or trace id as a dimension**: each value would create new series. Tempo's own metrics are scraped through its ServiceMonitor.

## Verifying

Send a test span from inside the cluster (OTLP/JSON to Alloy):

```bash
kubectl --context orbstack -n nexus run otlp-test --rm -i --restart=Never --image=curlimages/curl -- \
  sh -c 'T=$(date +%s)000000000; E=$(( $(date +%s) + 1 ))000000000;
  curl -sS -X POST http://alloy.nexus.svc.cluster.local:4318/v1/traces -H "Content-Type: application/json" -d "{\"resourceSpans\":[{\"resource\":{\"attributes\":[{\"key\":\"service.name\",\"value\":{\"stringValue\":\"otlp-test\"}}]},\"scopeSpans\":[{\"spans\":[{\"traceId\":\"5b8efff798038103d269b633813fc60c\",\"spanId\":\"eee19b7ec3c1b174\",\"name\":\"test span\",\"kind\":2,\"startTimeUnixNano\":\"$T\",\"endTimeUnixNano\":\"$E\"}]}]}]}"'
```

It answers `{"partialSuccess":{}}`. After a few seconds the trace `5b8efff798038103d269b633813fc60c` is found in Explore → Tempo by ID, and `{resource.service.name="otlp-test"}` finds it by search.

- Blocks reach B2: after the ingester has cut a block (up to 30 minutes), `tempo_ingester_blocks_flushed_total` is above 0, and the bucket has `single-tenant/<block id>/` prefixes. The compactor runs: `kubectl -n nexus logs statefulset/tempo | grep -i compact` shows its cycles once there are blocks to combine.
- Service graph: the `otlp-test` service appears under metrics `traces_spanmetrics_calls_total{service="otlp-test"}` within a minute or two; the graph itself needs client/server span pairs.

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

`span_between(name, start, end, parent, attributes)` is the same for a span that also ends in
the past; the end time is applied by a span processor before export, because `tracing` ends
spans when they close. With the cargo feature `testing`, `telemetry::trace::testing::Harness`
gives tests a subscriber over an in-memory exporter.

## nordic-live

`nordic-live` starts one trace per item of a registered company (items of other companies get no
span). Chart value `nordicLive.otlpEndpoint` sets `OTEL_EXPORTER_OTLP_ENDPOINT` (empty disables
export); `OTEL_SERVICE_NAME` is `nordic-live`.

| Span | Starts / ends | Notes |
|---|---|---|
| `nordic_live.release` (root) | MFN's publish instant, until the last step of the item is done | `nexus.ticker`, `nexus.exchange_mic`, `mfn.news_id`, `nexus.event_type`, `nexus.lang`, `nexus.fiscal_period`, `nordic_live.path` (`socket` or `catch_up`), `nordic_live.publish_to_receive_ms` (negative: clock skew), `nordic_live.disposition` (`new`, `already_ingested`) |
| `mfn.delivery` | publish instant to `received_at` | `nexus.clock=external`: both ends come from MFN's clock and ours; zero length with `nexus.clock_skew_ms` when MFN is ahead |
| `nordic_live.parse` | parsing and classifying the message | a child of the **first relevant item's** trace only, with `nordic_live.items_in_message`; none when the message has no relevant item |
| `nordic_live.ingested_check` | the lookup in signal's database | `nordic_live.disposition`; not made without `SIGNAL_DATABASE_URL` |
| `nordic_live.emit_queue` | announcements: time in the emit queue | |
| `send special.events` | producer span, until the broker acknowledged | `messaging.*`, partition and offset when acknowledged; ERROR on failure, also on the root |
| `nordic_live.slack` | the Slack post | `nordic_live.slack.kind`, `nordic_live.slack.outcome`; runs on the Slack worker, after publishing |

An item the poll already stored ends its trace at the check (`already_ingested`). `KafkaSink`
adds `traceparent` (and `tracestate`) to every message sent inside a span; the poll binaries send
without a span and add no header. Consumers that do not read headers (signal) are unaffected.

## analyst

The release path continues the release's trace from `special.events` and carries it on to
`analyst.decisions`. Only a release that passes the filter gets a span; the other events on the topic
cost no traces. `telemetry::trace::init("analyst")` is installed at startup; `OTEL_EXPORTER_OTLP_ENDPOINT`
unset or unreachable changes nothing but the missing traces (traced p99 with a dead collector:
about 1 ms more than untraced, see the PR of NEX-254).

| Span | Notes |
|---|---|
| `process special.events` (consumer) | parent is the message's `traceparent`; without one it is a root and `nexus.trace_parent_missing=true`. `messaging.*`, `nexus.ticker`, `nexus.exchange_mic`, `mfn.news_id`, `nexus.lang`, `nexus.fiscal_period`, and `nexus.decision` once decided |
| `analyst.match_target` | `analyst.target_id`, `analyst.match` |
| `analyst.kernel_run` | `analyst.kernel_id`, `analyst.kernel_micros`, `nexus.decision`; ERROR with the error kind when the kernel fails |
| `analyst.hand_off` | the OpenTelemetry context of this span goes with the `TradeSignal`, so the simulated trade joins the trace |
| `analyst.decision_write` | `analyst.write_attempts`; ERROR when the write gives up |
| `send analyst.decisions` (producer) | `traceparent` is added to the Kafka message |
| `analyst.commit_offset` | the offset commit after the release is done |

`decisions.trace_id` and `AnalystDecision.trace_id` hold the trace id, so a decision can be found in Tempo from
the database, and the other way round.
