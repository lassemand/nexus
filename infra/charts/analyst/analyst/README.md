# analyst chart

Namespace `analyst`, picked up by the ApplicationSet (`infra/charts/analyst/analyst`). Runs the
`analyst` Deployment, its Service, PodMonitor, ServiceAccount and Role, two ExternalSecrets and
two NetworkPolicies. Architecture and settings: `docs/guides/analyst.md`.

`replicaCount` is `0` until the preparation launcher, releases and trades (NEX-253 to NEX-255) are
merged and the Vault keys below exist. Set it to `1` then; never more (the trade timers are in-process).

## Vault keys

`nexus/analyst` (database URLs; user `nexus`, as signal's):

```bash
vault kv put secret/nexus/analyst \
  DATABASE_URL='postgres://nexus:<PASSWORD>@nexus-postgres.nexus.svc.cluster.local:5432/analyst?sslmode=require' \
  SIGNAL_DATABASE_URL='postgres://nexus:<PASSWORD>@nexus-postgres.nexus.svc.cluster.local:5432/backtest?sslmode=require'
```

No read-only role exists on the `backtest` database, so `SIGNAL_DATABASE_URL` is the same role as
signal's; the service opens that connection read-only (`default_transaction_read_only = on`).

`nexus/analyst-prep` (needed before the first live preparation):

```bash
vault kv put secret/nexus/analyst-prep ANTHROPIC_API_KEY=<YOUR_ANTHROPIC_API_KEY>
```

Never commit a real key.

## Network isolation

`analyst-prep` selects pods labelled `app.kubernetes.io/name: analyst-prep`: no ingress; egress to
kube-dns, the `analyst` Service on 8081 and TCP 443 to `0.0.0.0/0` except the pod CIDR, service CIDR
and the MetalLB range (`networkPolicy.*` in `values.yaml`), so a Job reaches Anthropic, MFN and the web and
nothing in the cluster (no Kafka, Postgres, Vault or Alloy). The Jobs have no `OTEL_*` variables and run with
`automountServiceAccountToken: false`.

Check enforcement before the first live preparation (a policy no CNI enforces does nothing):

```bash
kubectl --context orbstack -n analyst run np-check --rm -it --restart=Never \
  --labels=app.kubernetes.io/name=analyst-prep --image=nicolaka/netshoot -- sh -c \
  'nc -z -w 3 nexus-kafka-bootstrap.nexus.svc 9092; echo "kafka exit=$? (must not be 0)"; curl -sI -m 5 https://api.anthropic.com | head -1'
```

## Alerts

Rules are in `infra/charts/nexus/prometheus/rules/nexus.rules.yml` (group `nexus.analyst`), tested in
`nexus.rules.test.yml`. Metric names of the release path (`analyst_release_to_decision_seconds`,
`analyst_quotes_total{outcome}`, and on the dashboard `analyst_publish_to_decision_seconds`,
`analyst_decisions_total{decision}`, `analyst_entry_delay_seconds`, `analyst_quote_lateness_seconds`,
`analyst_prep_duration_seconds`, `analyst_prep_cost_usd_total`) belong to NEX-254/255/253 and must match
what those tasks register.

| Alert | Expression | Runbook |
|---|---|---|
| `AnalystDown` | `kube_deployment_spec_replicas{namespace="analyst",deployment="analyst"} > 0 unless on () max(up{job="analyst"}) == 1` for 5 m | `kubectl -n analyst describe pod -l app=analyst`, logs; the Recreate strategy means a bad rollout leaves no pod. Replicas 0 never alerts. |
| `AnalystKernelMissingSoon` | `analyst_next_report_seconds` in `[0, 3600)` and `analyst_targets{status=~"waiting\|preparing\|failed"}` summing above 0, for 5 m | The next report has no accepted kernel and will abstain. Look at `prep_runs` and the Job logs; add a note (`nexus expect`) for a failed target to get one more attempt. |
| `AnalystPrepFailing` | `sum(increase(analyst_prep_failed_total[6h])) >= 3` | Split by `reason`: `launch` (RBAC, image), `failed`/`timed_out` (Job logs, API key, quota), `rejected` (the upload failed validation). |
| `AnalystDecisionSlow` | p99 of `analyst_release_to_decision_seconds` above 0.05 for 10 m | CPU throttling of the pod (limits 2 CPU), kernel run times, a slow database write on the release path. |
| `AnalystQuoteErrors` | non-ok share of `analyst_quotes_total` over 20 % (at least 5 quotes in 10 m), Mon-Fri 07:00-16:00 UTC, for 10 m | Outcome split on the dashboard; then the Saxo dashboard and `saxo_quote_requests_total{outcome}`. |
| `SaxoQuotesDown` | `increase(saxo_quote_requests_total{outcome="no_token"}[5m]) > 0` for 5 m | Re-authenticate saxo-stream (`docs/guides/saxo-oauth-setup.md`); entries fail until it has a token. |
