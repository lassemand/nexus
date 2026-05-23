#!/usr/bin/env bash
# setup.sh — Bootstrap Vault after a fresh Helm install or a restart.
#
# Usage:
#   First-time init:
#     ./infra/vault/setup.sh --init
#
#   After a minikube restart (unseal only):
#     ./infra/vault/setup.sh --unseal
#
#   Full configure (policy / auth / roles / secrets) after init+unseal:
#     ./infra/vault/setup.sh --configure
#
#   All-in-one (init + unseal + configure):
#     ./infra/vault/setup.sh --init --configure
#
# Environment variables consumed during --configure:
#   POLYGON_API_KEY   — Polygon.io API key          (required)
#   KAFKA_BROKER      — Kafka broker URL             (required)
#   KAFKA_USERNAME    — Kafka SASL username          (optional)
#   KAFKA_PASSWORD    — Kafka SASL password          (optional)
#
# The file .vault-init.json (written by --init) contains the unseal key and
# root token. It is gitignored and must never be committed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VAULT_INIT_FILE="${REPO_ROOT}/.vault-init.json"
VAULT_NAMESPACE="vault"
VAULT_POD="vault-0"
POLICY_FILE="${REPO_ROOT}/infra/vault/policies/nexus-read.hcl"
ROLE_FILE="${REPO_ROOT}/infra/vault/roles/nexus.json"

# ── Helpers ──────────────────────────────────────────────────────────────────

log()  { echo "[setup.sh] $*"; }
die()  { echo "[setup.sh] ERROR: $*" >&2; exit 1; }

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "'$1' not found in PATH. Please install it."
}

vault_exec() {
  kubectl exec -n "${VAULT_NAMESPACE}" "${VAULT_POD}" -- vault "$@"
}

wait_for_pod() {
  log "Waiting for ${VAULT_POD} to be Running..."
  kubectl wait pod "${VAULT_POD}" \
    -n "${VAULT_NAMESPACE}" \
    --for=condition=Ready \
    --timeout=120s
}

wait_for_pvc() {
  log "Waiting for PVC to be Bound (up to 60 s)..."
  local deadline=$(( $(date +%s) + 60 ))
  while true; do
    local status
    status=$(kubectl get pvc -n "${VAULT_NAMESPACE}" \
      -o jsonpath='{.items[0].status.phase}' 2>/dev/null || true)
    if [[ "${status}" == "Bound" ]]; then
      log "PVC is Bound."
      return 0
    fi
    if (( $(date +%s) >= deadline )); then
      die "PVC did not reach Bound state within 60 s (current: '${status}'). Aborting."
    fi
    sleep 3
  done
}

vault_addr_from_cluster() {
  # Resolve a VAULT_ADDR reachable from within the pod (in-cluster service).
  echo "http://vault.${VAULT_NAMESPACE}.svc.cluster.local:8200"
}

# ── Init ──────────────────────────────────────────────────────────────────────

do_init() {
  if [[ -f "${VAULT_INIT_FILE}" ]]; then
    log ".vault-init.json already exists — skipping init."
    log "If you need to re-init, delete .vault-init.json and the Vault PVC first."
    return 0
  fi

  wait_for_pvc
  wait_for_pod

  log "Initializing Vault (1-of-1 key split)..."
  kubectl exec -n "${VAULT_NAMESPACE}" "${VAULT_POD}" -- \
    vault operator init -key-shares=1 -key-threshold=1 -format=json \
    > "${VAULT_INIT_FILE}"

  log "Vault initialized. Unseal key and root token saved to .vault-init.json"
  log "IMPORTANT: keep .vault-init.json safe — it is gitignored and must not be committed."
}

# ── Unseal ─────────────────────────────────────────────────────────────────

do_unseal() {
  [[ -f "${VAULT_INIT_FILE}" ]] || die ".vault-init.json not found. Run --init first."

  wait_for_pod

  local sealed
  sealed=$(vault_exec status -format=json 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin)['sealed'])" 2>/dev/null || echo "true")

  if [[ "${sealed}" == "False" ]] || [[ "${sealed}" == "false" ]]; then
    log "Vault is already unsealed."
    return 0
  fi

  local unseal_key
  unseal_key=$(python3 -c "import json; d=json.load(open('${VAULT_INIT_FILE}')); print(d['unseal_keys_b64'][0])")

  log "Unsealing Vault..."
  vault_exec operator unseal "${unseal_key}"
  log "Vault unsealed successfully."
}

# ── Configure ─────────────────────────────────────────────────────────────────

do_configure() {
  [[ -f "${VAULT_INIT_FILE}" ]] || die ".vault-init.json not found. Run --init first."

  # Validate required env vars for secrets
  [[ -n "${POLYGON_API_KEY:-}" ]] || die "POLYGON_API_KEY env var is required for --configure."
  [[ -n "${KAFKA_BROKER:-}"    ]] || die "KAFKA_BROKER env var is required for --configure."

  local root_token
  root_token=$(python3 -c "import json; print(json.load(open('${VAULT_INIT_FILE}'))['root_token'])")

  export VAULT_ADDR
  VAULT_ADDR=$(vault_addr_from_cluster)
  export VAULT_TOKEN="${root_token}"

  # Port-forward Vault so local vault CLI can reach it
  log "Starting kubectl port-forward vault-0 8200:8200 (background)..."
  kubectl port-forward -n "${VAULT_NAMESPACE}" "${VAULT_POD}" 8200:8200 &
  local pf_pid=$!
  trap "kill ${pf_pid} 2>/dev/null || true" EXIT

  # Use local address for vault CLI
  export VAULT_ADDR="http://127.0.0.1:8200"

  # Give port-forward a moment to establish
  sleep 2

  # 1. Enable KV v2 (idempotent)
  log "Enabling KV v2 secrets engine at secret/..."
  vault secrets enable -path=secret kv-v2 2>/dev/null \
    || log "KV v2 at secret/ already enabled — skipping."

  # 2. Write policy (idempotent — vault policy write is always an upsert)
  log "Writing nexus-read policy..."
  vault policy write nexus-read "${POLICY_FILE}"

  # 3. Enable Kubernetes auth method (idempotent)
  log "Enabling Kubernetes auth method..."
  vault auth enable kubernetes 2>/dev/null \
    || log "Kubernetes auth already enabled — skipping."

  # 4. Configure Kubernetes auth against the in-cluster API server.
  #
  # IMPORTANT: we use the in-cluster service DNS and the CA cert / SA token
  # from the Vault pod's own projected service-account mount — NOT the local
  # kubeconfig. The kubeconfig points to 127.0.0.1 (minikube tunnel) which is
  # unreachable from inside the pod.
  log "Configuring Kubernetes auth (in-cluster endpoint)..."
  local k8s_host="https://kubernetes.default.svc.cluster.local"

  local sa_jwt
  sa_jwt=$(kubectl exec -n "${VAULT_NAMESPACE}" "${VAULT_POD}" -- \
    cat /var/run/secrets/kubernetes.io/serviceaccount/token)

  local ca_cert
  ca_cert=$(kubectl exec -n "${VAULT_NAMESPACE}" "${VAULT_POD}" -- \
    cat /var/run/secrets/kubernetes.io/serviceaccount/ca.crt)

  vault write auth/kubernetes/config \
    token_reviewer_jwt="${sa_jwt}" \
    kubernetes_host="${k8s_host}" \
    kubernetes_ca_cert="${ca_cert}"

  # 5. Write Kubernetes auth role (idempotent — vault write is an upsert)
  log "Writing nexus Kubernetes auth role..."
  vault write auth/kubernetes/role/nexus @"${ROLE_FILE}"

  # 6. Write initial secrets (idempotent — kv put is an upsert)
  log "Writing secret/nexus/polygon..."
  vault kv put secret/nexus/polygon \
    api_key="${POLYGON_API_KEY}"

  log "Writing secret/nexus/kafka..."
  vault kv put secret/nexus/kafka \
    broker="${KAFKA_BROKER}" \
    username="${KAFKA_USERNAME:-}" \
    password="${KAFKA_PASSWORD:-}"

  log ""
  log "✓ Vault configuration complete."
  log "  Verify with: vault kv get secret/nexus/polygon"
}

# ── Entrypoint ────────────────────────────────────────────────────────────────

require_cmd kubectl
require_cmd python3
require_cmd vault

DO_INIT=false
DO_UNSEAL=false
DO_CONFIGURE=false

if [[ $# -eq 0 ]]; then
  echo "Usage: $0 [--init] [--unseal] [--configure]"
  exit 1
fi

for arg in "$@"; do
  case "${arg}" in
    --init)      DO_INIT=true ;;
    --unseal)    DO_UNSEAL=true ;;
    --configure) DO_CONFIGURE=true ;;
    *) die "Unknown argument: ${arg}" ;;
  esac
done

${DO_INIT}      && do_init
${DO_INIT}      && do_unseal   # always unseal after a fresh init
${DO_UNSEAL}    && ! ${DO_INIT} && do_unseal
${DO_CONFIGURE} && do_configure
