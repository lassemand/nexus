# Terraform

First Terraform usage in this repo. Everything else under `infra/` is Helm charts
reconciled by ArgoCD, which is entirely Kubernetes-scoped. Terraform exists here to
manage **host-level** configuration of the OrbStack machine — things ArgoCD cannot
reach (tmux crash-resilience, a manually-installed LaunchAgent, OrbStack's own app
settings). Those resources are not declared yet; this directory currently only
establishes the state backend they will need.

## State backend

Remote state lives in a dedicated `terraform` database on the existing
`nexus-postgres` cluster, via Terraform's native [`pg`][pg] backend, in schema
`terraform_state`.

[pg]: https://developer.hashicorp.com/terraform/language/backend/pg

### Why `pg`, and not the alternatives

| Option | Decision | Reasoning |
|---|---|---|
| **`pg` on existing `nexus-postgres`** | **Chosen** | Reuses a datastore that is already running, already operated by the Zalando operator, and already has a credential lifecycle. No new infrastructure and no new secret to manage. Supports state locking via Postgres advisory locks. |
| `local` state | Rejected | The state file would live on the very machine Terraform is meant to configure — a circular dependency, and one disk failure from unrecoverable. It is also not shared, so no locking across two shells, and it must never be committed. |
| Object storage (S3 / GCS / MinIO) | Rejected | Would mean standing up new storage — a MinIO deployment to operate and back up, or a cloud account plus credentials — purely to hold state for a single-machine setup. Strictly more moving parts than reusing Postgres. |

The task explicitly ruled out standing up a second Postgres instance, and this
reuses the existing cluster rather than introducing storage of any kind.

### How the database is provisioned

Declaratively, not by hand. It is declared in the Postgres CR:

```yaml
# infra/charts/nexus/postgres/templates/nexus-cluster.yaml
databases:
  terraform: nexus
```

ArgoCD syncs the CR and the Zalando operator creates the database, owned by the
existing `nexus` role. Nothing here requires a manual `CREATE DATABASE`.

Verify it landed:

```bash
kubectl exec -n nexus nexus-postgres-0 -- \
  psql -U postgres -tAc "SELECT datname FROM pg_database WHERE datname = 'terraform';"
```

### Credentials

Credentials are **never** written into a `.tf` file or committed. The `pg` backend
reads its connection string from the `PG_CONN_STR` environment variable at init
time, which is why `versions.tf` declares `backend "pg"` with only `schema_name`.
The value comes from the operator-managed secret
`nexus.nexus-postgres.credentials.postgresql.acid.zalan.do` (keys: `username`,
`password`).

### Initialising

`nexus-postgres` is a `ClusterIP` service with no NodePort, so it is **not**
reachable from the host. Port-forward first:

```bash
kubectl port-forward -n nexus svc/nexus-postgres 5432:5432 &

PGPASSWORD="$(kubectl get secret nexus.nexus-postgres.credentials.postgresql.acid.zalan.do \
  -n nexus -o jsonpath='{.data.password}' | base64 -d)"

export PG_CONN_STR="postgres://nexus:${PGPASSWORD}@localhost:5432/terraform?sslmode=require"

terraform -chdir=infra/terraform init
```

On first `init` the backend creates the `terraform_state` schema and its state
table itself; no manual DDL is needed.

#### On `sslmode`

The cluster runs with `ssl = on`, but Spilo serves a **self-signed** certificate.
So `sslmode=require` is correct — it encrypts the connection without trying to
verify the chain. `verify-ca` / `verify-full` would fail unless the cluster CA is
distributed to every machine that runs Terraform, which is not worth doing for a
single-host setup reached over a local port-forward.

#### A note on the password in the environment

`PG_CONN_STR` holds the password, so it is visible to child processes and may land
in shell history. Prefer the command substitution above (which keeps it off disk)
over pasting a literal, and avoid exporting it in a long-lived shell profile.
