# Terraform

First Terraform usage in this repo. Everything else under `infra/` is Helm reconciled
by ArgoCD, which is entirely Kubernetes-scoped. Terraform exists here to manage
**host-level** configuration of the OrbStack machine — things ArgoCD cannot reach
(OrbStack's own settings and tmux crash-resilience).

## Layers

Two root modules, each with **its own local state**:

| Layer | Directory | State | Manages |
|---|---|---|---|
| 1 | `orbstack/` | `orbstack/terraform.tfstate` | OrbStack itself — the VM and its app settings |
| 2 | `tmux/` | `tmux/terraform.tfstate` | tmux session/crash-resilience config for host processes |

### Resources in the layers

| Layer | Resource | What it does |
|---|---|---|
| `orbstack/` | `settings.tf` | Declares OrbStack settings through `orbctl config set`, reading before writing so an unchanged value is never re-set. See "OrbStack settings" below. |

An ArgoCD port-forward LaunchAgent used to live here too. It was removed: the
overlay's `argocd-server` LoadBalancer answers directly at
`https://192.168.139.11`, so nothing needs forwarding — and the agent could
never have loaded on this host anyway, since its account has no desktop session.
See `infra/argocd/README.md`.

### Why separate states and not one

tmux configuration is only meaningful once OrbStack is up: the processes it
supervises talk to the cluster, and the cluster runs inside OrbStack. A single
state would allow one `apply` to touch both a half-built OrbStack and config that
assumes a working one, and a failure mid-run would leave a single state describing
a machine in neither condition. Separate states give the layers independent
lifecycles — `tmux/` can be re-applied repeatedly without re-planning the VM.

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

### Loose end: the unused `terraform` database

`infra/charts/nexus/postgres/templates/nexus-cluster.yaml` still declares a
`terraform` database, added when the plan was to use the `pg` backend. Nothing
uses it now. It is harmless — the Zalando operator creates databases but never
drops them, so removing the declaration would not delete it either — but it is
cruft, along with the `terraform_state_orbstack` / `terraform_state_tmux` schemas
left inside it. Both want cleaning up in a follow-up.

## Applying

Terraform **runs on the host**, in a checkout of this repo on the Mac mini. That
is a deliberate decision, not a limitation: these layers manage the machine the
cluster runs inside, and the state describing that machine should live on it.
Nothing else applies, so there is only ever one copy of the truth.

One consequence worth stating, because it is easy to assume otherwise:

* **Terraform does not use SSH to reach the host.** It is already on it.
  Resources are local — `local_file`, `terraform_data` with `local-exec` for
  `launchctl` and `orb` — not SSH provisioners.

### From the host

```bash
make -C infra/terraform plan                 # both layers, in order
make -C infra/terraform apply                # both layers, interactive
make -C infra/terraform plan  LAYER=tmux     # one layer
make -C infra/terraform apply LAYER=orbstack
```

Layer order is `orbstack`, then `tmux`, because `tmux/` reads
`../orbstack/terraform.tfstate`. A failed layer stops the run, so a later layer
is never applied on top of a broken earlier one.

### From a laptop

```bash
ssh -t agent@Lasses-Mac-mini \
  'cd Documents/Projects/nexus && git pull --ff-only && make -C infra/terraform apply'
```

`-t` because `apply` prompts: the wrapper never passes `-auto-approve`, so the
confirmation is Terraform's own.

### One-time setup on the host

1. Clone this repo as the `agent` account.
2. Install Terraform ≥ 1.6 (`terraform version` to confirm; 1.16.4 at the time
   of writing).

Nothing else. No Vault token, no port-forward and no cluster are involved.

### What the wrapper refuses, and why

| Refusal | Why |
|---|---|
| Not on the host (`scutil --get LocalHostName` ≠ `HOST_NAME`) | This is what keeps state off a laptop. `scutil` missing counts as "not the host", so a Linux box is refused too rather than erroring. |
| `apply` with a dirty working tree | The host should be configured from committed code, not from someone's edits. |
| `apply` when `HEAD` ≠ `origin/main` | The host should match what was reviewed. Checked after `git fetch origin main`. |
| An unknown `LAYER` | Catches typos rather than silently planning nothing. |

`plan` is deliberately exempt from the tree and branch checks — it is read-only,
and planning a branch before merging it is the point.

`FORCE=1` skips the tree and branch checks, with a warning on stderr. It does
**not** skip the host check: that one protects where state lives, which is the
whole decision.

### State backups

Before each `apply`, the layer's current `terraform.tfstate` is copied to
`<layer>/backups/terraform.tfstate.<UTC timestamp>`, keeping the last 20. The
directory is gitignored.

This is the minimal answer to "local state has no backup", below. **Known gap:
the backups are on the same disk as the state they protect.** They survive a
corrupting `apply`, which is the common case, and not a lost or wiped machine.
Off-host backup is a separate piece of work.

### Limits, plainly

* **Merging to `main` changes nothing on the host.** Someone has to run
  `make apply`. There is no reconciliation loop here, unlike the Helm charts
  ArgoCD watches.
* **No CI-triggered apply.** GitHub-hosted runners cannot reach the host. A
  self-hosted runner on the host could call `bin/terraform-run` unchanged, which
  is why the guards live in the script rather than in the Makefile.
* **The ordering between the layers is enforced.** `tmux/remote-state.tf`
  fails to plan until `orbstack/` has been applied at least once.


## OrbStack settings

Inventoried on the real host rather than from documentation, per ADR-0001's
live-verification standard. Host: Mac mini, macOS 26.4, arm64, 10 cores
(4 performance), 16384 MiB RAM. `orbctl version 2.2.3 (2020300)`.

### Correction: this inventory is of the wrong OrbStack

Found after the section below was merged, and measured rather than inferred.
**OrbStack is per macOS user, and this host runs two instances:**

| Account | OrbStack | Kubernetes |
|---|---|---|
| `agent` (uid 505) | the one this inventory and `settings.tf` ran against | **off** — `k8s.enable: false` is accurate |
| `lasse` (uid 501, owns the console) | **serves the `nexus` cluster** | on |

```
agent's kubeconfig    server: https://127.0.0.1:26443
netstat               *.26443  LISTEN  OrbStack Helper:18834
ps                    pid 18834  user lasse
```

`agent`'s kubeconfig reaches the cluster only because `lasse`'s OrbStack holds
port 26443. What follows from that:

- **Every value in the inventory below is `agent`'s instance, not the
  cluster's VM.** The method and the key list still hold — they are the same
  OrbStack build — but the values describe a VM that does not run the cluster.
- **`settings.tf` has never configured the cluster.** It runs as `agent`, so
  `orbctl config set` writes `agent`'s config. Applying it is harmless and
  pointless until it targets `lasse`'s instance.
- **The cluster's memory comes from `lasse`'s settings.** Raising `lasse`'s
  `memory_mib` to 10240 on 2026-10-16 took the node from 7.8 to 9.8 GiB
  allocatable, while `agent`'s `memory_mib` still read 8192.
- **The cluster stops if `lasse` logs out**, since `lasse`'s OrbStack runs in
  `lasse`'s desktop session.

**Open decision**, for the owner of the host: make Terraform manage `lasse`'s
instance (it would need to run as `lasse`, which `agent` cannot do without
sudo), move the cluster into `agent`'s OrbStack, or drop `settings.tf`. Until
then, treat the settings layer as a record of method rather than of the
cluster's configuration.

### How settings are exposed

**`orbctl config get|set|show` is the programmatic surface.** Each candidate
location was checked, not assumed — but one conclusion drawn from this table was
wrong, see below it:

| Candidate | Reality |
|---|---|
| `~/.orbstack/config/docker.json` | `{}` — 2 bytes |
| `~/.orbstack/vmconfig.json` | `{}` — 2 bytes **when nothing is overridden**; holds every non-default setting |
| `~/Library/Preferences/dev.kdrag0n.MacVirt.plist` | Sparkle updater keys only (`SULastCheckTime`, `SUUpdateGroupIdentifier`) |
| `~/Library/Group Containers/HUAQ24HBR6.dev.orbstack/` | VM disk images (`data.img.raw`, `swap.img`), no configuration |
| `~/Library/Application Support/OrbStack/` | does not exist |

This originally concluded there was no settings file, so `local_file` had
nothing to write. That was wrong: `vmconfig.json` read `{}` only because no
setting had ever been overridden. The first one did it — `orbctl config set
k8s.expose_services true` wrote `{"k8s.expose_services": true}` there.
`settings.tf` still uses `terraform_data` with `local-exec` calling `orbctl
config set`, and that remains the better mechanism: `orbctl` validates the key
and value and reports when a restart is needed to apply them, neither of which
writing the JSON directly would do.

**Nothing in `agent`'s instance had been configured explicitly.** Every value
`orbctl config show` printed was a built-in default, which the hardware
confirms: `cpu: 10` is all ten cores, and `memory_mib: 8192` is half of 16 GiB.
Per the correction above, these are `agent`'s values, not the cluster VM's.

### The full inventory

All 31 keys `orbctl config show` reports, with the values measured on this host:

```
app.start_at_login: false        machine.docker.memory_mib: 0
app.wait_for_container_stop: false   machine.docker.mounts:
cpu: 10                          machine.docker.username: root
data_allow_backup: false         machines.expose_ports_to_lan: true
docker.expose_ports_to_lan: true machines.forward_ports: true
docker.node_name: orbstack       memory_mib: 8192
docker.set_context: true         mount_hide_shared: false
k8s.enable: false                network.https: true
k8s.expose_services: false       network.proxy.exclude:
k8s.kubeconfig_use_domain: false network.subnet4: 192.168.138.0/23
k8s.tls_san:                     network_bridge: true
machine.docker.cpu: 0            network_proxy: auto
machine.docker.disk_bytes: 0     power.pause_in_sleep: true
machine.docker.http_port: 0      rosetta: true
machine.docker.https_port: 0     setup.use_admin: false
machine.docker.isolated: false   ssh.expose_port: false
```

Every one of these is readable and writable through the CLI, so none of them is
GUI-only. **What could not be audited from a terminal is the GUI itself**: if
OrbStack's settings window exposes anything that has no `config` key, this
inventory would not show it. That is the honest boundary — the list above is
"everything the CLI exposes", not "everything that exists". Checking the
remainder needs someone in front of the app.

Four settings are declared in `settings.tf`, each at its current value so the
first apply proves the mechanism and not the host: `cpu`, `memory_mib`,
`rosetta`, `power.pause_in_sleep`.

### `k8s.enable` is deliberately not managed

The measured reason, which is also the hazard:

```
orbctl config get k8s.enable   → false
kubectl get node orbstack      → Ready, control-plane, 117d, v1.35.6+orb1
orbctl status                  → Running
```

This originally argued that the setting "does not describe whether the engine
is running". **That was wrong.** It is accurate: Kubernetes really is off in
`agent`'s instance, which is the one being read. The 117-day cluster is
`lasse`'s (see the correction at the top of this section).

The safeguard stays, for a better reason: this layer runs as `agent` and can
only write `agent`'s `k8s.enable`, which does not control the cluster. Managing
it would look like managing the cluster's Kubernetes while doing nothing of the
kind — and if the layer is ever pointed at `lasse`'s instance, writing it there
*would* tear down everything `infra/` depends on. Either way it should take a
deliberate opt-in.

`manage_k8s_enable` therefore defaults to `false`, which makes the key absent
from the managed set entirely; `k8s_enable` is ignored until someone opts in.

How this was verified non-destructive, on this host:

```
$ terraform -chdir=infra/terraform/orbstack plan
Plan: 6 to add, 0 to change, 0 to destroy.
  + k8s_enable_managed = false
  + managed_settings   = { cpu, memory_mib, power.pause_in_sleep, rosetta }
```

`k8s.enable` is absent from the planned set, and the only planned resources are
the four settings plus two LaunchAgent resources that have since been removed. No
`orbctl config set k8s.enable` has ever been run here. The "discrepancy" this
paragraph once cited was two instances being compared as if they were one.

The setter also reads before writing, so applying a value that is already set
runs no `set` at all. That is deliberate for `cpu` and `memory_mib`: it is not
known whether writing an unchanged value restarts the VM, and not writing is
the only certain way not to find out during a routine apply.

### Two findings the inventory turned up

**The VM is too small for the conductor chart's own invariant.** The chart says
`maxSessions` x ~3 GiB of `cargo build` has to fit inside both the container
limit and the VM's memory:

```
maxSessions 5 x 3Gi  = 15 GiB needed
container limit      = 16 GiB    ✓
VM allocatable       =  7.8 GiB  ✗   (lasse's VM; agent's memory_mib 8192 is unrelated)
```

Violated by roughly 2x. The chart's own remedy is to lower `maxSessions`, since
raising the limit past what the host has "just moves the failure from a cargo
OOM to the pod being OOMKilled, which takes every running agent with it". Not
changed here: `memory_mib` cannot be raised beyond the host's 16 GiB usefully,
and `maxSessions` belongs to the chart.

Since resolved: `maxSessions` went to 2 (#194), and when the pod was still
OOM-killed the idle-session cost was measured at ~440 MiB, after which
`maxLiveSessions` went to 3 and the container limit to 6Gi (#197). `lasse`'s
VM was raised to 10 GiB the same day, leaving conductor about 5.7 GiB.

**`power.pause_in_sleep: true` is why App JWTs were rejected.** With the VM
paused during sleep the guest clock stops and resumes behind real time — 26
minutes behind, measured — and every GitHub App JWT the conductor pod signed
was already expired when GitHub read it. Keeping the host awake is the remedy
(`sudo pmset -a sleep 0`; `disablesleep` is not reported by `pmset` on this
host, so it cannot be verified). The VM that pauses is `lasse`'s, so its
`power.pause_in_sleep` is the one that matters — the value recorded here is
`agent`'s.

### Changing `cpu` or `memory_mib`

As this layer stands, changing them restarts **`agent`'s** VM, which does not
run the cluster — so it is safe and also achieves nothing. Changing the
cluster's VM means changing `lasse`'s settings, which `orbctl` confirms needs a
restart (`Restart OrbStack with "orbctl stop" to apply changes.`) and so takes
down the cluster and every running agent. Do that in a window where losing the
cluster is acceptable.

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
