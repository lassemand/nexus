# Loki (central logs)

Loki runs in `SingleBinary` mode in namespace `nexus` (chart `infra/charts/nexus/loki/`, wrapping `grafana/loki`). Grafana has it as the provisioned datasource **Loki** (`uid: nexus-loki`). Collecting pod logs (Grafana Alloy) and alerting on the pipeline are separate tasks.

## Prerequisites (by hand, once)

Loki starts without these but cannot flush chunks to storage.

1. Create a Backblaze account and a **private** B2 bucket `nexus-loki` in region **EU Central (`eu-central-003`)**, with default encryption (SSE-B2) on.
2. **Lifecycle rule: "Keep only the last version of the file"** (`daysFromHidingToDeleting = 1`). **This is critical.** B2 keeps every old version by default, so the objects Loki's compactor deletes for retention would stay stored and billed for ever.
3. Create an **application key** restricted to bucket `nexus-loki`, with read and write. Note the `keyID` and `applicationKey`.
4. Store it in Vault from the Mac mini:
   `vault kv put secret/nexus/loki AWS_ACCESS_KEY_ID=<keyID> AWS_SECRET_ACCESS_KEY=<applicationKey>`
   **Never paste keys into a PR, an issue or chat.**

The `loki-s3` ExternalSecret then creates the Kubernetes secret `loki-s3`, which is injected as env into the Loki pod. The Loki config contains only `${AWS_ACCESS_KEY_ID}` / `${AWS_SECRET_ACCESS_KEY}` placeholders (`-config.expand-env=true`). The Vault policy `nexus-read` grants `secret/data/nexus/*`, which covers `nexus/loki`.

## Where logs are kept, and for how long

- Chunks and the TSDB index are stored in the B2 bucket `nexus-loki` (S3 API, virtual-hosted style addressing).
- A 10Gi `local-path` PVC holds the WAL (`/var/loki/wal`), the TSDB shipper directories and the compactor working directory, so a pod restart loses no unflushed logs.
- Retention is **30 days** (`retention_period: 720h`), enforced by the compactor deleting expired chunks. The B2 lifecycle rule above makes B2 purge them.
- Ingestion limits: 8 MB/s rate, 16 MB burst, lines over 256KB are truncated, samples older than 168h are rejected.
- The chunks and results caches are disabled (their defaults reserve several GB).

**Deleting the bucket deletes the log history.** Deleting the PVC only loses logs not yet flushed (up to ~2h).

## Querying

Grafana → Explore → datasource **Loki**. Example:

```logql
{namespace="chronicle"} |= "error"
```

Loki's own metrics are scraped through its ServiceMonitor (`loki` in `nexus`, port `http-metrics`).

## What is collected

Grafana Alloy (chart `infra/charts/nexus/alloy/`) runs as a DaemonSet in `nexus` and ships the logs of **every container in every namespace** to Loki. It discovers pods with `discovery.kubernetes`, tails the node's CRI log files under `/var/log/pods` (mounted read-only) with `loki.source.file`, and pushes with `loki.write` (positions and a write-ahead log are kept on the node in `/var/lib/alloy`, so a short Loki or Alloy restart loses nothing).

Labels (and only these): `namespace`, `pod`, `container`, `app` (from `app.kubernetes.io/name`, else `app`), `node`, `cluster="orbstack"`, and `level` (`trace`, `debug`, `info`, `warn`, `error`). `level` comes from a JSON `level` field or from the Rust `tracing` token after the timestamp; lines with no recognisable level have no `level` label. Loki also adds `service_name` itself, derived from `app`.

Processing: the CRI wrapper is stripped, so the stored line is the application's own line. Alloy's and Loki's own `level=debug` lines are dropped (warn and error are kept). Secrets are masked with `***`: `Bearer <token>`, `password=...`, and `access_token`/`refresh_token` values (JSON or `key=value`). This is a safety net, not a licence to log secrets.

### Why labels are restricted

Every distinct label combination is a separate stream. Labels such as pod IDs, `pod-template-hash` or `controller-revision-hash` multiply streams on every rollout, which slows Loki and bloats the index. Put everything else in the line and filter it at query time.

### More queries

```logql
{namespace="chronicle", app="nordic-reports"}
{namespace="signal"} | level="error"
sum by (namespace) (count_over_time({cluster="orbstack"}[1h]))
```

Alloy's metrics (`alloy_*`, `loki_write_*`) are scraped through its ServiceMonitor.

## When an alert fires

The rules are the group `nexus.log-pipeline` in `infra/charts/nexus/prometheus/rules/nexus.rules.yml` (unit tests in `nexus.rules.test.yml`). They route to Slack through the existing Alertmanager.

- **LokiDown** (critical): no Loki target is up for 5 minutes, so logs are not stored. Check `kubectl -n nexus get pods -l app.kubernetes.io/name=loki` and `kubectl -n nexus logs statefulset/loki`.
- **AlloyDown** (critical): no Alloy pod is up for 5 minutes, so logs are being lost. Check `kubectl -n nexus get pods -l app.kubernetes.io/name=alloy` and `kubectl -n nexus logs daemonset/alloy`.
- **LokiObjectStoreErrors** (warning): S3 requests to B2 fail (other than 404) or chunk flushes fail, for 15 minutes. Usually an expired or revoked application key or a B2 outage. Check the key in Vault `secret/nexus/loki`, the `loki-s3` ExternalSecret, and the Loki logs.
- **LokiChunksNotFlushing** (warning): logs arrive but no chunk has been flushed for 3 hours. The WAL is filling the 10Gi PVC. Same first steps as above; check the B2 key and `kubectl -n nexus logs statefulset/loki | grep -i flush`.
- **LokiCompactorNotRunning** (warning): no successful retention run for 3 hours. Expired logs are not deleted, so B2 storage and cost grow. Check `kubectl -n nexus logs statefulset/loki | grep -i compactor`.
- **AlloyLokiWriteFailing** (warning): Alloy retries or drops batches sent to Loki for 10 minutes. Check Alloy's logs and whether Loki is up.
- **LokiRequestRateLimited** (warning): Loki discards lines because of its limits (`rate_limited`, `stream_limit`, `per_stream_rate_limit`). Find the noisy stream in Explore, quieten it, or raise the limit in `infra/charts/nexus/loki/values.yaml`.

Metric names were checked against Loki 3.6.11 and Alloy v1.21.2 run locally. Loki 3.6.11 with the chart's default object-store client has no `loki_objstore_bucket_operation_failures_total`, so object-store errors use `loki_s3_request_duration_seconds_count{status_code}` and `loki_ingester_chunks_flush_failures_total`. Cost (the B2 free tier) is not observable from metrics and is not alerted on.

## Cost

B2 is free under 10 GB, then about $6.95 per TB per month. API calls are free.
