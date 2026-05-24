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
  root-app.yaml                      — App-of-Apps bootstrap (NEX-32 follow-up)
  apps/                              — Child Application CRs  (NEX-32 follow-up)
```

## Target namespace

`argocd`

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

### 4 — Access the ArgoCD UI

#### Minikube (NodePort — recommended for local dev)

Apply the minikube overlay to expose the server on stable node ports:

```bash
kubectl apply -k infra/argocd/overlays/minikube/
```

Then open **https://192.168.49.2:30443** in your browser.  
Accept the self-signed certificate warning (expected on first access).

> If your minikube IP differs, check it with:
> `kubectl get node minikube -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}'`

#### Fallback — port-forward

```bash
kubectl port-forward svc/argocd-server -n argocd 8080:443
# → https://localhost:8080
```

Login with username `admin` and the password retrieved in step 3.

#### CLI access

```bash
# Install the argocd CLI (macOS)
brew install argocd

# Login (NodePort)
argocd login 192.168.49.2:30443 \
  --username admin \
  --password <password-from-step-3> \
  --insecure

# Login (port-forward)
argocd login localhost:8080 \
  --username admin \
  --password <password-from-step-3> \
  --insecure
```

## Upgrading ArgoCD

1. Update the `targetRevision` tag in `infra/argocd/install/kustomization.yaml`
   (e.g. `v3.4.2` → `v3.5.0`)
2. Commit and push
3. Re-apply: `kubectl apply -k infra/argocd/install/`

## Out of scope (follow-up tasks)

- **Connecting to the nexus Git repository** — NEX-32
- **Application CRs** for nexus services — NEX-32
- **SSO / OIDC** integration
- **RBAC** customisation
