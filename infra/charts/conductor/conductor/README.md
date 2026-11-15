# conductor

The webhook router that runs Claude Code agent sessions — one per session
group, each in its own git worktree on the pod's volume.

The old local `mcp` webhook pipeline has been removed from the repository
(NEX-140). Make sure no `hookdeck listen` is still running on any machine, or
every event is worked twice.

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

## Reviews

Every Linear issue that moves **into** `review.state` (default `In Review`)
gets an automatic review: a fresh Claude session running the `reviewer` agent
(`infra/agents/reviewer.md`, model `review.model`) reviews every open pull
request attached to the issue. Only a real transition counts
(`updatedFrom.stateId`), so editing an issue that sits in review starts
nothing. A review starts within seconds when a review slot is free; reviews run
off every trading path.

- **Its own set and slots.** Each review runs in the set
  `review-<identifier>` (e.g. `review-nex-270`), in a worktree detached at the
  pull request head, never in the author's worktree. It has its own limit,
  `review.maxSessions` (`CONDUCTOR_MAX_REVIEW_SESSIONS`), so a review never
  takes a slot from implementation work. The set is closed once the verdict is
  handled (worktree and registry row removed, transcript kept), so every
  review starts fresh and reads earlier reviews from GitHub.
- **The verdict.** The reviewer writes `<worktree>/.review/verdict.json`;
  conductor reads it when the turn ends (the `Stop` hook) and checks that every
  pull request is covered with the head SHA it was given. Conductor, not the
  agent, does the side effects:
  - `changes_requested`: one more send-back, a Linear comment starting
    `Review send-back k of N` with the summary and review links, and the issue
    moves to **Todo**, which dispatches the author's set as usual;
  - `approve`: a Linear comment, `needs-human` removed, the issue stays in
    review for you to merge;
  - **failed** (no verdict file, invalid JSON, wrong SHAs, a turn longer than
    `review.timeout`, or a Linear write that kept failing for 10 minutes): a
    Linear comment with the reason, no state change, no send-back counted;
  - a pull request whose head moved during the review: the verdict is
    discarded and one new review is queued;
  - the issue leaving review mid-review (Done, Todo, Canceled): the session is
    interrupted and the verdict discarded.
- **The limit.** After `review.maxIterations` send-backs (default 2, so at
  most 3 automatic reviews; 0 means manual only), entering review starts no
  review. The pull requests get the label `needs-human`, and the issue gets
  one comment saying the limit is reached. Only reviewer verdicts count; moving
  an issue back to Todo by hand does not.
- **`/review`.** A comment starting with `/review` on a pull request, from a
  `githubWebhookUsers` login, queues a review for the issue that owns it (the
  `NEX-<n>` in the branch name, else the Linear link in the description),
  whatever the limit. Conductor reacts with 👀 when it is queued; the comment is
  not sent to the author's set. At most one review per issue is queued or
  running at a time; further triggers are dropped and logged.
- **The reviewer bot.** Comments by `review.botLogin`
  (`CONDUCTOR_REVIEW_BOT_LOGIN`) are never dispatched to a session; a
  send-back reaches the author only through the move to Todo.

```bash
scripts/conductor reviews list              # send-backs, last verdict and time per issue
scripts/conductor reviews list --json
scripts/conductor reviews reset NEX-270     # send-backs back to 0
```

Settings (`values.yaml` `review`, environment in brackets): `enabled`
(`CONDUCTOR_REVIEW_ENABLED`, `false` turns every trigger off), `state`
(`CONDUCTOR_REVIEW_STATE`), `maxIterations` (`CONDUCTOR_REVIEW_MAX_ITERATIONS`),
`model` (`CONDUCTOR_REVIEW_MODEL`), `maxSessions`
(`CONDUCTOR_MAX_REVIEW_SESSIONS`), `timeout` (`CONDUCTOR_REVIEW_TIMEOUT`,
`45m`, `2700s` or `1h`), `botLogin` (`CONDUCTOR_REVIEW_BOT_LOGIN`). Metrics
`conductor_reviews_total{trigger,verdict}`,
`conductor_review_duration_seconds`, `conductor_review_limit_reached_total`,
`conductor_review_sessions{kind}`, `conductor_review_linear_identity{kind}`,
`conductor_review_linear_auth_failures_total`; alert
`ConductorReviewsFailing`; row "Reviews" on the Conductor dashboard.

### The reviewer's own name in Linear (Annette)

Review comments and the move back to Todo are made by a Linear OAuth app
acting as itself, so they appear under the app's name and icon and use no paid
seat. Conductor uses Linear's **client-credentials grant** (an `app`-actor
token, valid 30 days, no refresh token): it fetches the token from the client
id and secret, renews it a day before expiry or when Linear rejects it, and
keeps it only in memory. Everything else conductor does in Linear still uses
`LINEAR_API_KEY`. One-time setup:

1. In Linear, **Settings → API → OAuth applications → New**: name it
   **Annette** and upload her icon.
2. In the app's settings, turn on **client credentials** (the app actor).
   Without it Linear answers
   `Client does not support the client_credentials grant type`.
3. Install the app on the NexusBoard workspace. The token covers every public
   team.
4. Store the credentials with `patch`, so the other properties stay:

   ```bash
   vault kv patch secret/nexus/conductor REVIEW_LINEAR_CLIENT_ID=<id> REVIEW_LINEAR_CLIENT_SECRET=<secret>
   ```

   The `dataFrom: extract` ExternalSecret turns them into environment
   variables with no chart change (within its 1h refresh, then restart the
   pod). Never paste them into a pull request, issue or chat.

Without `REVIEW_LINEAR_CLIENT_ID`, review writes fall back to `LINEAR_API_KEY`,
one warning is logged at startup, and
`conductor_review_linear_identity{kind="fallback"}` is 1. If the app token
fails (renewal fails or a 401), the write is retried with backoff and counted
in `conductor_review_linear_auth_failures_total`; it never switches to
`LINEAR_API_KEY` mid-flight.

## Enabling webhook intake

Only once no local `hookdeck listen` is running anywhere:

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
the exact failure conductor replaced the old local watchdog to eliminate.

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
