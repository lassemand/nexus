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
| `REVIEW_LINEAR_READ_API_KEY` | optional: a Linear API key for review sessions' Linear MCP, in place of `LINEAR_API_KEY` ("The review session's boundary") |

Plus **exactly one** model credential:

| Property | Lifetime |
|---|---|
| `CLAUDE_CODE_OAUTH_TOKEN` | From `claude setup-token`; valid for about a year, cannot renew. Wins over the file when both are present. Every dispatch fails once it expires. Write its expiry date next to it in `CLAUDE_CODE_OAUTH_TOKEN_EXPIRES_AT` (below). |
| `CLAUDE_CODE_CREDENTIALS_JSON` | Seeds `~/.claude/.credentials.json`, which renews its access token but rotates its refresh token, so nothing else may hold a copy — see below. |
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

### Expiry alerts

Conductor exports `conductor_claude_credential_expiry_timestamp_seconds{source}`
(`docs/guides/metrics.md`), re-read every 5 minutes:

- `source="oauth_token"` when `CLAUDE_CODE_OAUTH_TOKEN` is set: the date comes
  from the optional property `CLAUDE_CODE_OAUTH_TOKEN_EXPIRES_AT`, an RFC 3339
  date or date-time (`2027-11-14`, or `2027-11-14T10:00:00Z`; a date alone is
  00:00 UTC). The token is opaque, so whoever creates it writes the date.
- `source="credentials_file"` otherwise: `claudeAiOauth.refreshTokenExpiresAt`
  of `$HOME/.claude/.credentials.json`, which moves on every renewal.

With `CONDUCTOR_AUTH_MODE=api_key` neither applies and no expiry is exported.
A missing or unparseable date is logged once per change and leaves the expiry
gauge unset, or at its last good value if it had been set (a gauge cannot be
removed); `conductor_claude_credential_expiry_known` goes to 0 either way and
`ConductorClaudeCredentialExpiryUnknown` fires after 1 hour. The
other alerts are `ConductorClaudeCredentialExpiring` (under 7 days, warning) and
`ConductorClaudeCredentialExpired` (critical). A revoked credential still
reports a valid date and is not detected by these.

### Rotating the token

1. `claude setup-token` on the Mac.
2. Patch, not put: put replaces every other property in the path.

   ```bash
   vault kv patch secret/nexus/conductor \
     CLAUDE_CODE_OAUTH_TOKEN=<new token> \
     CLAUDE_CODE_OAUTH_TOKEN_EXPIRES_AT=<date the token expires, e.g. 2027-11-14>
   ```

3. `kubectl -n conductor rollout restart deploy/conductor`. The ExternalSecret
   refreshes hourly; restart after it has synced.

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
  `review-<identifier>` (e.g. `review-nex-270`), in a standalone clone detached
  at the pull request head (`/data/worktrees/review-<id>`, writable by the
  reviewer uid), never in the author's worktree. It has its own limit,
  `review.maxSessions` (`CONDUCTOR_MAX_REVIEW_SESSIONS`), so a review never
  takes a slot from implementation work. The set is closed once the verdict is
  handled (worktree and registry row removed, transcript kept), so every
  review starts fresh and reads earlier reviews from GitHub.
- **The verdict.** The reviewer writes `<worktree>/.review/verdict.json`;
  conductor reads it when the turn ends (the `Stop` hook) and checks that every
  pull request is covered with the head SHA it was given. Conductor, not the
  agent, does the side effects:
  - `changes_requested`: a Linear comment starting `Review send-back k of N`
    with the summary and review links, then the issue moves to **Todo**, which
    dispatches the author's set as usual. The send-back is counted when the
    comment is posted, so a failed move never makes the next comment repeat
    `k of N`; the failed move is reported in a second comment;
  - an automatic review acts on its verdict only if the issue is still in the
    review state; an issue that went to Done or back to In Progress meanwhile is
    left alone, and an automatic trigger for such an issue starts nothing;
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
  one comment saying the limit is reached, once per send-back count (a
  redelivered webhook or a second entry into review adds nothing until the
  count changes or `reviews reset` runs). Only reviewer verdicts count; moving
  an issue back to Todo by hand does not.
- **Which pull requests.** Only those in the repository the worktrees come from
  (`review.repo`, `CONDUCTOR_REVIEW_REPO`, default the `origin` remote of
  `CONDUCTOR_REPO_ROOT`); others the issue lists are commented on and skipped.
  And only those by an **allowed author**, because a review session runs the
  pull request's code and reads its text: the pull request's GitHub author
  (`user.login`, read from GitHub when the review is triggered, compared
  case-insensitively) must be in `githubWebhookUsers` (`GITHUB_WEBHOOK_USERS`,
  the same list and parser as the comment filter) or be the author app's own bot
  login (e.g. `nexus-mcp[bot]`). The bot login is read from the author app at
  startup (`GET /app`, logged as `author app: <slug>[bot]`) and can be set or
  overridden with `CONDUCTOR_AUTHOR_BOT_LOGIN`; if it cannot be read and is not
  set, only `githubWebhookUsers` are reviewed and a warning names the override.
  The bot is **not** added to `githubWebhookUsers`: that would send the agent's
  own pull request comments back into its session. The check applies to every
  trigger (entering review, `/review`, a review request) and runs before the CI
  gate and before any side effect. Other pull requests of the issue are listed
  in a Linear comment as skipped; with none left, no session starts, the issue
  stays in review and the comment is `Review not started: no pull request by an
  allowed author (<repo> PR <n> by <login>, ...)`
  (`conductor_review_skipped_total{reason="author_not_allowed"}`).
  A pull request a `/review` names stays part of the review if it is queued
  again after a new head.
- **Waiting.** A review waits for a free review slot without a deadline. The
  "did not start" failure counts from the moment its set got a slot.
- **Reserved names.** The set `review-<identifier>` belongs to reviews: a
  `session:review-*` Linear label is ignored (the issue gets its own set), and
  a review never closes a set of another kind that has the same name.
- **`/review`.** A comment starting with `/review` on a pull request, from a
  `githubWebhookUsers` login and on a pull request by an allowed author (see
  above), queues a review for the issue that owns it (the
  `NEX-<n>` in the branch name, else the Linear link in the description),
  whatever the limit. Conductor reacts with 👀 when it is queued; the comment is
  not sent to the author's set. At most one review per issue is queued or
  running at a time; further triggers are dropped and logged.
- **Re-request review.** The same manual review starts when a
  `githubWebhookUsers` login requests a review from the reviewer app in the
  pull request's **Reviewers** box, as with Copilot (a `pull_request`
  `review_requested` event naming the reviewer bot). GitHub only lists a
  third-party app there once it has reviewed the pull request, so this is the
  **Re-request review** button next to Annette after her first, automatic
  review; before that, or for a pull request she never reviewed, use
  `/review`. Needs the GitHub webhook to send `pull_request` events (below).
- **The reviewer bot.** Comments by the reviewer app's bot login are never
  dispatched to a session; a send-back reaches the author only through the
  move to Todo. The login is read from the app at startup (see "The reviewer's
  GitHub identity"); `review.botLogin` (`CONDUCTOR_REVIEW_BOT_LOGIN`) overrides
  it.

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
`45m`, `2700s` or `1h`), `botLogin` (`CONDUCTOR_REVIEW_BOT_LOGIN`), `repo` (`CONDUCTOR_REVIEW_REPO`).
`enabled` is `true` in the chart since the review session's boundary
(NEX-285, "The review session's boundary" below) closed the gaps found in the
review of PR 325. Conductor keeps reviews off, with an error in its log, when
`CONDUCTOR_REVIEW_IDENTITY_DIR` is unset, and a review whose identity is not
ready starts no session (below). Metrics
`conductor_reviews_total{trigger,verdict}`,
`conductor_review_failures_total{reason}` (`identity_not_ready`),
`conductor_review_skipped_total{reason}` (`author_not_allowed`),
`conductor_review_duration_seconds`, `conductor_review_limit_reached_total`,
`conductor_review_sessions{kind}`, `conductor_review_linear_identity{kind}`,
`conductor_review_linear_auth_failures_total`; alert
`ConductorReviewsFailing`; row "Reviews" on the Conductor dashboard.

### CI gate

No automatic review starts for an issue whose pull request checks are not all
green. The agent still owns the move to In Review; conductor enforces it and
recovers when the agent moves too early. The check runs **inside**
`ReviewManager`'s trigger (`Reviews::trigger`, `conductor/src/review/ci_gate.rs`),
before the `review_runs` row, the `review-<id>` set, the worktree or the prompt
exist, and again right before the review is queued (the pull request heads are
read again, so a push in between means no review).

- **What counts as green.** Every check run on the head commit of every open
  pull request attached to the issue (`GET /repos/{repo}/commits/{sha}/check-runs`,
  paginated) is `completed` with conclusion `success`, `neutral` or `skipped`.
  No check runs yet is *pending*, not green. If GitHub cannot be read, CI is
  treated as not green.
- **Pending (or unreadable):** the issue is moved to **In Progress**, with the
  comment `CI not finished on <repo> PR <n>; moved back until it passes.`, and
  is added to the watch list (table `ci_watch`, kept across restarts).
- **Failed:** the issue is moved to **Todo** with a comment listing each failed
  check and a link to its log. This is not a review send-back
  (`send_backs` is unchanged).
- **Auto-promote.** When every check on a watched issue's pull requests turns
  green, conductor moves the issue to In Review; the move's webhook runs the
  gate again like any other entry into review. A failed check on a watched
  issue sends it to Todo and ends the watch. A push to a watched pull request
  moves the watch to the new head and restarts the wait. The watch is dropped
  (and nothing moved) when the issue becomes Done or Canceled or its pull
  requests close. An issue moved by hand to another state is left alone; the
  watch stays. After `review.ci.maxWait` (`CONDUCTOR_CI_MAX_WAIT`, default `2h`)
  with CI still running, conductor comments and stops watching; the issue stays
  in In Progress.
- **Events and poll.** Conductor handles `check_suite` and `check_run` events
  with action `completed` and `pull_request` `synchronize`, which arrive over
  the same Hookdeck path as the other GitHub events, so CI finishing leads to
  the state change in under 10 s. A poll every `review.ci.pollInterval`
  (`CONDUCTOR_CI_POLL_INTERVAL`, default `60s`) re-checks watched issues in
  case a webhook is lost. Redelivered webhooks make no second move or comment.
- **Limits.** A pull request with no check runs at all stays pending, so a PR
  that triggers no workflow (a future path-filtered workflow, a repository
  without Actions) sits in In Progress until `review.ci.maxWait` and then
  times out. Only the Checks API is read: an external CI that reports legacy
  commit statuses (`/commits/{sha}/status`) is invisible to the gate.
- **`/review` skips the gate:** a person asked for that review explicitly.
- **Setup:** subscribe the GitHub App **and** the repository webhook (the
  Hookdeck source `github`) to **Checks** events: the App needs the *Checks:
  read* permission and the `check_suite` and `check_run` events; the webhook
  needs `check_suite`, `check_run` and `pull_request`. Without them only the
  poll works.
- Metrics `conductor_ci_gate_total{outcome}` (`green`, `pending`, `failed`,
  `promoted`, `timeout`, `github_error`) and `conductor_ci_watch_issues`; row
  "CI gate" on the Conductor dashboard.

### The reviewer's GitHub identity (Annette)

The pull requests are authored by the author app (`nexus-mcp[bot]`), and GitHub
never lets an author approve its own pull request, so the reviewer is a second
GitHub App. Review sessions run with **only** its credentials, and its Contents
permission is read-only, so a push from a review session fails at GitHub even
if one were attempted. One-time setup:

1. **Create the app** (GitHub → Settings → Developer settings → GitHub Apps →
   New): name it `annette-review` (plain `annette` is taken; any free name
   works, its bot login becomes `<slug>[bot]`), upload Annette's avatar.
   - Permissions: **Pull requests** read and write, **Contents** read-only,
     **Issues** read and write (labels and comments), **Metadata** read-only.
   - **Webhook: off.** Conductor gets every event through the existing
     repository webhook.
2. **Generate a private key** (downloads a `.pem`), and note the App ID.
3. **Install it** on `lassemand/nexus` only; the installation id is the
   number at the end of the installation's settings URL.
4. **Store the credentials**, with `patch` so the other properties stay:

   ```bash
   vault kv patch secret/nexus/conductor REVIEW_GITHUB_APP_ID=<id> REVIEW_GITHUB_APP_INSTALLATION_ID=<installation id> REVIEW_GITHUB_APP_PRIVATE_KEY=@<file.pem>
   ```

   `<id>` is the app's client ID (`Iv…`, GitHub's recommended JWT issuer) or
   its numeric App ID; both work. The installation id is always a number.

   Then delete the `.pem`. Never paste the key into a pull request, issue or
   chat. The ExternalSecret picks the properties up within an hour; restart
   the pod (`kubectl -n conductor rollout restart deploy/conductor`).
5. **Subscribe the repository webhook to `pull_request` events** (repository
   → Settings → Webhooks → the Hookdeck one → "Let me select individual
   events" → add **Pull requests**). Only needed for **Re-request review**.

At every start the entrypoint (`configure_review_identity`) prepares
`/data/review` (`CONDUCTOR_REVIEW_IDENTITY_DIR` is `/data/review/identity`):

| Path | What | Reviewer uid |
|---|---|---|
| `identity/` | the reviewer's key, its ids, a git config with its own credential cache, an MCP config (GitHub as the reviewer app, Linear with `LINEAR_API_KEY` or the optional `REVIEW_LINEAR_READ_API_KEY`), the Claude settings (`conductor/deploy/reviewer-settings.json` plus the four conductor hooks), `bot-login`, and last `ready` (or `not-ready` with the reason) | read |
| `home/` | its `HOME`: `.claude.json`, the `reviewer` agent, transcripts, a copy of the model login | read and write |
| `cargo/`, `target/` | its own `CARGO_HOME` and `CARGO_TARGET_DIR` | read and write |

It mints a token to prove the credentials work and reads the app's slug
(`GET /app`) for the bot login, which it logs (`reviewer app:
annette-review[bot]`).

### The review session's boundary (NEX-285)

A review session reads untrusted text (the diff, comments) and runs the pull
request's code. So that a prompt injection in a pull request cannot merge,
close or edit pull requests, push, write to Linear as the author, or run
arbitrary commands as the author, the session is separated from conductor and
the author sessions in five ways.

**1. A separate uid, 1001 `reviewer`.** Conductor and the author sessions run
as uid 1000 and the container never runs as root. A review set's tmux pane
runs `conductor-review-launch claude ...` as uid 1000, which hands over to
`/usr/local/libexec/conductor-review-switch`, which becomes uid/gid 1001 and
execs `conductor-review-session` with an allow-listed environment. As 1001 the
session cannot read the author app's key (`/data/home/.config/github-app`), the
git credential cache socket, `/data/home/.claude.json` or `~/.claude`
credentials, a conductor process's `/proc/<pid>/environ`, or the tmux server's;
and cannot write any author worktree, the author's home, or the identity
directory (so it cannot rewrite its own settings). It can read and write its
own review checkout, `home/`, `cargo/` and `target/` (so a review compiles in
its own target directory, not the shared `CARGO_TARGET_DIR`: a build artifact
the reviewer can write must not be one the author builds from. The cost is one
more workspace build on `/data` per crate set touched, the first time, and
cargo's registry cache, a few hundred MB to a few GB; `rm -rf /data/review/target`
reclaims it).

- *Mechanism, and why it is the smaller surface.* A C program, 90 lines
  (`conductor/deploy/review-switch.c`), with the file capabilities
  `cap_setuid,cap_setgid+ep` set in the image. It can only become uid/gid 1001
  (compiled in; it cannot name root or anyone else), only runs for uid 1000 (the
  file is `root:agent 0750` and it checks its real uid), execs one fixed
  program, clears the environment down to an allow-list, drops supplementary
  groups, sets `no_new_privs` and `umask 0`. The alternative, a second
  container running the review tmux server as another uid, would need a shared
  volume with the author's container, a second set of credentials and probes,
  a way for conductor to drive a tmux server in another container (a socket
  both can write, which is itself an attack surface) and a different
  `sessions attach`; the helper changes none of that. The pod needs
  `allowPrivilegeEscalation: true` (the chart sets it explicitly) and keeps the
  default capability set; the entrypoint runs the switch with `--probe` and
  keeps the identity not ready if it does not reach uid 1001.
- *`/data` is made sticky.* The volume provisioner creates `/data` as `0777`
  without the sticky bit, which would let uid 1001 rename `/data/home` or
  `/data/worktrees` away and put its own in place. An init container
  (`data-sticky-bit`, root, all capabilities dropped but `FOWNER`, read-only
  root filesystem, one `chmod 1777 /data`) sets the bit before conductor
  starts; the entrypoint refuses to declare the reviewer ready while `/data` is
  world-writable without it. `/data/home` and `/data/state` are `0700`.
- *The review checkout is a clone, not a worktree.* A `git worktree` shares the
  author's `.git` (objects, config, hooks, the worktree index), which uid 1001
  must not write. Conductor makes a standalone clone (`git init` + `fetch` from
  the author's repository, `origin` pointing at GitHub) as uid 1000, opens it
  with `chmod -R a+rwX`, and never runs git inside it afterwards (its config and
  hooks are the reviewer's), removing it with `remove_dir_all`. The verdict
  file is read as a regular file of bounded size, never through a link.
- *The model login.* The reviewer cannot read `/data/home/.claude`, so
  `conductor-review-launch` copies the current `~/.claude/.credentials.json` into
  `home/.claude/` before each switch (subscription mode); with
  `ANTHROPIC_API_KEY` the key is passed through the switch's environment
  allow-list. A subscription login that refreshes in the reviewer's copy while
  the author's refreshes too can leave one of them stale until the next launch
  copies it again; a review is at most 45 minutes.
- *Tested.* `conductor/deploy/review-isolation-test.sh` starts sessions through
  tmux and the real switch and asserts every denial above plus that the
  operator's tmux (`sessions attach`, `send-keys`, `kill-session`) still
  drives the pane. CI job `review-isolation` runs it on a runner; the release
  and image-verification workflows run it inside the image.

**2. No raw GitHub API.** `Bash(gh api:*)` is gone. The reviewer app has Pull
requests: write, so `gh api` could close, edit or retitle any pull request.
Instead `conductor-review-gh` exposes `pr view|diff|checks <n>`, `reviews`,
`threads` (a fixed GraphQL query) and `post-review` (a `POST` of one
`.review/review-<n>.json` with only the keys `commit_id`, `event`, `body`,
`comments`), building every request itself, so the method and endpoint cannot be
chosen; replies and thread resolution use the GitHub MCP tools
(`add_reply_to_pull_request_comment`, `pull_request_review_write`). The wrapper
mints the reviewer app's token per call; no token sits in the session's
environment. Merge, close and edit paths (`merge_pull_request`,
`update_pull_request`, `issue_write`, `label_write`, ...) are not allowed and
are also on the deny list.

**3. No command injection through allowed tools.** `reviewer-settings.json`
allows only `conductor-review-git` (`log`, `diff`, `show`, `fetch origin`,
`checkout`, `status`, `rev-parse`, `blame`, `ls-files`), `conductor-review-cargo`
(`check`, `clippy`, `test`) and `conductor-review-gh`; raw `git`, `gh` and
`cargo` are denied. The wrappers reject `--config`, `-Z`, `--manifest-path`
outside the worktree, `--upload-pack`, `--receive-pack`, `-c`, `--exec-path`,
`--no-index`, `--output`, `ext::` remotes and more; clear `CARGO_*` (keeping
`CARGO_TARGET_DIR`), `RUSTC*`, `RUSTFLAGS`, `GIT_*`, `LD_PRELOAD` and the tokens;
refuse a worktree with a `.cargo/config` (a pull request controls it, and it can
name a runner or wrapper); and pin the toolchain to `stable`. Building a pull
request still runs its `build.rs` and proc macros: that is inherent to running
tests, and the uid is what contains it. Tests, one per rejected form, check the
exit code and that a marker file the injected command would have written is
absent (`conductor/tests/review_wrappers.rs`); `review_settings.rs` evaluates the
settings the way Claude Code's `dontAsk` mode does.

**4. Linear: the existing key, with the write tools denied.** The reviewer's
`mcp.json` uses `LINEAR_API_KEY` (the decision recorded on NEX-285: reuse the
existing write key rather than issue a separate one), or
`REVIEW_LINEAR_READ_API_KEY` when that optional property is set. The Linear write
tools (`save_*`, `delete_*`, `create_*`) are not allowed and are denied, but **the
key itself is readable by the review session**, so a prompt injection that runs
code (through a pull request's `build.rs`, say) could use it with `curl` to write
to Linear as the author. This is an accepted risk. To close it, create a Linear
API key with the **Read** permission only and store it, with `patch` so the other
properties stay (never `put`):

```bash
vault kv patch secret/nexus/conductor REVIEW_LINEAR_READ_API_KEY=<read-only key>
```

and restart the pod after the ExternalSecret's 1h refresh. Conductor's own review
writes (comments, state moves) are unchanged and run in conductor, not in the
session.

**5. Fail closed.** There is no fallback to the author's credentials any more.
Every failure in `configure_review_identity` logs `review identity NOT ready:
<reason>`, keeps `ready` absent and writes the reason to `identity/not-ready`.
`StartSpec::for_key` refuses a review set with no ready identity (and never sets
`--dangerously-skip-permissions` on a review). A review trigger that finds the
identity not ready starts no session, posts one Linear comment `Review not
started: the reviewer identity is not ready (<reason>)`, counts
`conductor_review_failures_total{reason="identity_not_ready"}` and leaves the
issue in review. Startup keeps reviews off, with an error, when
`CONDUCTOR_REVIEW_IDENTITY_DIR` is unset.

**Not covered.** There is no per-uid network egress control inside one pod, so a
review session can reach the network, and holds the model credential it needs
(`ANTHROPIC_API_KEY`, or its copy of the subscription login) and the reviewer
app's key (Pull requests: write, Contents read-only). Conductor's loopback
listeners (hook port, webhook port) accept connections from any uid in the pod;
the hook endpoint is unauthenticated, so a review session could forge a hook
event for a set. Sandboxing `build.rs` and proc macros beyond the uid is out of
scope.

Without the three `REVIEW_GITHUB_APP_*` properties, a Linear key, or a working
switch, the identity is not ready and reviews start nothing.

Known limit: GitHub does not let an app be picked as a reviewer before it has
reviewed (see **Re-request review**).

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

## One shared cargo build directory

Every session gets `CARGO_TARGET_DIR=/data/target` (`cargoTargetDir`), so the
worktrees share one workspace build instead of each keeping its own `target/`,
which reached 4–40 GB per worktree and filled the disk. The cost: cargo locks
the build directory, so two sessions building at the same moment run one
after the other ("Blocking waiting for file lock on build directory"). Branches
that differ only in workspace crates reuse every dependency artifact.

Set `cargoTargetDir: ""` to go back to one `target/` per worktree. To reclaim
space, delete `/data/target` while no session is building. Old `target/`
folders in existing worktrees are not used any more and can be deleted.

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
