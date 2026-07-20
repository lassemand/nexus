# ArgoCD — deployment guide

ArgoCD is the GitOps continuous delivery controller for the Nexus cluster.
It watches the Git repository and reconciles cluster state to match what is
declared in source control — no manual `kubectl apply` required after bootstrap.

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
    minikube/
      kustomization.yaml             — patches argocd-server to NodePort 30080/30443
      argocd-server-nodeport.yaml    — NodePort Service patch
  root-app.yaml                      — App-of-Apps bootstrap
  apps/
    nexus-appset.yaml                — ApplicationSet (Git directory generator over infra/charts/*/*)

infra/charts/                        — layout: infra/charts/<namespace>/<chart>/
  nexus/
    postgres-operator/ — Zalando Postgres Operator (umbrella, wraps upstream Helm chart)
    postgres/          — nexus PostgreSQL cluster CR
    kafka/             — Strimzi operator + nexus Kafka cluster CRs
    vault/             — HashiCorp Vault standalone (umbrella, wraps upstream Helm chart)
    external-secrets/  — External Secrets Operator + ClusterSecretStore
  signal/
    signal/            — nexus signal service (Helm chart, Deployment)
  chronicle/
    chronicle/         — nexus chronicle binary (Helm chart, CronJob — EDGAR → earnings.calendar)
  market/
    market/            — nexus market binary   (Helm chart, CronJob — Polygon  → market.bars)
```

Adding `infra/charts/<namespace>/<chart>/` is all that is needed for ArgoCD to deploy a chart into
`<namespace>` — no manual Application CR required. The ApplicationSet derives both the app name
(`<namespace>-<chart>`) and the target namespace (`<namespace>`) from the directory path.

## Target namespace

`argocd`

## Helm dependencies

The umbrella charts (`postgres-operator`, `kafka`, `vault`) vendor their upstream
Helm chart dependencies directly in git under each chart's `charts/` directory.
ArgoCD renders them from the repo with no outbound Helm registry calls — no
`argocd repo add` or repo Secret required.

To upgrade a dependency, run `helm dependency update infra/charts/<name>/` and
commit the updated `Chart.lock` and `charts/*.tgz`.

---

## Bootstrap steps

These steps are performed **once** per cluster.

### 1 — Create the namespace and install ArgoCD

```bash
kubectl apply -f infra/argocd/install/namespace.yaml

# Step A — main install (client-side apply)
kubectl apply -n argocd \
  -f https://raw.githubusercontent.com/argoproj/argo-cd/v3.4.2/manifests/install.yaml

# Step B — ApplicationSet CRD (server-side apply required)
# The applicationsets.argoproj.io CRD exceeds the 262 KB annotation limit
# for client-side apply. Server-side apply bypasses this restriction.
kubectl apply --server-side --force-conflicts \
  -f https://raw.githubusercontent.com/argoproj/argo-cd/v3.4.2/manifests/crds/applicationset-crd.yaml
```

Re-applying either step is idempotent — existing resources are patched, not replaced.

### 2 — Wait for all core pods to be ready

```bash
kubectl rollout status deployment/argocd-server          -n argocd
kubectl rollout status deployment/argocd-repo-server     -n argocd
kubectl rollout status deployment/argocd-applicationset-controller -n argocd
kubectl rollout status deployment/argocd-dex-server      -n argocd
kubectl rollout status deployment/argocd-notifications-controller  -n argocd
```

Or watch all pods at once:

```bash
kubectl get pods -n argocd -w
```

### 3 — Retrieve the initial admin password

ArgoCD generates a random initial password for the `admin` user and stores it
in a Secret:

```bash
kubectl get secret argocd-initial-admin-secret \
  -n argocd \
  -o jsonpath='{.data.password}' | base64 -d; echo
```

> **Important:** Delete this Secret after you have logged in and changed the
> password via the UI or CLI:
> `kubectl delete secret argocd-initial-admin-secret -n argocd`

### 4 — Start minikube with host port bindings

NodePorts are accessible at `localhost` because minikube is started with explicit
`--ports` bindings baked into the Docker container.  Start it once with:

```bash
minikube start \
  --ports=30080:30080 \
  --ports=30443:30443 \
  --ports=32090:32090 \
  --ports=32092:32092 \
  --ports=32093:32093 \
  --ports=32200:32200 \
  --driver=docker \
  --memory=4096 \
  --cpus=2
```

After that every NodePort in the table below is permanently reachable at `localhost`
with no tunnel, no port-forward, and no daemon.

| Service | Host address |
|---|---|
| ArgoCD UI | `https://localhost:30443` |
| Kafka bootstrap | `localhost:32092` |
| Prometheus | `http://localhost:32090` |
| Vault UI | `http://localhost:32200` |

### 5 — Access the ArgoCD UI

Apply the minikube overlay to expose the ArgoCD server as a NodePort:

```bash
kubectl apply -k infra/argocd/overlays/minikube/
# → https://localhost:30443
```

```bash
# Check the agent is running
launchctl list io.nexus.argocd-port-forward

# View logs
tail -f /tmp/argocd-port-forward.log

# Uninstall
launchctl unload -w ~/Library/LaunchAgents/io.nexus.argocd-port-forward.plist
rm ~/Library/LaunchAgents/io.nexus.argocd-port-forward.plist
```

#### Linux / bare-metal minikube — NodePort

On Linux the Docker bridge **is** routable from the host. Apply the overlay
and open the URL directly — no daemon needed:

```bash
kubectl apply -k infra/argocd/overlays/minikube/
# → https://192.168.49.2:30443
```

#### CLI access

```bash
# Install the argocd CLI (macOS)
brew install argocd

# Login (macOS — LaunchAgent running)
argocd login localhost:8080 \
  --username admin \
  --password <password-from-step-3> \
  --insecure

# Login (Linux — NodePort)
argocd login 192.168.49.2:30443 \
  --username admin \
  --password <password-from-step-3> \
  --insecure
```

### 6 — Grant the nexus service account token-review permission

Vault's Kubernetes auth method needs to validate incoming JWTs against the
Kubernetes API. The `nexus` service account (in the `nexus` namespace) must
have the `system:auth-delegator` ClusterRole to do this:

```bash
kubectl create clusterrolebinding nexus-auth-delegator \
  --clusterrole=system:auth-delegator \
  --serviceaccount=nexus:nexus
```

This step must be performed **after ArgoCD has synced** (which creates the
`nexus` namespace and service account) and **before** Vault's Kubernetes auth
is configured.  Without it the ClusterSecretStore will fail with
`permission denied` and all ExternalSecrets will stay in `SecretSyncedError`.

### 7 — Apply the root Application (App of Apps)

This is the single manual `kubectl apply` that hands control to ArgoCD for
everything else.  After this step, all future changes are made by merging to
`main` — ArgoCD reconciles the cluster automatically.

```bash
kubectl apply -f infra/argocd/root-app.yaml
```

ArgoCD will:
1. Sync `infra/argocd/apps/` → create the `nexus-charts` ApplicationSet plus
   the explicit `nexus-chronicle` and `nexus-market` Application CRs
2. The ApplicationSet enumerates `infra/charts/*` (excluding `chronicle` and
   `market`) and creates one Application per remaining chart directory, all
   targeting the `nexus` namespace
3. `nexus-chronicle` and `nexus-market` deploy into their own `chronicle` and
   `market` namespaces respectively (created automatically via `CreateNamespace=true`)

Watch progress in the UI or with:

```bash
kubectl get applications -n argocd -w
```

---

## Kafka CLI access

The Kafka NodePort bootstrap is permanently available at `localhost:32092`
(bound via `--ports=32092:32092` at minikube start — no tunnel needed).

```bash
# List topics
kafka-topics --bootstrap-server localhost:32092 --list

# Describe a topic
kafka-topics --bootstrap-server localhost:32092 \
  --describe --topic earnings.calendar

# Consume from the beginning
kafka-console-consumer --bootstrap-server localhost:32092 \
  --topic market.bars --from-beginning
```

Install Kafka CLI tools on macOS:

```bash
brew install kafka
```

---

## Sync policy

All ArgoCD Applications use **automated sync with prune and self-heal**:

```yaml
syncPolicy:
  automated:
    prune: true     # delete resources removed from git
    selfHeal: true  # revert any manual kubectl changes
```

Manual `kubectl apply` against the `nexus` namespace is no longer the workflow
after bootstrap — ArgoCD will revert any out-of-band changes within its
reconcile interval (~3 minutes).

---

## Upgrading ArgoCD

1. Update the `targetRevision` tag in `infra/argocd/install/kustomization.yaml`
   (e.g. `v3.4.2` → `v3.5.0`)
2. Commit and push
3. Re-apply: `kubectl apply -k infra/argocd/install/`

## Out of scope (follow-up tasks)

- **SSO / OIDC** integration
- **RBAC** customisation
