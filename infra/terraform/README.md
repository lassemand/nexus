# Terraform

First Terraform usage in this repo. Everything else under `infra/` is Helm reconciled
by ArgoCD, which is entirely Kubernetes-scoped. Terraform exists here to manage
**host-level** configuration of the OrbStack machine — things ArgoCD cannot reach
(OrbStack's own settings, tmux crash-resilience, a manually-installed LaunchAgent).

## Layers

Two root modules, each with **its own state**, and deliberately **different backends**:

| Layer | Directory | State | Manages |
|---|---|---|---|
| 1 | `orbstack/` | **local** — `orbstack/terraform.tfstate` | OrbStack itself — the VM and its app settings |
| 2 | `tmux/` | `pg` backend, schema `terraform_state_tmux` | tmux session/crash-resilience config for host processes |

### Why two states and not one

tmux configuration is only meaningful once OrbStack is up: the processes it
supervises talk to the cluster, and the cluster runs inside OrbStack. A single
state would allow one `apply` to touch both a half-built OrbStack and config that
assumes a working one, and a failure mid-run would leave a single state describing
a machine in neither condition. Separate states give the layers independent
lifecycles — `tmux/` can be re-applied repeatedly without re-planning the VM.

Separate **root modules** rather than workspaces: workspaces share one
configuration and differ only by variables, whereas these layers manage entirely
different resources.

### Why the backends differ

This is the important part, and it is not an inconsistency.

The `pg` backend lives in Postgres → in Kubernetes → **inside OrbStack**. So for
the layer that *manages OrbStack*, storing state there is circular: planning or
applying it would require the VM to already be up and healthy, with k8s, Postgres
and a port-forward all working. That is precisely the situation in which you most
need Terraform to function. It is not hypothetical — an OrbStack restart during
development made `terraform init` impossible for over an hour.

So layer 1 keeps state locally and depends on nothing. Layer 2 uses `pg`, because
its dependency on a running OrbStack is inherent anyway: moving its state out
would buy nothing.

#### Known downsides of the local state, accepted for now

- **No backup.** If the file is lost, the layer's state is gone and resources must
  be re-imported. Nothing here backs it up yet.
- **Not shared.** One machine, one operator. Fine today, not a team answer.
- **Absent in a fresh clone**, and never committed (`*.tfstate` is gitignored —
  state is credential-adjacent and must stay out of git).

If this layer grows real resources, revisit: the usual fix is a backend that lives
outside the machine entirely (object storage or Terraform Cloud), which breaks the
cycle without relying on one local file.

### Apply order

```bash
terraform -chdir=infra/terraform/orbstack apply   # layer 1 — no cluster needed
terraform -chdir=infra/terraform/tmux     apply   # layer 2 — needs the port-forward below
```

The ordering is **enforced, not just documented**. `tmux/remote-state.tf` reads the
orbstack layer's state, and reading a state that does not exist is an error, so a
`plan` in `tmux/` fails with `Unable to find remote state` until `orbstack/` has
been applied at least once. If you see that, apply layer 1 — the guardrail is
working.

## State backend (layer 2)

Layer 2 stores state in the dedicated `terraform` database on the existing
`nexus-postgres` cluster, via Terraform's native [`pg`][pg] backend, in schema
`terraform_state_tmux`.

[pg]: https://developer.hashicorp.com/terraform/language/backend/pg

### Why `pg` for this layer

| Option | Decision | Reasoning |
|---|---|---|
| **`pg` on existing `nexus-postgres`** | **Chosen** | Reuses a datastore already running, already operated by the Zalando operator, already with a credential lifecycle. No new infrastructure, no new secret. Supports locking via Postgres advisory locks. Its dependency on OrbStack costs nothing here, since this layer requires OrbStack regardless. |
| `local` state | Rejected *for this layer* | Not shared, no locking between shells, no backup. Correct for the bootstrap layer above, where reachability outweighs those costs; wrong here, where the `pg` backend is reachable whenever this layer is applicable at all. |
| Object storage (S3 / GCS / MinIO) | Rejected | Means standing up new storage to operate and back up, purely to hold state for a single-machine setup. |

### How the database is provisioned

Declaratively, in the Postgres CR — not by hand:

```yaml
# infra/charts/nexus/postgres/templates/nexus-cluster.yaml
databases:
  terraform: nexus
```

ArgoCD syncs the CR and the Zalando operator creates the database, owned by the
existing `nexus` role. The state schema is created by the `pg` backend itself on
first `init`; no manual DDL.

### Initialising layer 2

`nexus-postgres` is a `ClusterIP` service with no NodePort, so it is **not**
reachable from the host. Port-forward first — and note it must target the **pod**,
not the service: the Zalando operator creates the primary service *without a
selector* (Patroni manages its Endpoints so it can repoint at the current primary),
and `kubectl port-forward svc/...` needs a selector to find a pod. Forwarding the
service fails with `invalid service 'nexus-postgres': Service is defined without a
selector`.

```bash
kubectl port-forward -n nexus pod/nexus-postgres-0 5432:5432 &

PGPASSWORD="$(kubectl get secret nexus.nexus-postgres.credentials.postgresql.acid.zalan.do \
  -n nexus -o jsonpath='{.data.password}' | base64 -d)"

export PG_CONN_STR="postgres://nexus:${PGPASSWORD}@127.0.0.1:5432/terraform?sslmode=require"

terraform -chdir=infra/terraform/tmux init
```

Layer 1 needs none of this — it is local and has no dependencies.

Credentials are never written into a `.tf` file or committed: the backend takes its
connection string from the environment, which is why the `backend "pg"` block
declares only `schema_name`.

#### On `sslmode`

The cluster runs with `ssl = on`, but Spilo serves a **self-signed** certificate, so
`sslmode=require` is correct — encrypt without verifying the chain. `verify-ca` /
`verify-full` would fail unless the cluster CA were distributed to every machine
running Terraform, which is not worth it for a single host reached over a local
port-forward.

## Secrets the tmux layer needs

The tmux layer supervises the host processes that drive this repo's automation — the
Hookdeck listeners and the Claude Code session with its MCP servers. Those need
credentials, and **the repo's usual delivery mechanism does not work for them.**

### Why External Secrets Operator does not apply here

ESO's `ClusterSecretStore` authenticates to Vault with the **Kubernetes auth method**,
exchanging a TokenRequest for the `nexus` ServiceAccount:

```yaml
# infra/charts/nexus/external-secrets/templates/cluster-secret-store.yaml
auth:
  kubernetes:
    role: nexus
    serviceAccountRef: { name: nexus, namespace: nexus }
```

That identity only exists for workloads running **inside** the cluster. The tmux
layer's processes run on the **host**, outside Kubernetes, so they have no
ServiceAccount to present and ESO has no way to deliver to them. Host processes must
pull from Vault directly.

### What is needed

| Secret | Consumed by | Where it lives today | Gap |
|---|---|---|---|
| Hookdeck CLI API key | `hookdeck listen` | `~/.config/hookdeck/config.toml` (mode `0600`, per-user) | Static local file, not in Vault — lost on machine rebuild |
| `GITHUB_WEBHOOK_SECRET` | `mcp` server, verifies `X-Hub-Signature-256` | **unset** | Verification is silently skipped; unsigned payloads are accepted |
| GitHub App private key | `github-mcp-server` | `/Users/agent/.config/github-mcp/private-key.pem` | Mode `0644` — world-readable |
| `LINEAR_API_TOKEN` | linear MCP server | `~/.claude.json` | Plaintext in a config file |
| `GITHUB_WEBHOOK_USERS` | `mcp` server | unset (defaults to `lassemand`) | Not a secret — plain config, fine as-is |

### Storing them in Vault

Vault's NodePort (32200) is not reachable from the host, so port-forward. Paths follow
the existing `secret/nexus/*` convention, which the `nexus-read` policy already covers
(`secret/data/nexus/*`):

```bash
kubectl port-forward -n nexus svc/vault 8200:8200 &
export VAULT_ADDR=http://127.0.0.1:8200
export VAULT_TOKEN=<operator token>

vault kv put secret/nexus/hookdeck api_key=<hookdeck-api-key>
vault kv put secret/nexus/github   webhook_secret=<random-32-bytes> \
                                   app_private_key=@/path/to/private-key.pem
vault kv put secret/nexus/linear   api_token=<linear-api-token>
```

`webhook_secret` must be the *same* value configured on the GitHub side (App or repo
webhook settings), or every delivery fails signature verification.

### Materialising them for the session

Pull at session start rather than persisting to disk, so nothing long-lived is left
lying around:

```bash
export GITHUB_WEBHOOK_SECRET="$(vault kv get -field=webhook_secret secret/nexus/github)"
export GITHUB_WEBHOOK_USERS="lassemand"
```

The Hookdeck CLI reads its own config file rather than an environment variable, so its
key is written to `~/.config/hookdeck/config.toml` (mode `0600`) by `hookdeck login`;
Vault holds the copy of record for rebuilding that file.

### A bootstrap ordering consequence

Vault runs in the cluster, and the cluster runs inside OrbStack. So pulling these
secrets **requires OrbStack to already be up** — the same dependency the two-layer
split encodes. Any tmux startup that fetches from Vault must therefore run after the
orbstack layer, and will fail with connection errors rather than anything descriptive
if OrbStack is down. Check `orb status` first when a session comes up without its
credentials.
