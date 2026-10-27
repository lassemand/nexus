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

## Cost

B2 is free under 10 GB, then about $6.95 per TB per month. API calls are free.
