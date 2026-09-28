# ArgoCD — deployment guide

ArgoCD is the GitOps continuous delivery controller for the Nexus cluster.
It watches the Git repository and reconciles cluster state to match what is
declared in source control — no manual `kubectl apply` required after bootstrap.

The cluster runs on **OrbStack** (macOS). All `kubectl` commands below pass
`--context orbstack` explicitly so they cannot be applied to the wrong cluster.

## Version

**ArgoCD v3.4.2** (official upstream `install.yaml`)

## Repository layout

```
infra/argocd/
  README.md                          — this file
  install/
    namespace.yaml                   — argocd Namespace
    kustomization.yaml               — pinned upstream install (v3.4.2)
  overlays/
    orbstack/
      kustomization.yaml             — patches argocd-server to LoadBalancer
      argocd-server-service.yaml     — LoadBalancer Service patch
      io.nexus.argocd-port-forward.plist — optional LaunchAgent (localhost:8080)
  root-app.yaml                      — App-of-Apps bootstrap
  apps/
    nexus-appset.yaml                — two ApplicationSets (see below)

infra/charts/                        — layout: infra/charts/<namespace>/<chart>/
  nexus/
    postgres-operator/ — Zalando Postgres Operator (umbrella, wraps upstream chart)
    postgres/          — nexus PostgreSQL cluster CR
    kafka/             — Strimzi operator + nexus Kafka cluster CRs
    vault/             — HashiCorp Vault standalone (umbrella, wraps upstream chart)
    external-secrets/  — External Secrets Operator + ClusterSecretStore
    grafana/           — Grafana
    prometheus/        — Prometheus + Alertmanager
  argocd/
    argocd-notifications/ — Slack notification config
    argocd-image-updater/ — image tag write-back to git
  signal/signal/       — signal service (Deployment)
  chronicle/chronicle/ — chronicle + market + earnings CronJobs, saxo-stream
  market/market/       — legacy market CronJob (superseded by chronicle/)
  insight/insight/     — MCP server exposing derived results
```

Adding `infra/charts/<namespace>/<chart>/` is all that is needed for ArgoCD to
deploy a chart into `<namespace>` — no manual Application CR required. Both
ApplicationSets derive the app name (`<namespace>-<chart>`) and target namespace
(`<namespace>`) from the directory path.

`apps/nexus-appset.yaml` defines **two** ApplicationSets, which differ only in
whether Image Updater tracks them:

| ApplicationSet | Generates from | Image Updater |
|---|---|---|
| `nexus-infra-charts` | `infra/charts/nexus/*`, `infra/charts/argocd/*`, `infra/charts/scanner/*` | no |
| `nexus-app-charts` | `infra/charts/signal/*`, `chronicle/*`, `market/*`, `insight/*` | yes — tracks `lmapwns/nexus`, semver, writes tags back to `main` |

> `infra/charts/scanner/` does not currently exist. The generator entry is
> harmless — a Git directory generator simply produces nothing for a missing
> path — but it is dead configuration.

## Target namespace

`argocd`

## Helm dependencies

The umbrella charts (`postgres-operator`, `kafka`, `vault`) vendor their upstream
Helm chart dependencies directly in git under each chart's `charts/` directory.
ArgoCD renders them from the repo with no outbound Helm registry calls — no
`argocd repo add` or repo Secret required.

To upgrade a dependency, run `helm dependency update infra/charts/<ns>/<chart>/`
and commit the updated `Chart.lock` and `charts/*.tgz`.

---

## Bootstrap steps

Performed **once** per cluster. Steps are in dependency order — later steps
assume earlier ones have completed.

### 1 — Prerequisites: OrbStack with Kubernetes enabled

Kubernetes is enabled in the OrbStack app, not from the CLI:

> **OrbStack → Settings → Kubernetes → enable the toggle**

OrbStack then adds an `orbstack` context to `~/.kube/config` automatically.
Verify before continuing:

```bash
kubectl config get-contexts orbstack
kubectl --context orbstack get nodes
```

The node should report `Ready`. OrbStack provides `local-path` as the default
StorageClass, which is what `postgres` and `vault` request:

```bash
kubectl --context orbstack get storageclass
```

No tunnel, host-port binding or driver flag is needed: OrbStack assigns
`LoadBalancer` services reachable IPs directly from the host.

### 2 — Create the namespace and install ArgoCD

```bash
kubectl --context orbstack apply -f infra/argocd/install/namespace.yaml

# Step A — main install (client-side apply)
kubectl --context orbstack apply -n argocd \
  -f https://raw.githubusercontent.com/argoproj/argo-cd/v3.4.2/manifests/install.yaml

# Step B — ApplicationSet CRD (server-side apply required)
# The applicationsets.argoproj.io CRD exceeds the 262 KB annotation limit for
# client-side apply. Server-side apply bypasses this restriction.
kubectl --context orbstack apply --server-side --force-conflicts \
  -f https://raw.githubusercontent.com/argoproj/argo-cd/v3.4.2/manifests/crds/applicationset-crd.yaml
```

Re-applying either step is idempotent — existing resources are patched, not
replaced.

### 3 — Wait for the core pods

```bash
for d in argocd-server argocd-repo-server argocd-applicationset-controller \
         argocd-dex-server argocd-notifications-controller; do
  kubectl --context orbstack rollout status deployment/$d -n argocd
done
```

Or watch them together:

```bash
kubectl --context orbstack get pods -n argocd -w
```

### 4 — Expose the ArgoCD UI

Apply the OrbStack overlay, which patches `argocd-server` to a `LoadBalancer`:

```bash
kubectl --context orbstack apply -k infra/argocd/overlays/orbstack/
```

Then read the assigned address:

```bash
kubectl --context orbstack get svc argocd-server -n argocd
```

Open `https://<EXTERNAL-IP>` (self-signed certificate — expect a browser
warning).

<details>
<summary>Optional: reach it at <code>https://localhost:8080</code> instead</summary>

A LaunchAgent is provided if you prefer a stable localhost address over the
assigned LoadBalancer IP:

```bash
cp infra/argocd/overlays/orbstack/io.nexus.argocd-port-forward.plist \
   ~/Library/LaunchAgents/
launchctl load -w ~/Library/LaunchAgents/io.nexus.argocd-port-forward.plist

# Check it is running
launchctl list io.nexus.argocd-port-forward

# Logs
tail -f /tmp/argocd-port-forward.log

# Uninstall
launchctl unload -w ~/Library/LaunchAgents/io.nexus.argocd-port-forward.plist
rm ~/Library/LaunchAgents/io.nexus.argocd-port-forward.plist
```
</details>

### 5 — Retrieve the initial admin password

```bash
kubectl --context orbstack get secret argocd-initial-admin-secret \
  -n argocd -o jsonpath='{.data.password}' | base64 -d; echo
```

> **Important:** delete this Secret once you have logged in and changed the
> password: `kubectl --context orbstack delete secret argocd-initial-admin-secret -n argocd`

CLI access:

```bash
brew install argocd

argocd login <argocd-address> --username admin --password <password> --insecure
```

### 6 — Apply the root Application (App of Apps)

The single manual apply that hands control to ArgoCD. After this, changes are
made by merging to `main`.

```bash
kubectl --context orbstack apply -f infra/argocd/root-app.yaml
```

ArgoCD then syncs `infra/argocd/apps/`, creating both ApplicationSets, which in
turn create one Application per chart directory. Watch progress:

```bash
kubectl --context orbstack get applications -n argocd -w
```

Expect `nexus-external-secrets` and `nexus-vault` to become Healthy, while
Applications owning ExternalSecrets stay degraded until Vault is bootstrapped in
step 8 — that is normal at this point.

### 7 — Grant the nexus service account token-review permission

Vault's Kubernetes auth method validates incoming JWTs against the Kubernetes
API, so the `nexus` service account needs the `system:auth-delegator`
ClusterRole:

```bash
kubectl --context orbstack create clusterrolebinding nexus-auth-delegator \
  --clusterrole=system:auth-delegator \
  --serviceaccount=nexus:nexus
```

This must run **after** step 6 (ArgoCD creates the `nexus` namespace and service
account) and **before** step 8 configures Vault's Kubernetes auth. Without it the
ClusterSecretStore fails with `permission denied` and every ExternalSecret stays
in `SecretSyncedError`.

### 8 — Bootstrap Vault

Vault is deployed by ArgoCD but starts **uninitialised**. Everything in this step
is one-time; secrets are created from scratch.

#### 8a — Reach the Vault API

The chart sets `ui.serviceType: LoadBalancer`, so Vault gets an external address:

```bash
kubectl --context orbstack get svc -n nexus | grep vault
export VAULT_ADDR="http://<vault-external-ip>:8200"
```

If no external IP is assigned, port-forward instead:

```bash
kubectl --context orbstack port-forward -n nexus svc/vault 8200:8200 &
export VAULT_ADDR="http://127.0.0.1:8200"
```

#### 8b — Initialise and unseal

Initialise with a **single** unseal key. This is deliberate: the chart's
`postStart` hook auto-unseals on restart by reading one key from a
`vault-unseal-key` Secret, so a multi-key split would break unattended restarts
and require manual unsealing every time the pod moves.

```bash
vault operator init -key-shares=1 -key-threshold=1
```

Record both values from the output — they are shown **once**:

- `Unseal Key 1`
- `Initial Root Token`

```bash
vault operator unseal <unseal-key>
export VAULT_TOKEN=<initial-root-token>
vault status          # Sealed should read false
```

#### 8c — Store the unseal key so restarts self-heal

The `postStart` hook reads `/vault-unseal/vault-unseal-key/key`, mounted from a
Secret that must be created by hand. It cannot come from External Secrets —
that would require a working Vault, which is what this key unseals.

```bash
kubectl --context orbstack create secret generic vault-unseal-key \
  -n nexus --from-literal=key=<unseal-key>
```

Without this Secret, Vault comes up **sealed** after any restart and every
ExternalSecret fails until it is unsealed manually.

#### 8d — Enable the auth method, KV store, policy and role

```bash
vault auth enable kubernetes

vault write auth/kubernetes/config \
  kubernetes_host="https://kubernetes.default.svc:443"

vault secrets enable -path=secret kv-v2

vault policy write nexus-read - <<'POLICY'
path "secret/data/nexus/*" {
  capabilities = ["read"]
}
POLICY

vault write auth/kubernetes/role/nexus \
  bound_service_account_names=nexus \
  bound_service_account_namespaces=nexus \
  policies=nexus-read \
  ttl=24h
```

The role name, service account and namespace must match
`infra/charts/nexus/external-secrets/templates/cluster-secret-store.yaml`,
which authenticates as role `nexus` using the `nexus` ServiceAccount in the
`nexus` namespace.

#### 8e — Populate the secrets

Every path below is referenced by an ExternalSecret; a missing path leaves its
Application degraded. Replace all placeholders with real values.

```bash
# argocd-image-updater — git write-back via GitHub App
vault kv put secret/nexus/github-app \
  app_id="<github-app-id>" \
  installation_id="<installation-id>" \
  private_key="@/path/to/private-key.pem"

# argocd-notifications — Slack
vault kv put secret/nexus/argocd-notifications \
  SLACK_TOKEN="<slack-bot-token>"

# grafana
vault kv put secret/nexus/grafana \
  GF_SECURITY_ADMIN_USER="admin" \
  GF_SECURITY_ADMIN_PASSWORD="<password>" \
  GF_DATABASE_PASSWORD="<password>"

# signal
vault kv put secret/nexus/signal \
  DATABASE_URL="postgresql://nexus:<password>@nexus-postgres.nexus.svc.cluster.local:5432/backtest" \
  KAFKA_BROKERS="nexus-kafka-bootstrap.nexus.svc.cluster.local:9092" \
  KAFKA_TOPIC="market.bars" \
  KAFKA_TICKERS_TOPIC="earnings.calendar" \
  KAFKA_GROUP_ID="signal" \
  POLYGON_API_KEY="<polygon-key>"

# chronicle
vault kv put secret/nexus/chronicle \
  DATABASE_URL="postgresql://nexus:<password>@nexus-postgres.nexus.svc.cluster.local:5432/chronicle" \
  KAFKA_BROKERS="nexus-kafka-bootstrap.nexus.svc.cluster.local:9092" \
  KAFKA_TOPIC="earnings.calendar" \
  KAFKA_GROUP_ID="chronicle" \
  POLYGON_API_KEY="<polygon-key>"

# chronicle — Saxo OpenAPI (see docs/adr/0003)
vault kv put secret/nexus/saxo \
  client_id="<saxo-client-id>" \
  client_secret="<saxo-client-secret>" \
  api_base="https://gateway.saxobank.com/sim/openapi" \
  streaming_base="wss://streaming.saxobank.com/sim/openapi/streamingws" \
  auth_base="https://sim.logonvalidation.net"

# market
vault kv put secret/nexus/market \
  POLYGON_API_KEY="<polygon-key>" \
  KAFKA_BROKERS="nexus-kafka-bootstrap.nexus.svc.cluster.local:9092" \
  KAFKA_TOPIC="market.bars"

# insight
vault kv put secret/nexus/insight \
  DATABASE_URL="postgresql://nexus:<password>@nexus-postgres.nexus.svc.cluster.local:5432/backtest"
```

The Postgres password is generated by the Zalando operator:

```bash
kubectl --context orbstack get secret \
  nexus.nexus-postgres.credentials.postgresql.acid.zalan.do \
  -n nexus -o jsonpath='{.data.password}' | base64 -d; echo
```

> **Known issue — Alertmanager secret does not sync.**
> `infra/charts/nexus/prometheus/templates/external-secret.yaml` requests
> `key: secret/nexus/alertmanager`, but the ClusterSecretStore already sets
> `path: secret`, so it resolves to `secret/data/secret/nexus/alertmanager` and
> can never be found. Every other ExternalSecret uses `key: nexus/<service>`.
> Creating `secret/nexus/alertmanager` will **not** fix it — the manifest needs
> correcting. Tracked separately; no `vault kv put` is listed here because the
> correct path depends on that fix.

### 9 — Verify

```bash
kubectl --context orbstack get externalsecrets -A
kubectl --context orbstack get applications -n argocd
```

Every ExternalSecret should report `SecretSynced` (except `alertmanager-slack`,
per the known issue above) and all Applications `Synced` / `Healthy`.

---

## Service addresses

Services are exposed as `LoadBalancer`, and OrbStack assigns each a reachable IP
rather than binding fixed host ports. Addresses are therefore **assigned, not
fixed** — read them from the cluster:

```bash
kubectl --context orbstack get svc -A --field-selector spec.type=LoadBalancer
```

| Service | Namespace | Type |
|---|---|---|
| ArgoCD UI | `argocd` | LoadBalancer (via the orbstack overlay) |
| Grafana | `nexus` | LoadBalancer |
| Prometheus | `nexus` | LoadBalancer |
| Vault UI/API | `nexus` | LoadBalancer (`vault.ui.serviceType`) |
| Kafka external listener | `nexus` | LoadBalancer (Strimzi assigns and advertises it) |
| insight | `insight` | LoadBalancer |
| chronicle `saxo-stream` | `chronicle` | NodePort **32080** — the one fixed port |

Postgres is intentionally **not** exposed: `nexus-postgres` is `ClusterIP` with
no external service. Reach it with a port-forward, targeting the **pod** — the
Zalando operator creates the primary service without a selector, so
`port-forward svc/...` fails with
`Service is defined without a selector`:

```bash
kubectl --context orbstack port-forward -n nexus pod/nexus-postgres-0 5432:5432
```

## Kafka CLI access

Read the advertised bootstrap address from the Kafka cluster, then use it:

```bash
kubectl --context orbstack get kafka nexus-kafka -n nexus \
  -o jsonpath='{.status.listeners[?(@.name=="external")].bootstrapServers}'; echo
```

```bash
BOOTSTRAP=<address-from-above>

kafka-topics --bootstrap-server "$BOOTSTRAP" --list
kafka-topics --bootstrap-server "$BOOTSTRAP" --describe --topic earnings.calendar
kafka-console-consumer --bootstrap-server "$BOOTSTRAP" --topic market.bars --from-beginning
```

Install the CLI tools on macOS with `brew install kafka`.

---

## Sync policy

All Applications use **automated sync with prune and self-heal**:

```yaml
syncPolicy:
  automated:
    prune: true     # delete resources removed from git
    selfHeal: true  # revert any manual kubectl changes
```

Manual `kubectl apply` against the `nexus` namespace is not the workflow after
bootstrap — ArgoCD reverts out-of-band changes within its reconcile interval
(~3 minutes).

## Upgrading ArgoCD

1. Update the pinned version in `infra/argocd/install/kustomization.yaml`
   (e.g. `v3.4.2` → `v3.5.0`)
2. Commit and push
3. Re-apply: `kubectl --context orbstack apply -k infra/argocd/install/`

## Out of scope (follow-up tasks)

- **SSO / OIDC** integration
- **RBAC** customisation
- Automating the Vault unseal beyond the single-key `postStart` hook
