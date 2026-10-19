#!/usr/bin/env bash
# A peer deploy leg must build, lock, and write its manifest under the peer's own repo
# and root, never under repo/root/lock paths exported by the deploying host.
#
# The real _deploy_to_one_peer runs against an ssh stub that executes each remote
# `bash -lc` script locally under a separate fake peer HOME and login profile. The
# remote deploy command runs a probe in place of the peer's deploy-release.sh: the
# probe is the real script's repo/root/lock resolution block, so the values checked
# are the ones a peer child would compute.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Overridable so a mutation run can point the same assertions at a patched copy.
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-peer-host-paths-test.XXXXXX")
TMP_ROOT="$(cd "$TMP_ROOT" && pwd)"
trap 'rm -rf "$TMP_ROOT"' EXIT
REAL_HOME="$HOME"

failures=0
fail_test() {
    printf 'FAIL: %s\n' "$1" >&2
    failures=$((failures + 1))
}

extract_function() {
    local function_name="$1"
    awk -v start="^${function_name}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$DEPLOY_SH"
}

for fn in _deploy_peer_env_prelude _extract_yaml_server_port_shell _deploy_to_one_peer; do
    body="$(extract_function "$fn")"
    if [ -z "$body" ]; then
        printf 'FAIL: could not extract %s from %s\n' "$fn" "$DEPLOY_SH" >&2
        exit 1
    fi
    eval "$body"
done

# The child's own resolution block, from SCRIPT_DIR through DEPLOY_LOCK_FILE.
resolution_block="$(awk '
    /^SCRIPT_DIR=/ { printing = 1 }
    printing { print }
    printing && /^DEPLOY_LOCK_FILE=/ { exit }
' "$DEPLOY_SH")"
case "$resolution_block" in
    *$'\nADK_REL='*$'\nREPO='*$'\nDEPLOY_LOCK_FILE='*) : ;;
    *) resolution_block="" ;;
esac
case "$(printf '%s\n' "$resolution_block" | tail -n 1)" in
    DEPLOY_LOCK_FILE=*) : ;;
    *)
        printf 'FAIL: could not extract the ADK_REL/REPO/DEPLOY_LOCK_FILE resolution block from %s\n' "$DEPLOY_SH" >&2
        exit 1
        ;;
esac

# Host side: what the detached deploy child has in its environment.
HOST_ROOT="$TMP_ROOT/host-only/root"
HOST_REPO="$TMP_ROOT/host-only/repo"
HOST_LOCK="$HOST_ROOT/runtime/deploy-release.lock"
mkdir -p "$HOST_ROOT/routines" "$HOST_REPO/scripts" "$TMP_ROOT/host-home"
printf '#!/bin/sh\n' >"$HOST_ROOT/routines/routine.sh"
export HOME="$TMP_ROOT/host-home"
export AGENTDESK_DEPLOY_FAST=1
# shellcheck disable=SC2034  # Read by the production functions loaded through eval.
REPO="$HOST_REPO"
# shellcheck disable=SC2034
ADK_REL="$HOST_ROOT"
# shellcheck disable=SC2034
DEPLOY_SSH_CONNECT_TIMEOUT=10
# shellcheck disable=SC2034
DEPLOY_TARGET_SHA=""

LEADER_HEAD="0123456789abcdef0123456789abcdef01234567"
# shellcheck disable=SC2329  # Invoked by the production function.
git() {
    if [ "${3:-}" = "rev-parse" ] && [ "${4:-}" = "HEAD" ]; then
        printf '%s\n' "$LEADER_HEAD"
        return 0
    fi
    printf 'unexpected host git call: %s\n' "$*" >&2
    return 97
}

# shellcheck disable=SC2329
rsync() {
    local dest="${*: -1}"
    printf '%s\n' "$dest" >"$CASE_DIR/rsync-dest"
}

# shellcheck disable=SC2329
_wait_for_peer_deploy_verdict() {
    printf '%s\n' "$@" >"$CASE_DIR/verdict-args"
}

# Every remote script reaches the peer as `bash -lc <quoted script>`; run it through
# a real login bash whose HOME (and so profile, defaults, and paths) is the peer's.
# shellcheck disable=SC2329
ssh() {
    local cmd="${*: -1}"
    case "$cmd" in
        'bash -lc '*) : ;;
        *) printf 'unexpected ssh shape: %s\n' "$cmd" >&2; return 97 ;;
    esac
    env -i HOME="$PEER_HOME" PATH="/usr/bin:/bin" TMPDIR="$TMP_ROOT" "$BASH" -c "$cmd"
}

# Any checkout the peer child might build carries the probe; it records into the
# peer HOME of the running case.
write_peer_checkout() {
    local dir="$1"
    mkdir -p "$dir/scripts"
    cp "$REPO_ROOT/scripts/_defaults.sh" "$dir/scripts/_defaults.sh"
    {
        printf '#!/usr/bin/env bash\nset -euo pipefail\n'
        printf '%s\n' "$resolution_block"
        cat <<'PROBE'
printf '%s\n' "ADK_REL=$ADK_REL" "REPO=$REPO" "DEPLOY_LOCK_FILE=$DEPLOY_LOCK_FILE" \
    "DEPLOY_LOG=${AGENTDESK_DEPLOY_LOG:-}" "PEER_INVOCATION=${AGENTDESK_DEPLOY_PEER_INVOCATION:-}" \
    "DEPLOY_FAST=${AGENTDESK_DEPLOY_FAST:-}" >"$HOME/child.env"
PROBE
    } >"$dir/scripts/deploy-release.sh"
}

write_peer_root() {
    mkdir -p "$1/config"
    printf 'server:\n  port: 9411\n' >"$1/config/agentdesk.yaml"
}

child_value() {
    sed -n "s|^$1=||p" "$PEER_HOME/child.env"
}

# (label, peer login profile body, host AGENTDESK_PEER_REPO_DIR, expected peer root, expected peer repo)
run_case() {
    local label="$1" profile_body="$2" peer_repo_override="$3" want_root="$4" want_repo="$5"
    local rc=0 out
    mkdir -p "$CASE_DIR/bin" "$PEER_HOME"
    # shellcheck disable=SC2016  # $PWD and $PATH expand on the peer, not here.
    printf '#!/bin/sh\nprintf "%%s\\n" "$PWD" >>%q\n' "$CASE_DIR/presync-pwd" >"$CASE_DIR/bin/git"
    chmod +x "$CASE_DIR/bin/git"
    # shellcheck disable=SC2016
    printf 'export PATH=%q:"$PATH"\n%s\n' "$CASE_DIR/bin" "$profile_body" >"$PEER_HOME/.bash_profile"
    if [ -n "$peer_repo_override" ]; then
        export AGENTDESK_PEER_REPO_DIR="$peer_repo_override"
    else
        unset AGENTDESK_PEER_REPO_DIR
    fi

    out="$(_deploy_to_one_peer peer-stub 2>&1)" || rc=$?
    if [ "$rc" -ne 0 ] || [ ! -f "$CASE_DIR/verdict-args" ]; then
        fail_test "$label: peer leg did not reach the verdict (rc=$rc); got: $out"
        return
    fi
    if [ ! -f "$PEER_HOME/child.env" ]; then
        fail_test "$label: the remote deploy command never ran the peer checkout's deploy script; got: $out"
        return
    fi

    local got_root got_repo got_lock got_log verdict_log verdict_manifest verdict_port presync_pwd rsync_dest
    got_root="$(child_value ADK_REL)"
    got_repo="$(child_value REPO)"
    got_lock="$(child_value DEPLOY_LOCK_FILE)"
    got_log="$(child_value DEPLOY_LOG)"
    verdict_log="$(sed -n 2p "$CASE_DIR/verdict-args")"
    verdict_manifest="$(sed -n 3p "$CASE_DIR/verdict-args")"
    verdict_port="$(sed -n 4p "$CASE_DIR/verdict-args")"
    presync_pwd="$(sort -u "$CASE_DIR/presync-pwd" 2>/dev/null || true)"
    rsync_dest="$(cat "$CASE_DIR/rsync-dest" 2>/dev/null || true)"

    [ "$verdict_port" = "9411" ] \
        || fail_test "$label: the root/port read did not run against the peer's config; port '$verdict_port'"
    [ "$got_root" = "$want_root" ] \
        || fail_test "$label: peer child root must be the peer's ($want_root); got '$got_root'"
    [ "$verdict_manifest" = "$got_root/runtime/release-source.json" ] \
        || fail_test "$label: verdict reads '$verdict_manifest' but the child writes its manifest under '$got_root'"
    [ "$got_repo" = "$want_repo" ] \
        || fail_test "$label: peer child must build the peer checkout ($want_repo); got '$got_repo'"
    [ "$presync_pwd" = "$want_repo" ] \
        || fail_test "$label: pre-sync fast-forwarded '$presync_pwd', not the checkout the child builds ($want_repo)"
    [ "$got_lock" = "$want_root/runtime/deploy-release.lock" ] \
        || fail_test "$label: peer child lock must live under the peer root; got '$got_lock'"
    [ -n "$got_log" ] && [ "$got_log" = "$verdict_log" ] \
        || fail_test "$label: child log '$got_log' is not the one the verdict polls ('$verdict_log')"
    [ "$rsync_dest" = "peer-stub:$want_root/routines/" ] \
        || fail_test "$label: routines must sync to the peer root; got '$rsync_dest'"
    # The prelude must still carry the operator settings, so an empty prelude cannot pass.
    [ "$(child_value PEER_INVOCATION)" = "1" ] && [ "$(child_value DEPLOY_FAST)" = "1" ] \
        || fail_test "$label: forwarded settings did not reach the peer child; got: $(tr '\n' ' ' <"$PEER_HOME/child.env")"
    local reached
    reached="$(cat "$PEER_HOME/child.env") $presync_pwd $rsync_dest"
    case "$reached" in
        *"$TMP_ROOT/host-only"*) fail_test "$label: a host-only path reached the peer leg" ;;
    esac
    case "$TMP_ROOT/" in
        "$REAL_HOME/"*) : ;;
        *) case "$reached" in
            *"$REAL_HOME/"*) fail_test "$label: a path under the real HOME reached the peer leg" ;;
        esac ;;
    esac
}

new_case() {
    CASE_DIR="$TMP_ROOT/case-$1"
    PEER_HOME="$CASE_DIR/peer-home"
    mkdir -p "$CASE_DIR"
}

# Host env as the detached child exports it: host repo, lock, and an explicit root.
export AGENTDESK_ROOT_DIR="$HOST_ROOT"
export AGENTDESK_REPO_DIR="$HOST_REPO"
export AGENTDESK_DEPLOY_LOCK_FILE="$HOST_LOCK"

new_case defaults
root="$PEER_HOME/.adk/release"
write_peer_root "$root"
write_peer_checkout "$root/workspaces/agentdesk"
write_peer_checkout "$HOST_REPO"
run_case "peer login env empty" "" "" "$root" "$root/workspaces/agentdesk"

new_case login-root
root="$CASE_DIR/peer-custom-root"
write_peer_root "$root"
write_peer_checkout "$root/workspaces/agentdesk"
run_case "peer login env sets its root" "export AGENTDESK_ROOT_DIR=$(printf '%q' "$root")" "" \
    "$root" "$root/workspaces/agentdesk"

new_case explicit-repo
root="$PEER_HOME/.adk/release"
write_peer_root "$root"
write_peer_checkout "$CASE_DIR/peer-explicit-repo"
run_case "AGENTDESK_PEER_REPO_DIR set" "" "$CASE_DIR/peer-explicit-repo" \
    "$root" "$CASE_DIR/peer-explicit-repo"

new_case explicit-repo-vs-login
root="$PEER_HOME/.adk/release"
write_peer_root "$root"
write_peer_checkout "$CASE_DIR/peer-explicit-repo"
write_peer_checkout "$CASE_DIR/peer-login-repo"
run_case "AGENTDESK_PEER_REPO_DIR set, peer login env names another repo" \
    "export AGENTDESK_REPO_DIR=$(printf '%q' "$CASE_DIR/peer-login-repo")" "$CASE_DIR/peer-explicit-repo" \
    "$root" "$CASE_DIR/peer-explicit-repo"

# Same user and layout on both nodes: forwarded and peer-resolved values coincide.
new_case same-home
root="$PEER_HOME/.adk/release"
write_peer_root "$root"
write_peer_checkout "$root/workspaces/agentdesk"
export AGENTDESK_ROOT_DIR="$root"
export AGENTDESK_REPO_DIR="$root/workspaces/agentdesk"
export AGENTDESK_DEPLOY_LOCK_FILE="$root/runtime/deploy-release.lock"
run_case "same HOME layout on host and peer" "" "" "$root" "$root/workspaces/agentdesk"

if [ "$failures" -ne 0 ]; then
    printf '%s\n' "test_cluster_deploy_peer_host_paths_6162: $failures assertion(s) failed" >&2
    exit 1
fi

printf '%s\n' "test_cluster_deploy_peer_host_paths_6162: all assertions passed"
