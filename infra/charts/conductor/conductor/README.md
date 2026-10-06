# conductor

The webhook router that runs Claude Code agent sessions — one per session
group, each in its own git worktree on the pod's volume.

Deployed with **webhook intake switched off**. The local `mcp` setup is still
receiving Linear events; enabling intake here before that is retired would have
both dispatching the same work twice. Intake is enabled at cutover (NEX-132).

```
Linear ──┐
         ├─ Hookdeck ─→ hookdeck sidecars (off) ─→ conductor serve ─→ Postgres
GitHub ──┘                                              │
                                                        └─→ claude, one per group,
                                                            in /data/worktrees/<slug>
```

## Manual prerequisite: write the Vault secret

**The chart will not start without this.** The ExternalSecret reads
`secret/nexus/conductor`; until that path exists, `conductor-secret` is never
created and the pod stays in `CreateContainerConfigError`.

The agent that wrote this chart cannot write Vault secrets. This step is yours.

```bash
export VAULT_ADDR=http://192.168.139.10:8200
export VAULT_TOKEN="$(kubectl --context orbstack -n nexus get secret vault-unseal-key \
  -o jsonpath='{.data.root-token}' | base64 -d)"

vault status   # expect Sealed: false
```

The `nexus-reader` policy already grants `secret/data/nexus/*`, so no policy
change is needed.

### Required properties

| Property | Why |
|---|---|
| `DATABASE_URL` | The session registry. **conductor exits immediately without it.** |
| `GITHUB_TOKEN` | contents rw, pull requests rw, issues rw on `lassemand/nexus` |
| `LINEAR_API_KEY` | the Linear MCP server the agent uses to move issues |
| `GITHUB_WEBHOOK_SECRET` | webhook signature verification |
| `CONDUCTOR_AUTH_MODE` | `subscription` or `api_key` — the billing choice, made explicit |
| `HOOKDECK_API_KEY` | only needed once `hookdeck.enabled` is true |

Plus **exactly one** model credential:

| Property | Lifetime |
|---|---|
| `CLAUDE_CODE_CREDENTIALS_JSON` | **Preferred.** Renews itself — see below. |
| `CLAUDE_CODE_OAUTH_TOKEN` | ~2 hours, cannot renew. Every dispatch fails once it expires. |
| `ANTHROPIC_API_KEY` | No expiry; metered per-token billing. |

Both a subscription credential and `ANTHROPIC_API_KEY` present without
`CONDUCTOR_AUTH_MODE` is refused at startup rather than guessed — Claude Code
resolves `ANTHROPIC_API_KEY` first, so guessing wrong would silently bill
per-token while appearing to use the subscription.

### Writing it

`DATABASE_URL` needs the operator-managed Postgres password, which lives in the
**`nexus`** namespace — Kubernetes secrets do not cross namespaces, which is
why it goes through Vault:

```bash
PGPASS="$(kubectl --context orbstack -n nexus get secret \
  nexus.nexus-postgres.credentials.postgresql.acid.zalan.do \
  -o jsonpath='{.data.password}' | base64 -d)"

cat > /tmp/conductor.json <<JSON
{
  "DATABASE_URL": "postgresql://nexus:${PGPASS}@nexus-postgres.nexus.svc.cluster.local:5432/conductor",
  "GITHUB_TOKEN": "...",
  "LINEAR_API_KEY": "...",
  "GITHUB_WEBHOOK_SECRET": "...",
  "HOOKDECK_API_KEY": "...",
  "CONDUCTOR_AUTH_MODE": "subscription"
}
JSON
vault kv put secret/nexus/conductor @/tmp/conductor.json
rm -P /tmp/conductor.json
```

Then add the subscription credential. `@file` keeps it out of shell history,
and `patch` adds the key without replacing the ones above:

```bash
vault kv patch secret/nexus/conductor \
  CLAUDE_CODE_CREDENTIALS_JSON=@"$HOME/.claude/.credentials.json"
```

Confirm without printing values:

```bash
vault kv get -format=json secret/nexus/conductor | jq -r '.data.data | keys[]'
```

The `conductor` database itself is already provisioned by the Postgres cluster
CR (`infra/charts/nexus/postgres`), and conductor runs its own migrations at
startup.

## Credential renewal

Claude Code renews its own subscription credential.
`~/.claude/.credentials.json` holds an access token (~2 hours) and a refresh
token (~21 days); when the access token expires it exchanges the refresh token
and writes the new pair back. `HOME` is `/data/home` on the volume, so that
happens in place and survives restarts.

The entrypoint seeds that file **only if it is absent** — overwriting it would
roll the credential back to the Vault snapshot and undo every renewal since.

Two consequences worth knowing:

- **The Vault copy goes stale as soon as the pod refreshes.** That is fine while
  the volume survives, but it quietly degrades the recovery path: if the PVC is
  ever recreated, the entrypoint re-seeds from Vault, and that copy may by then
  be expired. Re-run the `vault kv patch` above occasionally if you want the
  recovery copy to stay usable.
- **The refresh token is the standing deadline.** The entrypoint reports the
  remaining days at every start, warns at 5 days
  (`CONDUCTOR_CREDENTIAL_WARN_DAYS`), and **fails the pod** once past — because
  an expired credential otherwise fails every dispatch while the pod still
  reports Ready, which reads as a dispatcher bug.

To renew by hand: `claude setup-token` on the Mac, then the `vault kv patch`
above, then `kubectl -n conductor rollout restart deploy/conductor`.

## Operating it

There is **no Service and no Ingress** by design. Webhooks arrive through the
in-pod sidecars over localhost; humans use `kubectl exec`.

```bash
# What is running
kubectl --context orbstack -n conductor exec -it deploy/conductor -c conductor -- \
  conductor sessions list

# Take a session over interactively
kubectl --context orbstack -n conductor exec -it deploy/conductor -c conductor -- \
  conductor sessions attach <group>

# Health, without a Service
kubectl --context orbstack -n conductor port-forward deploy/conductor 8788
curl -s -o /dev/null -w '%{http_code}\n' localhost:8788/health   # 200
```

`scripts/conductor` in the repository root wraps the exec form.

## Enabling webhook intake

Only after NEX-132 has retired the local `mcp` setup:

```yaml
hookdeck:
  enabled: true
```

`HOOKDECK_API_KEY` must be in Vault first. The second argument to
`hookdeck listen` is the Hookdeck **source** name (`linear`, `github`), not the
destination name (`cli-linear`, `cli-github`) — the wrong one makes the CLI
prompt interactively, which in a container means it hangs with no useful log.

## Memory, and why `maxSessions` is the dial

Each concurrent session may run a workspace `cargo build`, peaking around
2–3 GiB. So `maxSessions × 3Gi` has to fit inside `resources.limits.memory`
**and** inside the OrbStack VM's memory setting.

If both do not hold, lower `maxSessions`. Raising the limit past what the VM
actually has just moves the failure from a cargo OOM to the pod being
OOMKilled — which takes every running agent session with it.

## Single replica is a correctness requirement

`replicas: 1` with `strategy: Recreate`, and both matter.

The dispatcher verifies each group's recorded PID against its own `/proc` to
decide whether a run is still alive. A second pod reads the first pod's live
runs as dead, marks them `Lost`, resets those groups to `Idle`, and starts
duplicate runs in them — two concurrent resumes of one Claude conversation,
the exact failure conductor replaced `mcp-watchdog.sh` to eliminate.

`Recreate` is part of the requirement: the default `RollingUpdate` briefly runs
two pods, and that window is long enough for the new pod's startup reconcile to
do precisely that to the old pod's work.

Making this genuinely multi-replica would mean scoping liveness by host
(storing a node or boot id alongside the PID). Not planned.

## Known gaps

- **`GITHUB_WEBHOOK_SECRET` is only honoured if set.** With it absent,
  conductor logs a warning and accepts unsigned payloads — while binding
  `0.0.0.0:8788` and spawning `claude --dangerously-skip-permissions` from
  webhook content. Set it.
- **No Postgres backups.** The cluster runs a single instance on local-path
  storage with `archive_command = /bin/true`. It now holds the session
  registry as well as `chronicle` and `backtest`.
- **MCP tokens land in plaintext on the volume.** `claude mcp add` takes the
  header as an argument, so the tokens appear in that process's argv and are
  then stored in `~/.claude.json`. Inherent to the CLI, not to this chart.
- **Runtime worktree trust is unverified.** The entrypoint pre-trusts
  `/data/repo` and `/data/worktrees`, but `hasTrustDialogAccepted` is keyed by
  absolute path and worktrees appear at runtime as `/data/worktrees/<slug>`. If
  parent trust does not cascade, `sessions attach` will hang on a trust prompt.
