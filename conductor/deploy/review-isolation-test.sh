#!/usr/bin/env bash
#
# Proves the review session's boundary with the real mechanism, not a model of
# it: the real entrypoint prepares the identity, the real tmux starts
# conductor-review-launch, which goes through the real conductor-review-switch
# (file capabilities) to the reviewer uid, and every probe below runs as that
# uid inside the session. Needs no network, GitHub or Linear: the GitHub App
# credential helper is a stub and Claude itself is replaced by `sh -c`.
#
# Run as the agent user (not root), with the image's layout: the switch, the
# wrappers and conductor-review-launch installed, and a data directory that is
# sticky (`chmod 1777`, which the chart's init container does):
#
#   docker run --rm --user 1000:1000 -v "$tmp:/data" -v "$PWD/conductor/deploy:/test:ro" \
#     nexus-conductor:verify bash /test/review-isolation-test.sh
#
# The CI job `review-isolation` runs it on a runner with a freshly built switch;
# its uids and paths come from the environment:
#   REVIEW_TEST_DATA          data directory (default /data)
#   CONDUCTOR_REVIEW_UID      the reviewer's uid (default 1001)
#   CONDUCTOR_REVIEW_SWITCH   the switch binary
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA="${REVIEW_TEST_DATA:-/data}"
REVIEWER_UID="${CONDUCTOR_REVIEW_UID:-1001}"
export CONDUCTOR_REVIEW_UID="$REVIEWER_UID"
export CONDUCTOR_REVIEW_SWITCH="${CONDUCTOR_REVIEW_SWITCH:-/usr/local/libexec/conductor-review-switch}"

TALLY="$(mktemp -d)"
cleanup() {
  tmux -L review-isolation kill-server >/dev/null 2>&1 || true
  [[ -n "${SECRET_PID:-}" ]] && kill "$SECRET_PID" >/dev/null 2>&1 || true
  [[ -n "${SOCKET_PID:-}" ]] && kill "$SOCKET_PID" >/dev/null 2>&1 || true
  rm -rf "$TALLY"
}
trap cleanup EXIT
ok()   { printf '%s\n' "$1" >> "${TALLY}/pass"; printf '  ok    %s\n' "$1"; }
bad()  { printf '%s\n' "$1" >> "${TALLY}/fail"; printf '  FAIL  %s\n' "$1"; [[ -n "${2:-}" ]] && printf '          %s\n' "$2"; }

[[ "$(id -u)" != "0" ]] || { echo "run as the agent user, not root" >&2; exit 2; }
AGENT_UID="$(id -u)"

export CONDUCTOR_DATA_DIR="$DATA"
unset CONDUCTOR_REVIEW_IDENTITY_DIR CONDUCTOR_REVIEW_ROOT CONDUCTOR_REVIEW_SKIP_SWITCH_CHECK
export CLAUDE_CODE_VERSION=9.9.9-test
# shellcheck source=conductor/deploy/entrypoint.sh
CONDUCTOR_ENTRYPOINT_LIB=1 . "${HERE}/entrypoint.sh"
set +e

printf '== the identity is prepared by the real entrypoint ==\n'

init_layout >/dev/null
SANDBOX="$(mktemp -d)"
cat > "${SANDBOX}/helper" <<'STUB'
#!/usr/bin/env bash
case "${1:-}" in
  get) cat >/dev/null; [[ -r "${GITHUB_APP_PRIVATE_KEY_PATH:-}" ]] || exit 1
       printf 'username=x-access-token\npassword=token-for-%s\n' "$GITHUB_APP_ID" ;;
  app-slug) printf 'annette-review\n' ;;
esac
STUB
chmod 0755 "${SANDBOX}/helper"
export GITHUB_APP_CREDENTIAL_HELPER="${SANDBOX}/helper"
export CONDUCTOR_REVIEW_SETTINGS_SRC="${HERE}/reviewer-settings.json"
mkdir -p "${SANDBOX}/agents"
printf -- '---\nname: reviewer\n---\n' > "${SANDBOX}/agents/reviewer.md"
export CONDUCTOR_AGENT_DIR="${SANDBOX}/agents"
export REVIEW_GITHUB_APP_ID=42 REVIEW_GITHUB_APP_INSTALLATION_ID=7
REVIEW_GITHUB_APP_PRIVATE_KEY="$(openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 2>/dev/null)"
export REVIEW_GITHUB_APP_PRIVATE_KEY
export REVIEW_LINEAR_READ_API_KEY=read-only-key
export LINEAR_API_KEY=author-linear-key GITHUB_TOKEN=author-github-token ANTHROPIC_API_KEY=session-model-key

configure_review_identity >/dev/null 2>&1
IDENTITY="${DATA}/review/identity"
export CONDUCTOR_REVIEW_IDENTITY_DIR="$IDENTITY"
if [[ -f "${IDENTITY}/ready" ]]; then
  ok "the identity is ready (the entrypoint's own uid-switch probe passed)"
else
  bad "the identity is ready" "$(cat "${IDENTITY}/not-ready" 2>/dev/null)"
  exit 1
fi

AUTHOR_KEY="${HOME}/.config/github-app/private-key.pem"
mkdir -p "$(dirname "$AUTHOR_KEY")" "${HOME}/.cache/git" "${HOME}/.claude" "${DATA}/worktrees/author-task" "${DATA}/worktrees/review-nex-1"
chmod 0700 "${HOME}/.cache/git"
printf 'author key\n' > "$AUTHOR_KEY" && chmod 0600 "$AUTHOR_KEY"
printf '{"oauth":"author"}\n' > "${HOME}/.claude.json" && chmod 0600 "${HOME}/.claude.json"
printf '{"claudeAiOauth":"author-login"}\n' > "${HOME}/.claude/.credentials.json" && chmod 0600 "${HOME}/.claude/.credentials.json"
printf 'author work\n' > "${DATA}/worktrees/author-task/file.txt"
chmod 0755 "${DATA}/worktrees/author-task"
chmod 0777 "${DATA}/worktrees/review-nex-1"
chmod 0755 "${DATA}/worktrees"

SOCKET="${HOME}/.cache/git/credential-socket"
python3 - "$SOCKET" <<'PY' &
import os, socket, sys, time
path = sys.argv[1]
s = socket.socket(socket.AF_UNIX)
s.bind(path)
os.chmod(path, 0o600)
s.listen(1)
time.sleep(600)
PY
SOCKET_PID=$!
env CONDUCTOR_SECRET=only-conductor-sees-this sleep 600 &
SECRET_PID=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do [[ -S "$SOCKET" ]] && break; sleep 0.2; done

printf '== the session runs as the reviewer, started the way conductor starts it ==\n'

chmod 0777 "$SANDBOX"
tmux -L review-isolation -f /dev/null new-session -d -s agent-session -x 100 -y 20 "sleep 600"
tmux -L review-isolation display -p -t agent-session '#{pid}' > "${SANDBOX}/tmux.pid"
chmod 0644 "${SANDBOX}/tmux.pid"

PROBES="${SANDBOX}/probes.sh"
OUT="${SANDBOX}/probe.out"
cat > "$PROBES" <<PROBE
#!/usr/bin/env bash
cd "${DATA}/worktrees/review-nex-1"
res() { printf '%s=%s\n' "\$1" "\$2"; }
denied() { local name="\$1"; shift; if "\$@" >/dev/null 2>&1; then res "\$name" allowed; else res "\$name" denied; fi; }
allowed() { local name="\$1"; shift; if "\$@" >/dev/null 2>&1; then res "\$name" allowed; else res "\$name" denied; fi; }
res uid "\$(id -u)"
res gid "\$(id -g)"
res groups "\$(id -G)"
res caps "\$(awk '/^CapEff/ {print \$2}' /proc/self/status)"
res nonewprivs "\$(awk '/^NoNewPrivs/ {print \$2}' /proc/self/status)"
res home "\$HOME"
res umask "\$(umask)"
denied read_author_key cat "${AUTHOR_KEY}"
denied read_claude_json cat "${HOME}/.claude.json"
denied read_claude_credentials cat "${HOME}/.claude/.credentials.json"
denied list_author_home ls "${HOME}"
denied read_conductor_environ cat "/proc/${SECRET_PID}/environ"
denied read_tmux_server_environ cat "/proc/\$(cat ${SANDBOX}/tmux.pid)/environ"
denied connect_credential_socket python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])' "${SOCKET}"
denied write_author_worktree touch "${DATA}/worktrees/author-task/planted"
denied create_in_worktrees touch "${DATA}/worktrees/planted"
denied delete_author_file rm -f "${DATA}/worktrees/author-task/file.txt"
denied write_identity touch "${IDENTITY}/planted"
denied replace_settings cp /dev/null "${IDENTITY}/settings.json"
denied run_the_switch "${CONDUCTOR_REVIEW_SWITCH}" --probe
denied become_root setpriv --reuid=0 true
allowed write_own_worktree touch "${DATA}/worktrees/review-nex-1/verdict-probe"
allowed read_identity cat "${IDENTITY}/app.env"
allowed write_own_home touch "\$HOME/probe"
allowed write_cargo_home touch "${DATA}/review/cargo/probe"
allowed write_target_dir touch "${DATA}/review/target/probe"
allowed read_own_login cat "\$HOME/.claude/.credentials.json"
res env_secrets "\$(env | grep -cE '^(LINEAR_API_KEY|GITHUB_TOKEN|GH_TOKEN|GITHUB_APP_PRIVATE_KEY|CONDUCTOR_SECRET|REVIEW_GITHUB_APP_PRIVATE_KEY|REVIEW_LINEAR_READ_API_KEY)=')"
res env_model_key "\$(env | grep -c '^ANTHROPIC_API_KEY=')"
res env_identity_dir "\${CONDUCTOR_REVIEW_IDENTITY_DIR:-}"
denied rename_worktrees mv "${DATA}/worktrees" "${DATA}/worktrees.moved"
denied rename_author_home mv "${HOME}" "${HOME}.moved"
denied rename_author_state mv "${DATA}/state" "${DATA}/state.moved"
PROBE
chmod 0755 "$PROBES"

tmux -L review-isolation -f /dev/null new-session -d -s review-nex-1 -x 200 -y 50 \
  -c "${DATA}/worktrees/review-nex-1" \
  conductor-review-launch bash -c "bash '${PROBES}' > '${OUT}' 2>&1; sleep 120"
for _ in $(seq 1 100); do [[ -s "$OUT" ]] && grep -q '^rename_author_state=' "$OUT" && break; sleep 0.2; done
if [[ ! -s "$OUT" ]]; then
  bad "the session ran" "$(tmux -L review-isolation capture-pane -p -t review-nex-1 2>&1 | tail -5)"
  exit 1
fi

got() { sed -n "s/^$1=//p" "$OUT" | head -1; }
expect() { local name="$1" want="$2" label="$3"; local have; have="$(got "$name")"; [[ "$have" == "$want" ]] && ok "$label" || bad "$label" "expected [$want], got [$have]"; }

expect uid "$REVIEWER_UID" "the session runs as the reviewer uid, not the agent's"
expect gid "$REVIEWER_UID" "with the reviewer's group only"
expect groups "$REVIEWER_UID" "and no supplementary group"
expect caps "0000000000000000" "with no capability left after the switch"
expect nonewprivs "1" "and no_new_privs set"
expect home "${DATA}/review/home" "with its own HOME"
expect umask "0000" "with umask 0, so the agent can remove what it writes"
[[ "$AGENT_UID" != "$REVIEWER_UID" ]] && ok "the two uids differ" || bad "the two uids differ"

printf '== as that uid, the author'"'"'s files and processes are out of reach ==\n'

expect read_author_key denied "reading the author app's private key fails"
expect read_claude_json denied "reading ~/.claude.json fails"
expect read_claude_credentials denied "reading ~/.claude credentials fails"
expect list_author_home denied "listing the author's home fails"
expect read_conductor_environ denied "reading a conductor process's /proc/<pid>/environ fails"
expect read_tmux_server_environ denied "reading the tmux server's environment fails"
expect connect_credential_socket denied "connecting to the author's git credential cache socket fails"
expect write_author_worktree denied "writing into an author worktree fails"
expect create_in_worktrees denied "creating a directory beside the author worktrees fails"
expect delete_author_file denied "deleting an author file fails"
expect rename_worktrees denied "renaming the worktrees directory away fails"
expect rename_author_home denied "renaming the author's home away fails"
expect rename_author_state denied "renaming the author state directory away fails (the sticky bit)"
expect write_identity denied "writing its own identity directory fails"
expect replace_settings denied "replacing its own settings fails"
expect run_the_switch denied "running the switch again fails"
expect become_root denied "becoming root fails"

printf '== and what it needs is writable ==\n'

expect write_own_worktree allowed "it writes its own review worktree"
expect read_identity allowed "it reads its identity"
expect write_own_home allowed "it writes its own home"
expect write_cargo_home allowed "it writes its own cargo home"
expect write_target_dir allowed "it writes its own target directory"
expect read_own_login allowed "it reads its own copy of the model login"
expect env_secrets 0 "none of the author's secrets is in its environment"
expect env_model_key 1 "it keeps the model key it was started with"
expect env_identity_dir "${DATA}/review/identity" "and knows its identity directory"

cmp -s "${HOME}/.claude/.credentials.json" "${DATA}/review/home/.claude/.credentials.json" \
  && ok "the login it reads is a copy of the author's current one" || bad "the login it reads is a copy of the author's current one"
[[ ! -e "${DATA}/worktrees/author-task/planted" && -e "${DATA}/worktrees/author-task/file.txt" && -d "${DATA}/worktrees" ]] \
  && ok "afterwards the author's tree is intact" || bad "afterwards the author's tree is intact"
rm -f "${DATA}/worktrees/review-nex-1/verdict-probe" && ok "the agent removes what the reviewer wrote in its worktree" || bad "the agent removes what the reviewer wrote in its worktree"
rm -f "${DATA}/review/home/probe" && ok "and in the reviewer's home" || bad "and in the reviewer's home"

printf '== tmux still works for the operator ==\n'

pane_pid="$(tmux -L review-isolation display -p -t review-nex-1 '#{pane_pid}')"
[[ "$(ps -o uid= -p "$pane_pid" | tr -d ' ')" == "$REVIEWER_UID" ]] \
  && ok "the pane's process is the reviewer's" || bad "the pane's process is the reviewer's"
tmux -L review-isolation has-session -t review-nex-1 && ok "the operator's tmux sees the session" || bad "the operator's tmux sees the session"
tmux -L review-isolation capture-pane -p -t review-nex-1 >/dev/null 2>&1 && ok "capture-pane works (attach reads the pane)" || bad "capture-pane works"
tmux -L review-isolation send-keys -t review-nex-1 Escape >/dev/null 2>&1 && ok "send-keys works (interrupt)" || bad "send-keys works"
tmux -L review-isolation pipe-pane -t review-nex-1 'cat >/dev/null' >/dev/null 2>&1 && ok "the operator's tmux can drive the pane" || bad "the operator's tmux can drive the pane"
tmux -L review-isolation kill-session -t review-nex-1
for _ in $(seq 1 50); do kill -0 "$pane_pid" 2>/dev/null || break; sleep 0.1; done
if kill -0 "$pane_pid" 2>/dev/null && [[ "$(ps -o stat= -p "$pane_pid" | tr -d ' ')" != Z* ]]; then
  bad "closing the session ends the reviewer's process"
else
  ok "closing the session ends the reviewer's process"
fi

printf '== the reviewer cannot use the operator'"'"'s tmux either ==\n'

tmux -L review-isolation new-session -d -s review-nex-1 -x 200 -y 50 -c "${DATA}/worktrees/review-nex-1" \
  conductor-review-launch bash -c "tmux -L review-isolation ls > '${SANDBOX}/tmux-from-reviewer.out' 2>&1; echo rc=\$? >> '${SANDBOX}/tmux-from-reviewer.out'; sleep 30"
for _ in $(seq 1 50); do grep -q '^rc=' "${SANDBOX}/tmux-from-reviewer.out" 2>/dev/null && break; sleep 0.2; done
grep -q '^rc=0' "${SANDBOX}/tmux-from-reviewer.out" && bad "the reviewer cannot list the agent's tmux sessions" "$(cat "${SANDBOX}/tmux-from-reviewer.out")" \
  || ok "the reviewer cannot list the agent's tmux sessions"

printf '== the wrappers, run as the reviewer ==\n'

WRAPPERS="${SANDBOX}/wrappers.sh"
WOUT="${SANDBOX}/wrappers.out"
cat > "$WRAPPERS" <<WRAP
#!/usr/bin/env bash
cd "${DATA}/worktrees/review-nex-1"
git init -q . 2>/dev/null
refused() { local name="\$1"; shift; "\$@" >/dev/null 2>&1; local rc=\$?; if [[ \$rc -ne 0 ]]; then printf '%s=refused\n' "\$name"; else printf '%s=RAN\n' "\$name"; fi; }
M="${DATA}/review/home/marker"
rm -f "\$M"
refused git_upload_pack conductor-review-git fetch "--upload-pack=touch \$M" origin
refused git_config_flag conductor-review-git -c "core.sshCommand=touch \$M" fetch origin
refused git_fsmonitor conductor-review-git -c "core.fsmonitor=touch \$M" status
refused git_push conductor-review-git push origin HEAD
refused cargo_config conductor-review-cargo test --config "target.x86_64-unknown-linux-gnu.runner='touch \$M'"
refused cargo_manifest conductor-review-cargo check --manifest-path /etc/Cargo.toml
refused cargo_build conductor-review-cargo build
refused gh_api conductor-review-gh api repos/o/r/pulls/1
refused gh_patch conductor-review-gh api -X PATCH repos/o/r/pulls/1 -f state=closed
refused gh_merge conductor-review-gh pr merge 1
refused gh_close conductor-review-gh pr close 1
refused gh_edit conductor-review-gh pr edit 1 --title x
refused raw_gh_api gh api repos/o/r/pulls/1
if [[ -e "\$M" ]]; then echo marker=EXISTS; else echo marker=absent; fi
WRAP
chmod 0755 "$WRAPPERS"
tmux -L review-isolation new-session -d -s review-wrappers -x 200 -y 50 -c "${DATA}/worktrees/review-nex-1" \
  conductor-review-launch bash -c "bash '${WRAPPERS}' > '${WOUT}' 2>&1; sleep 30"
for _ in $(seq 1 100); do grep -q '^marker=' "$WOUT" 2>/dev/null && break; sleep 0.2; done
wgot() { sed -n "s/^$1=//p" "$WOUT" | head -1; }
for name in git_upload_pack git_config_flag git_fsmonitor git_push cargo_config cargo_manifest cargo_build gh_api gh_patch gh_merge gh_close gh_edit; do
  [[ "$(wgot "$name")" == "refused" ]] && ok "${name} is refused" || bad "${name} is refused" "$(wgot "$name")"
done
[[ "$(wgot marker)" == "absent" ]] && ok "no injected command ran" || bad "no injected command ran"
tmux -L review-isolation kill-server >/dev/null 2>&1

passed=$( [[ -f "${TALLY}/pass" ]] && wc -l < "${TALLY}/pass" || echo 0 )
failed=$( [[ -f "${TALLY}/fail" ]] && wc -l < "${TALLY}/fail" || echo 0 )
printf '\n%d passed, %d failed\n' "$passed" "$failed"
if [[ "$failed" -ne 0 ]]; then
  printf 'failing checks:\n'
  sed 's/^/  - /' "${TALLY}/fail"
  exit 1
fi
[[ "$passed" -gt 0 ]] || { echo "no checks ran — treating as failure" >&2; exit 1; }
