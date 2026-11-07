# Metrics

Every service records metrics with the `metrics` facade (`metrics::counter!`,
`gauge!`, `histogram!`). The `telemetry` crate installs the exporter. Prometheus
(kube-prometheus-stack) scrapes long-running services and a Pushgateway receives
the metrics of short-lived CronJobs.

## Long-running services (scrape)

Call once at startup, before recording anything:

```rust
telemetry::serve("signal", 9464).await?;
```

* Serves `GET /metrics` on `0.0.0.0:<port>` and sets `nexus_build_info{service, version} = 1`.
* Errors (never panics) if a global recorder is already installed or the port is taken.
* Port `9464`, container port named `metrics`. Never expose it through a Service that
  faces the internet; Prometheus reaches the pod directly.
* Scrape it with a `PodMonitor` in the service's own chart (`jobLabel: app` makes the `job`
  label the pod's `app` label). Prometheus selects PodMonitors, ServiceMonitors, Probes and
  ScrapeConfigs in every namespace; do not add `additionalScrapeConfigs`. Example:
  `infra/charts/chronicle/chronicle/templates/podmonitor-nordic-live.yaml`.

The endpoint is a minimal handler on tokio instead of the exporter's `http-listener`
feature, which would add hyper's server stack and `ipnet` to every service.

## CronJobs (push)

```rust
let batch = telemetry::batch("market")?;
let result = run().await;
batch.finish(result.as_ref().map(|_| ())).await;
result
```

`finish` sets `nexus_job_last_run_timestamp_seconds`, `nexus_job_last_run_success` (1/0),
`nexus_job_duration_seconds` and, on success only, `nexus_job_last_success_timestamp_seconds`
(all `{job}`), then pushes everything recorded during the run with one HTTP **POST** to
`$PUSHGATEWAY_URL/metrics/job/<job>` (10 s timeout, one immediate retry, only for a transport error, 408, 429 or 5xx; other 4xx fail at once).

* POST, never PUT: PUT replaces every metric in the group, which would erase
  `last_success_timestamp` after a failed run. The exporter's own `push-gateway` feature
  uses PUT and pushes periodically, so it is not used.
* No `PUSHGATEWAY_URL`: one `warn!`, no push. A failed push is logged at `error` and never
  changes the job's exit code.
* Set `PUSHGATEWAY_URL=http://pushgateway.nexus.svc.cluster.local:9091` on the CronJob.

**Pushed values describe the last run.** Read them as gauges; never `rate()` or `increase()`
them. A pushed series keeps its last value until the next run replaces it.

### saxo-stream

Keeps its own hand-written `/metrics` route (the health server is shared with `/tokens`), and
sets `nexus_build_info{service="saxo-stream"}` by hand. Series: the original four
(`saxo_refresh_token_seconds_remaining`, `saxo_refresh_token_failures_total`,
`saxo_daily_bars_last_success_timestamp`, `saxo_daily_bars_failures_total`) plus
`saxo_token_refreshes_total{outcome}`, `saxo_daily_bars_sweep_duration_seconds` (histogram),
`saxo_daily_bars_sweep_instruments`, `saxo_daily_bars_published_total{exchange_mic}` (recent days are
re-published every sweep), `saxo_daily_bars_backfills_total{outcome}`,
`saxo_daily_bars_latest_date_timestamp_seconds{ticker,exchange_mic}` (midnight UTC of the newest bar),
`saxo_api_requests_total{endpoint,status_class}`, `saxo_api_request_duration_seconds{endpoint}` and
`saxo_api_rate_limited_total`. `endpoint` is `token`, `chart` or `instruments`; `status_class` is
`2xx`, `4xx`, `5xx` or `error` (no response). The HTTP series are recorded in `alpha::saxo`.

A deregistered ticker keeps no stale series: on every sweep, instruments that had a freshness gauge
and are no longer registered are set to 0 (the `metrics` crate cannot delete a series) and
`SaxoDailyBarsStaleInstrument` ignores 0. They stay at 0 until the pod restarts.
Alerts: `SaxoDailyBarsStaleInstrument` (latest bar > 4 days, weekdays from 18:00 UTC),
`SaxoApiRateLimited` (> 5 429s in 30 minutes).

## Conventions

**Names:** `<service>_<subject>_<unit>`. Counters end in `_total`, durations are histograms
in `_seconds`, timestamps are gauges ending in `_timestamp_seconds`.

**Labels:** bounded values only, such as `topic`, `event_type`, `outcome`, `job`,
`exchange_mic`, `tool`, `reason`. Never a news id, URL, error message or user input.
`ticker` is allowed only on per-company freshness gauges, which are bounded by the
registered companies.

**Dashboards:** one per service as
`infra/charts/nexus/grafana/templates/dashboard-<service>.yaml`, following the existing
ConfigMaps (folder "Nexus", Prometheus datasource uid `nexus-prometheus`). The generic
`Nexus Overview` dashboard already lists every scraped service (`up`, `nexus_build_info`),
pod restarts, every pushed job and Kafka consumer lag.

**Alerts:** added to `infra/charts/nexus/prometheus/rules/nexus.rules.yml`, each with a test
in `nexus.rules.test.yml`. Generic alerts: `NexusScrapeTargetDown` (any target in the app
namespaces down for 5 m) and `NexusJobLastRunFailed`.

## signal report store

A Nordic report event that `signal` cannot store is handled by the kind of error.
A transient one (connection lost, pool exhausted, serialization failure) is retried
with backoff (1 s doubling to 60 s) and counts `signal_messages_total{outcome="db_error_retried"}`
and `signal_store_retries_total{topic}`; `signal_store_blocked` is 1 while it retries.
Any other error (a value the schema rejects) would block the partition forever, so the
report is logged at `error`, counted in `signal_report_db_errors_total` and its offset
committed (`outcome="db_error_committed"`). The producer does not resend it, so the
alert `SignalReportRejected` (`increase(signal_report_db_errors_total[15m]) > 0`,
warning) means the report was dropped and must be replayed from MFN.

## Cluster pieces

| Piece | Where |
|---|---|
| Pushgateway (in-cluster only, persistent) | `infra/charts/nexus/pushgateway`, `pushgateway.nexus.svc.cluster.local:9091` |
| Kafka consumer lag (`kafka_consumergroup_lag{consumergroup,topic,partition}`) | Strimzi `kafkaExporter` on the Kafka CR, PodMonitor `kafka-exporter` |
| saxo-stream | PodMonitor in `infra/charts/chronicle/chronicle/templates/`, dashboard `Saxo` (uid `saxo-token-health`), see below |
| nordic-live | PodMonitor in `infra/charts/chronicle/chronicle/templates/`, dashboard `Nordic Live`, alerts `nexus.nordic-live` (`nordic_live_*`, `nordic_slack_posts_total`) |
| signal | PodMonitor in `infra/charts/signal/signal/templates/`, dashboard `Signal`, alerts `nexus.signal` |
| conductor | PodMonitor in `infra/charts/conductor/conductor/templates/`, dashboard `Conductor`, alerts `nexus.conductor` |
