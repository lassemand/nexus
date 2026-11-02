# Long-term metrics (Thanos on Backblaze B2)

Prometheus keeps **14 days** of full-detail metrics locally (`retention: 14d`, `retentionSize: 9GB`, 20Gi `local-path` PVC). A Thanos sidecar uploads every finished 2-hour block to the B2 bucket `nexus-metrics`. History in B2:

| Age | Resolution |
|---|---|
| 0–14 days | raw |
| 14–30 days | 5-minute |
| 30–90 days | 1-hour |
| over 90 days | deleted |

## Components

| Where | What |
|---|---|
| `infra/charts/nexus/prometheus/` | Prometheus with the Thanos sidecar (`thanos.objectStorageConfig`), headless `prometheus-kube-prometheus-thanos-discovery` Service and its ServiceMonitor. The operator sets min/max block duration to 2h, which disables local compaction. |
| `infra/charts/nexus/thanos/` | Umbrella over `stevehipwell/thanos` 1.24.2 (Thanos 0.42.4), plus the `thanos-objstore` ExternalSecret. Query, Store Gateway, Compactor. Receive, Ruler and Query Frontend are off. |
| Grafana | Datasource `Thanos` (`uid: nexus-thanos`). `Prometheus` stays the default; moving dashboards over is a separate decision. |

- **Query** (`thanos-query:10902`): endpoints are the sidecar and the store gateway, replica label `prometheus_replica`, `--query.auto-downsampling`, `--query.partial-response` (dashboards show local data with a warning when B2 is unreachable). Requests 50m CPU / 128Mi, limit 512Mi.
- **Store Gateway**: `--max-time=-12d`, so it serves only data older than 12 days; the last 14 days come from local Prometheus (the 2-day overlap is deduplicated) and everyday dashboards never read B2. 128MB in-memory index cache plus a 128MB in-memory chunk cache; 5Gi `local-path` PVC. Requests 50m / 384Mi, limit 768Mi.
- **Compactor**: one replica, `--wait --wait-interval=30m`, downsampling on, `--retention.resolution-raw=14d`, `-5m=30d`, `-1h=90d` (every resolution is set: unset or `0s` means keep forever), `--debug.max-compaction-level=3` (merge 2h → 8h → 2d and stop; the flag exists in 0.42.4). With blocks capped at 2 days every retention holds to within 2 days. 5Gi `local-path` scratch PVC. Requests 50m / 256Mi, limit 1Gi.
- Sidecar requests 20m / 64Mi, limit 256Mi.

## Prerequisites (done by the owner)

- [x] B2 bucket `nexus-metrics` in `eu-central-003`, private, SSE-B2, lifecycle rule **"Keep only the last version"**. Without it the blocks the compactor replaces stay as hidden versions and are billed forever.
- [x] Application key restricted to that bucket, read and write.
- [x] Vault: `vault kv put secret/nexus/thanos AWS_ACCESS_KEY_ID=<keyID> AWS_SECRET_ACCESS_KEY=-` (secret from stdin). The ExternalSecret renders `objstore.yml` (`type: S3`, endpoint `s3.eu-central-003.backblazeb2.com`, region `eu-central-003`). Never paste keys into a PR or chat.

## Alerts

`nexus.metrics-longterm` in `infra/charts/nexus/prometheus/rules/nexus.rules.yml` (promtool-tested): `ThanosSidecarUploadFailing` (no upload for 4h), `ThanosCompactorHalted`, `ThanosCompactorNotRunning` (no iteration in 24h), `ThanosStoreGatewayBucketErrors`, `PrometheusLocalRetentionShort` (under 12 days held locally after 14 days uptime). Metric names come from the Thanos 0.42.4 source; confirm them on the running pods.

## Cost

B2 is about $6.95/TB/month with the first 10 GB free and free API calls. At roughly 300–600 MB of metrics per day expect 8–20 GB stored, so about $0.00–0.07 per month. Measure the real volume in Grafana (datasource Prometheus): `prometheus_tsdb_storage_blocks_bytes / 1e6 / 14`.

## Verification after the first sync

1. Within 3 hours: `nexus-metrics` contains block directories (one per 2h). Count them in the B2 console.
2. Prometheus pod has the `thanos-sidecar` container; `kubectl -n nexus get pods | grep thanos` shows query, store-gateway-0 and compact-0 Ready; Prometheus → Status → Targets shows all Thanos jobs up.
3. Grafana, datasource Thanos: a query over the last 7 days returns data. `thanos_bucket_store_blocks_loaded` is above 0 on the store gateway.

Follow-up checks (cannot be done at merge time):

- After 14 days: a Thanos query for a range older than 14 days (step 5m or more) still returns data, from 5-minute blocks (`thanos tools bucket ls` or the Query UI with "max source resolution" 5m).
- After 30 days: data older than 30 days is returned from 1-hour blocks.
- After 90 days (plus at most one 2-day block): nothing older remains in the bucket.

## Operations

- Query returning only partial data: check B2 status and `ThanosStoreGatewayBucketErrors`.
- Compactor halted: usually an overlapping or corrupt block; read its log before deleting anything in the bucket.
- Deleting the Prometheus PVC loses only blocks not yet uploaded (up to ~2h); deleting the bucket loses the history.
