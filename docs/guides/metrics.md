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
`$PUSHGATEWAY_URL/metrics/job/<job>` (10 s timeout, one retry).

* POST, never PUT: PUT replaces every metric in the group, which would erase
  `last_success_timestamp` after a failed run. The exporter's own `push-gateway` feature
  uses PUT and pushes periodically, so it is not used.
* No `PUSHGATEWAY_URL`: one `warn!`, no push. A failed push is logged at `error` and never
  changes the job's exit code.
* Set `PUSHGATEWAY_URL=http://pushgateway.nexus.svc.cluster.local:9091` on the CronJob.

**Pushed values describe the last run.** Read them as gauges; never `rate()` or `increase()`
them. A pushed series keeps its last value until the next run replaces it.

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

## Cluster pieces

| Piece | Where |
|---|---|
| Pushgateway (in-cluster only, persistent) | `infra/charts/nexus/pushgateway`, `pushgateway.nexus.svc.cluster.local:9091` |
| Kafka consumer lag (`kafka_consumergroup_lag{consumergroup,topic,partition}`) | Strimzi `kafkaExporter` on the Kafka CR, PodMonitor `kafka-exporter` |
| saxo-stream, nordic-live | PodMonitors in `infra/charts/chronicle/chronicle/templates/` |
