# Terraform

First Terraform usage in this repo. Everything else under `infra/` is Helm reconciled
by ArgoCD, which is entirely Kubernetes-scoped. Terraform exists here to manage
**host-level** configuration of the OrbStack machine — things ArgoCD cannot reach
(OrbStack's own settings, tmux crash-resilience, a manually-installed LaunchAgent).

## Layers

Three root modules, each with **its own local state**:

| Layer | Directory | State | Manages |
|---|---|---|---|
| 0 | `vault-ssh/` | `vault-ssh/terraform.tfstate` | Vault's SSH CA — the credential later layers use to reach the host |
| 1 | `orbstack/` | `orbstack/terraform.tfstate` | OrbStack itself — the VM and its app settings |
| 2 | `tmux/` | `tmux/terraform.tfstate` | tmux session/crash-resilience config for host processes |

### Why separate states and not one

tmux configuration is only meaningful once OrbStack is up: the processes it
supervises talk to the cluster, and the cluster runs inside OrbStack. A single
state would allow one `apply` to touch both a half-built OrbStack and config that
assumes a working one, and a failure mid-run would leave a single state describing
a machine in neither condition. Separate states give the layers independent
lifecycles — `tmux/` can be re-applied repeatedly without re-planning the VM.

Layer 0 is separate for a different reason: it is the only layer that needs a
reachable cluster, a port-forward and a Vault token, and it is applied once
rather than on every change. Merging it into either of the others would have
made them need all three too.

Separate **root modules** rather than workspaces: workspaces share one
configuration and differ only by variables, whereas these layers manage entirely
different resources.

### Why local state, and not the `pg` backend on nexus-postgres

Layer 1 cannot use it, and layer 2 gains nothing from it.

**Layer 1 is a bootstrap layer.** The `pg` backend lives in Postgres → in
Kubernetes → **inside OrbStack**. Storing the state of the layer that *manages
OrbStack* there is circular: planning or applying it would require the VM to
already be up and healthy, with k8s, Postgres and a port-forward all working —
precisely the situation in which you most need Terraform to function. Not
hypothetical: an OrbStack restart during development made `terraform init`
impossible for over an hour.

**Layer 2 could have used it, but the backend buys nothing here.** The apparent
advantage would be durability, and the cluster does not provide it:

- `numberOfInstances: 1` with `storageClass: local-path` — one node-local disk
  inside the OrbStack VM
- `archive_mode = on` but `archive_command = /bin/true` — WAL archiving ships
  nothing anywhere
- no logical-backup configuration at the operator level

So state in `pg` is exactly as durable as a file on disk — one disk, no backups —
and it sits inside the VM, so an `orb reset` destroys it identically. What it does
charge is a pod port-forward and a credential fetch on every apply, plus continued
dependence on cluster health. Locking is its one genuine feature, and Postgres
advisory locks are moot for a single operator on a single machine.

### Known downsides of local state, accepted for now

- **No backup.** Lose the file and that layer's state is gone; resources must be
  re-imported. Nothing here backs it up. Note this is *not a regression* versus
  the `pg` backend, which has no backups either.
- **Not shared.** One machine, one operator. Fine today, not a team answer.
- **Absent in a fresh clone**, and never committed (`*.tfstate` is gitignored —
  state is credential-adjacent and must stay out of git).

Revisit when either layer grows real resources. The fix then is a backend that
lives *outside* this machine (object storage or Terraform Cloud), which breaks the
cycle without depending on one local file — not `pg` on a cluster inside the VM
being managed.

### Apply order

```bash
terraform -chdir=infra/terraform/vault-ssh apply  # layer 0, once (see below)
terraform -chdir=infra/terraform/orbstack  apply  # then layer 1
terraform -chdir=infra/terraform/tmux      apply  # then layer 2
```

Layers 1 and 2 need no cluster, port-forward or credential. **Layer 0 needs all
three**, which is why it is a separate root module rather than part of either:
folding Vault configuration into them would have cost them that property, and it
is the property that makes them usable when the cluster is broken.

Layer 0 is also not part of the enforced chain. `tmux/remote-state.tf` reads
layer 1's state, so that ordering fails loudly if ignored; nothing reads layer
0's state, because the thing it produces is consumed by `sshd` on the host
rather than by Terraform. Applying it is a one-time step, not part of the
routine cycle.

The ordering is **enforced, not just documented**. `tmux/remote-state.tf` reads
layer 1's state, and reading a state that does not exist is an error, so a `plan`
in `tmux/` fails with `Unable to find remote state` until `orbstack/` has been
applied at least once. If you see that, apply layer 1 — the guardrail is working.

### Loose end: the unused `terraform` database

`infra/charts/nexus/postgres/templates/nexus-cluster.yaml` still declares a
`terraform` database, added when the plan was to use the `pg` backend. Nothing
uses it now. It is harmless — the Zalando operator creates databases but never
drops them, so removing the declaration would not delete it either — but it is
cruft, along with the `terraform_state_orbstack` / `terraform_state_tmux` schemas
left inside it. Both want cleaning up in a follow-up.

## SSH access to the host (layer 0)

Terraform manages host-level configuration, which means something has to reach
the host. This layer provisions that credential (NEX-120).

### Signed certificates, not a stored keypair

Two options were on the table. The SSH secrets engine was chosen.

| | Vault SSH secrets engine (**chosen**) | Static keypair in Vault KV |
|---|---|---|
| What is stored | Nothing on disk; Vault holds the CA private key and never emits it | A long-lived private key, copied to every machine that connects |
| Expiry | Built in — a certificate lives 30m by default | None; valid until someone edits `authorized_keys` |
| Revoking access | A Vault policy change | Editing `authorized_keys` on the host, by hand |
| Host setup | One-time: trust the CA via `TrustedUserCAKeys` | One-time: append a public key to `authorized_keys` |
| Per-session cost | A Vault round-trip to sign | None |

The engine costs a Vault round-trip and slightly more host setup. What it buys
is that a leaked credential expires by itself, which is the pattern already
established everywhere else in this repo — ADR-0003's Saxo refresh tokens, the
GitHub App's minted installation tokens — and the reason none of them sit on
disk. A static key in KV would have been the one exception.

### One-time host setup

Vault's NodePort (32200) is not reachable from the host, so every command here
goes through a port-forward.

```bash
kubectl port-forward -n nexus svc/vault 8200:8200 &
export VAULT_ADDR=http://127.0.0.1:8200
export VAULT_TOKEN=<operator token>

terraform -chdir=infra/terraform/vault-ssh apply
```

Then trust the CA on the host. This needs `sudo`, and is the one step Terraform
cannot do for itself — it is editing the host's own `sshd`:

```bash
terraform -chdir=infra/terraform/vault-ssh output -raw ca_public_key \
  | sudo tee /etc/ssh/trusted-user-ca-keys.pem

# Append once, then reload sshd.
echo 'TrustedUserCAKeys /etc/ssh/trusted-user-ca-keys.pem' \
  | sudo tee -a /etc/ssh/sshd_config

# macOS: Remote Login must be on, and sshd is launchd-managed rather than
# systemd. `systemsetup -getremotelogin` reports the current state.
sudo systemsetup -setremotelogin on
```

### Connecting

`vault ssh` signs a certificate and hands it to `ssh` in one step:

```bash
vault ssh -mode=ca -role=host "$(whoami)@127.0.0.1"
```

Or sign explicitly, which is what a script would do:

```bash
ssh-keygen -t ed25519 -N '' -f /tmp/nexus-ssh          # throwaway keypair
vault write -field=signed_key ssh-client-signer/sign/host \
  public_key=@/tmp/nexus-ssh.pub > /tmp/nexus-ssh-cert.pub
ssh -i /tmp/nexus-ssh -o CertificateFile=/tmp/nexus-ssh-cert.pub \
    agent@127.0.0.1
```

The certificate is valid for 30 minutes (`certificate_ttl`), allows only the
`agent` account (`host_user`), and carries only `permit-pty` — enough for an
interactive shell, but not agent forwarding, port forwarding or X11.

Requesting one needs the `nexus-ssh-host` policy, which grants signing against
this single role and nothing else. Notably it does not grant read on the mount's
configuration, so holding it does not let you read or replace the CA.

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
| `GITHUB_WEBHOOK_SECRET` | removed `mcp` server (now `conductor`), verifies `X-Hub-Signature-256` | **unset** | Verification is silently skipped; unsigned payloads are accepted |
| GitHub App private key | `github-mcp-server` | `/Users/agent/.config/github-mcp/private-key.pem` | Mode `0644` — world-readable |
| `LINEAR_API_TOKEN` | linear MCP server | `~/.claude.json` | Plaintext in a config file |
| `GITHUB_WEBHOOK_USERS` | removed `mcp` server (now `conductor`) | unset (defaults to `lassemand`) | Not a secret — plain config, fine as-is |

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
