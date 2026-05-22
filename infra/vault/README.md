# Vault — Local Minikube Setup

This directory contains the GitOps-managed configuration for a standalone HashiCorp
Vault instance running on minikube. No secrets are stored here — only structural
config (Helm values, policy HCL, role definitions, and the bootstrap script).

---

## Repository Layout

```
infra/vault/
├── values.yaml          # Helm chart values — committed, no secrets
├── policies/
│   └── nexus-read.hcl   # Vault policy granting read access to secret/nexus/*
├── roles/
│   └── nexus.json       # Kubernetes auth role bound to the "nexus" service account
├── setup.sh             # Idempotent bootstrap script
└── README.md            # This file

.vault-init.json         # Root token + unseal key — GITIGNORED, never committed
```

---

## Prerequisites

| Tool | Purpose |
|------|---------|
| `minikube` | Local Kubernetes cluster |
| `kubectl` | Cluster interactions |
| `helm` ≥ 3 | Chart install |
| `vault` CLI | Verify secrets after setup |
| `python3` | JSON parsing inside setup.sh |
| `jq` | Optional but handy for inspecting `.vault-init.json` |

---

## First-Time Setup

### 1. Add the Hashicorp Helm repo

```bash
helm repo add hashicorp https://helm.releases.hashicorp.com
helm repo update
```

### 2. Deploy Vault

```bash
helm install vault hashicorp/vault \
  --namespace vault --create-namespace \
  -f infra/vault/values.yaml
```

Wait for the `vault-0` pod to appear (it will be `0/1 Running` — this is normal
before init):

```bash
kubectl get pods -n vault -w
```

### 3. Init, unseal, and configure

Export your secret values, then run the bootstrap script:

```bash
export POLYGON_API_KEY="<your-polygon-key>"
export KAFKA_BROKER="<broker-url>"
export KAFKA_USERNAME="<user>"        # optional
export KAFKA_PASSWORD="<password>"    # optional

chmod +x infra/vault/setup.sh
./infra/vault/setup.sh --init --configure
```

The `--init` flag:
- Waits for the PVC to be `Bound` and the pod to be `Ready`
- Runs `vault operator init -key-shares=1 -key-threshold=1`
- Saves output to `.vault-init.json` (gitignored)
- Immediately unseals Vault

The `--configure` flag (runs after init in the combined invocation):
- Enables KV v2 at `secret/`
- Writes the `nexus-read` policy
- Enables and configures the Kubernetes auth method
- Creates the `nexus` auth role
- Writes initial secrets (`secret/nexus/polygon`, `secret/nexus/kafka`)

### 4. Verify

```bash
# Port-forward in one terminal
kubectl port-forward -n vault vault-0 8200:8200

# In another terminal
export VAULT_ADDR="http://127.0.0.1:8200"
export VAULT_TOKEN=$(jq -r '.root_token' .vault-init.json)

vault kv get secret/nexus/polygon
vault kv get secret/nexus/kafka
```

---

## Unseal After a minikube Restart

Vault seals itself whenever its pod is restarted. After `minikube start`:

```bash
./infra/vault/setup.sh --unseal
```

Or manually with a single command:

```bash
kubectl exec -n vault vault-0 -- \
  vault operator unseal "$(jq -r '.unseal_keys_b64[0]' .vault-init.json)"
```

> **Tip:** Add this to your minikube start alias so you never forget.

---

## Re-running Configuration (Idempotency)

All `--configure` steps are idempotent:

| Step | Behaviour on re-run |
|------|---------------------|
| `secrets enable kv-v2` | Skipped if already enabled |
| `policy write nexus-read` | Upsert — safe to re-run |
| `auth enable kubernetes` | Skipped if already enabled |
| `auth/kubernetes/config` | Upsert — safe to re-run |
| `auth/kubernetes/role/nexus` | Upsert — safe to re-run |
| `kv put secret/nexus/*` | Upsert — creates a new secret version |

---

## Kubernetes Auth Role

The `nexus` role (defined in `roles/nexus.json`) binds Vault access to:

- **Service account:** `nexus` in namespace `default`
- **Policy:** `nexus-read` (read-only on `secret/data/nexus/*`)
- **Token TTL:** 24 h (acceptable for local dev; shorten for production)

To authenticate as the nexus service account:

```bash
SA_TOKEN=$(kubectl create token nexus -n default)
vault write auth/kubernetes/login role=nexus jwt="${SA_TOKEN}"
```

---

## Security Notes

- `.vault-init.json` is gitignored. **Never commit it.**
- The 1-of-1 unseal key split is intentional for local dev convenience.
  Use auto-unseal (cloud KMS or Vault transit) in production.
- The root token should be revoked after initial setup in production:
  `vault token revoke <root-token>`
- Secret values (API keys, credentials) are always passed via environment
  variables to `setup.sh` — never hardcoded in any committed file.

---

## Upgrading Vault

```bash
helm repo update
helm upgrade vault hashicorp/vault \
  --namespace vault \
  -f infra/vault/values.yaml
```

The PVC is retained across upgrades so data is preserved.
