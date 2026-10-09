#!/usr/bin/env bash
# Regression test for #5817: Postgres is shared by the cluster, so a deploy that
# advances its schema must not leave a peer on a binary that refuses to boot on it.
# On 2026-09-09 the leader migrated first, the peer leg was then refused by the
# peer's resource gate, and the peer crash-looped on the old binary for over an hour.
#
# The production peer functions run for real here; only ssh, the peer health probe
# and the agentdesk binary are faked. A fake shared database (one applied version
# per line) records which migrations each side applied.

# The production functions are loaded through eval and read these settings.
# shellcheck disable=SC2034
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Overridable so a mutation run can point the same assertions at a patched copy.
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-schema-order-test.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

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

# shellcheck source=/dev/null
. "$REPO_ROOT/scripts/_defaults.sh"

for fn in _resolve_deploy_peers _deploy_peer_env_prelude _deploy_to_one_peer \
    _deploy_to_all_peers _wait_for_peer_deploy_verdict _report_peer_verdict_failure \
    _schema_order_peers _schema_order_refusal _cluster_target_refusal \
    _schema_order_guard_before_build \
    _doctor_migration_snapshot _classify_schema_peer_failure \
    _pin_cluster_target_to_built_source _run_schema_peers_first \
    _prepare_release_migrations _finish_cluster_stage _cleanup_on_exit \
    _preserve_staged_binary_for_recovery _migration_floor_artifact_path \
    _spawn_detached_helper _handle_cleanup_signal; do
    body="$(extract_function "$fn")"
    if [ -z "$body" ]; then
        printf 'FAIL: %s is not defined in %s\n' "$fn" "$DEPLOY_SH" >&2
        exit 1
    fi
    eval "$body"
done

# The top-level migration step, from the tunnel migration up to the forward-only
# floor, runs as a function so a refusal returns instead of ending the test.
step_body="$(awk '
    /^_migrate_pg_tunnel_before_release_stop$/ { printing = 1; next }
    /^# Migration 0100 is now a forward-only binary floor/ { exit }
    printing { print }
' "$DEPLOY_SH" | sed -E 's/exit 1$/return 1/')"
case "$step_body" in
    *release-migrate-postgres*) : ;;
    *) printf 'FAIL: the top-level migration step was not found in %s\n' "$DEPLOY_SH" >&2; exit 1 ;;
esac
eval "_release_migration_step() {
$step_body
}"

# --- fixtures ----------------------------------------------------------------
# H1 is what the leader built (two new migrations, 0002 and 0003); H2 lands on
# origin/main afterwards with 0004, which the leader's binary does not embed.
ORIGIN="$TMP_ROOT/origin.git"
REPO="$TMP_ROOT/leader"
PEER="$TMP_ROOT/peer"
git init --quiet --bare -b main "$ORIGIN"
git init --quiet -b main "$REPO"
git -C "$REPO" config user.email t@example.com
git -C "$REPO" config user.name t
git -C "$REPO" remote add origin "$ORIGIN"
mkdir -p "$REPO/migrations/postgres"
commit_migration() {
    : >"$REPO/migrations/postgres/$1"
    git -C "$REPO" add -A
    git -C "$REPO" commit --quiet -m "$1"
    git -C "$REPO" rev-parse HEAD
}
H0="$(commit_migration 0001_a.sql)"
commit_migration 0002_b.sql >/dev/null
H1="$(commit_migration 0003_c.sql)"
H2="$(commit_migration 0004_d.sql)"
git -C "$REPO" push --quiet origin "$H2:refs/heads/main"
git -C "$REPO" reset --quiet --hard "$H1"
git clone --quiet "$ORIGIN" "$PEER"
git -C "$PEER" config user.email t@example.com
git -C "$PEER" config user.name t

# Fake agentdesk: doctor reports pending/applied against the fake database, and
# release-migrate-postgres applies everything it embeds.
FAKE_BIN="$TMP_ROOT/agentdesk.staged"
cat >"$FAKE_BIN" <<'SH'
#!/usr/bin/env bash
case "$1" in
    doctor)
        [ "${FAKE_DOCTOR_BROKEN:-0}" != 1 ] || { echo 'not json'; exit 1; }
        python3 - "$FAKE_DB" "$FAKE_KNOWN" <<'PY'
import json, sys
applied = sorted({int(v) for v in open(sys.argv[1]).read().split()})
known = sorted(int(v) for v in sys.argv[2].split())
evidence = {
    "applied_count": len(applied),
    "resolved_count": len(known),
    "pending_versions": [v for v in known if v not in applied],
}
missing = [v for v in applied if v not in known]
if missing:
    evidence["missing_from_resolved"] = missing
    evidence["unsuccessful_versions"] = []
print(json.dumps({"checks": [{"id": "postgres_connection", "evidence": evidence}]}))
PY
        ;;
    release-migrate-postgres)
        echo "leader-migrate" >>"$FAKE_CALLS"
        for v in $FAKE_KNOWN; do echo "$v" >>"$FAKE_DB"; done
        ;;
esac
SH
chmod +x "$FAKE_BIN"
export FAKE_DB="$TMP_ROOT/db" FAKE_CALLS="$TMP_ROOT/calls" FAKE_KNOWN="1 2 3"

# ssh: pre-sync runs against the peer clone, the port query answers, and the
# deploy applies the migrations of whatever the peer checked out (PEER_APPLIES).
# shellcheck disable=SC2329  # Invoked by the production functions loaded through eval.
ssh() {
    local remote
    remote="${*: -1}"
    remote="${remote#bash -lc }"
    remote="$(eval "printf '%s' $remote")"
    case "$remote" in
        *_extract_yaml_server_port_shell*)
            printf '%s\n%s\n' "$TMP_ROOT/peer-root" 8791
            ;;
        *"git merge"*)
            bash -c "$remote"
            ;;
        *scripts/deploy-release.sh*)
            local head pin version applied=0
            head="$(git -C "$PEER" rev-parse HEAD)"
            pin="$(grep -oE 'AGENTDESK_DEPLOY_TARGET_SHA=[0-9a-f]+' <<<"$remote" || true)"
            echo "peer-deploy head=$head ${pin}" >>"$FAKE_CALLS"
            for version in $(ls "$PEER/migrations/postgres" | sed -E 's/^0*([0-9]+)_.*/\1/'); do
                grep -qx "$version" "$FAKE_DB" && continue
                [ "$PEER_APPLIES" != none ] || break
                echo "$version" >>"$FAKE_DB"
                applied=$((applied + 1))
                [ "$PEER_APPLIES" != first ] || break
            done
            return "${PEER_LAUNCH_RC:-0}"
            ;;
        *) return 1 ;;
    esac
}
# shellcheck disable=SC2329
_probe_peer_deploy_state() {
    printf '%s\tdetail\t%s\trepo\ttrue\thealth\t{}\n' "$PROBE_MARKER" "$(git -C "$PEER" rev-parse HEAD)"
}
# shellcheck disable=SC2329
health_json_is_ready() { return 0; }
# shellcheck disable=SC2329
_migration_floor_may_advance() { [ "${FLOOR_ADVANCES:-1}" = 1 ]; }
# shellcheck disable=SC2329
_cleanup_owned_pg_tunnel_preflight() { :; }
# shellcheck disable=SC2329
_rollback_pg_tunnel_migration() { :; }
# shellcheck disable=SC2329
_rollback_release_binary() { :; }
# shellcheck disable=SC2329
_recover_or_preserve_past_migration_floor() { echo "recover-floor" >>"$FAKE_CALLS"; }
# shellcheck disable=SC2329
_emit_terminal_deploy_marker() { :; }
# shellcheck disable=SC2329
_finalize_detached_helper() { :; }

ADK_REL="$TMP_ROOT/adk-rel"
mkdir -p "$ADK_REL/bin"
DEPLOY_SSH_CONNECT_TIMEOUT=1
DEPLOY_PEER_VERDICT_TIMEOUT_SECS=0
DEPLOY_PEER_VERDICT_POLL_INTERVAL_SECS=1
DEPLOY_PEERS_OVERRIDE=()
DEPLOY_PEERS_FILE="$TMP_ROOT/no-peers-file"
export AGENTDESK_PEER_REPO_DIR="$PEER"

# Resets the fixtures and loads a scenario's defaults plus its KEY=VALUE settings
# into the calling subshell.
reset_fixtures() {
    rm -f "$FAKE_CALLS" "$TMP_ROOT/state" "$ADK_REL/bin/"agentdesk*
    : >"$FAKE_CALLS"
    git -C "$PEER" checkout --quiet main
    git -C "$PEER" reset --quiet --hard "$H0"
    git -C "$REPO" reset --quiet --hard "$H1"
}
load_scenario() {
    DEPLOY_ALL_NODES=1 DEPLOY_PEER_INVOCATION=0 DEPLOY_TARGET_SHA=""
    AGENTDESK_DEPLOY_PEERS=peer-stub PEER_APPLIES=all PEER_LAUNCH_RC=0
    PROBE_MARKER=success FLOOR_ADVANCES=1 DEPLOY_BUILT_SOURCE_SHA="$H1"
    AGENTDESK_DEPLOY_ALLOW_SCHEMA_AHEAD_OF_PEERS=0 AGENTDESK_DEPLOY_BINARY=""
    AGENTDESK_DEPLOY_TARGET_SHA="" MIGRATION_FLOOR_ARMED=0 DEPLOY_OK=0
    SCHEMA_PEERS_FIRST_STATE="" SCHEMA_PEERS_FIRST_FAILED=0
    PEER_LEG_LAUNCHED=0 PEER_VERDICT_MARKER=unknown RUN_FINISH=0 RUN_CLEANUP=0
    DB_INIT=1
    local kv
    for kv in "$@"; do eval "${kv%%=*}=\${kv#*=}"; done
    export AGENTDESK_DEPLOY_PEERS PEER_APPLIES PEER_LAUNCH_RC FAKE_DOCTOR_BROKEN
    tr ' ' '\n' <<<"$DB_INIT" >"$FAKE_DB"
    cp "$FAKE_BIN" "$ADK_REL/bin/agentdesk.deploy.test"
    STAGED_BINARY="$ADK_REL/bin/agentdesk.deploy.test"
}

# Runs the production migration step in a subshell, the way the top level calls
# it, and records what happened. (label, then KEY=VALUE scenario settings)
run_step() {
    local label="$1"
    shift
    reset_fixtures
    (
        load_scenario "$@"
        local rc=0 guard_rc=0 finish_rc=skipped
        _schema_order_guard_before_build >"$TMP_ROOT/guard.out" 2>&1 || guard_rc=$?
        _release_migration_step >"$TMP_ROOT/step.out" 2>&1 || rc=$?
        if [ "$RUN_FINISH" = 1 ] && [ "$rc" = 0 ]; then
            finish_rc=0
            _finish_cluster_stage >>"$TMP_ROOT/step.out" 2>&1 || finish_rc=$?
        fi
        {
            echo "guard_rc=$guard_rc"
            echo "rc=$rc"
            echo "finish_rc=$finish_rc"
            echo "armed=$MIGRATION_FLOOR_ARMED"
            echo "state=$SCHEMA_PEERS_FIRST_STATE"
            echo "peer_first_failed=$SCHEMA_PEERS_FIRST_FAILED"
        } >"$TMP_ROOT/state"
        if [ "$RUN_CLEANUP" = 1 ]; then
            _cleanup_on_exit 1 >>"$TMP_ROOT/step.out" 2>&1
        fi
    ) || true
    [ -s "$TMP_ROOT/state" ] || fail_test "$label: the migration step exited the shell instead of returning: $(cat "$TMP_ROOT/step.out" 2>/dev/null)"
    CURRENT_LABEL="$label"
}
state_is() {
    grep -qx "$1" "$TMP_ROOT/state" 2>/dev/null \
        || fail_test "$CURRENT_LABEL: expected $1; state: $(tr '\n' ' ' <"$TMP_ROOT/state" 2>/dev/null) output: $(cat "$TMP_ROOT/step.out" 2>/dev/null)"
}
calls_count() { grep -c "^$1" "$FAKE_CALLS" || true; }
leader_migrated() { [ "$(calls_count leader-migrate)" -gt 0 ]; }
db_has() { grep -qx "$1" "$FAKE_DB"; }
first_line_of() { grep -n "^$1" "$FAKE_CALLS" | head -1 | cut -d: -f1; }

# --- 1. single-node deploys never advance the schema under a peer -------------
run_step "single node, schema advances (manifest)" DEPLOY_ALL_NODES=0
state_is "guard_rc=1"
state_is "rc=1"
grep -q "rerun with --all-nodes" "$TMP_ROOT/guard.out" || fail_test "$CURRENT_LABEL: the refusal must name --all-nodes: $(cat "$TMP_ROOT/guard.out")"
! leader_migrated || fail_test "$CURRENT_LABEL: the leader migrated with a peer left on the old binary"
[ "$(calls_count peer-deploy)" = 0 ] || fail_test "$CURRENT_LABEL: a single-node deploy deployed the peer"

# The manifest says nothing advances, but the candidate binary still has pending
# migrations: the recheck before migrating must refuse the same way.
run_step "single node, manifest quiet but doctor pending" DEPLOY_ALL_NODES=0 FLOOR_ADVANCES=0
state_is "guard_rc=0"
state_is "rc=1"
! leader_migrated || fail_test "$CURRENT_LABEL: pending migrations reached the shared schema past the recheck"

run_step "single node, nothing pending" DEPLOY_ALL_NODES=0 FLOOR_ADVANCES=0 "FAKE_KNOWN=1"
state_is "rc=0"
leader_migrated || fail_test "$CURRENT_LABEL: a deploy that changes no schema must migrate as before"

run_step "single node, override" DEPLOY_ALL_NODES=0 AGENTDESK_DEPLOY_ALLOW_SCHEMA_AHEAD_OF_PEERS=1
state_is "guard_rc=0"
state_is "rc=0"
leader_migrated || fail_test "$CURRENT_LABEL: the override must keep the old order"

run_step "peer leg itself" DEPLOY_ALL_NODES=0 DEPLOY_PEER_INVOCATION=1
state_is "guard_rc=0"
state_is "rc=0"

run_step "no peers configured" DEPLOY_ALL_NODES=0 AGENTDESK_DEPLOY_PEERS=""
state_is "guard_rc=0"
state_is "rc=0"
leader_migrated || fail_test "$CURRENT_LABEL: a node without peers must migrate as before"

run_step "two peers" "AGENTDESK_DEPLOY_PEERS=peer-a,peer-b"
state_is "guard_rc=1"
state_is "rc=1"
[ "$(calls_count peer-deploy)" = 0 ] || fail_test "$CURRENT_LABEL: a second peer must be refused before any deploy"

run_step "external artifact" "AGENTDESK_DEPLOY_BINARY=$TMP_ROOT/elsewhere"
state_is "rc=1"
! leader_migrated || fail_test "$CURRENT_LABEL: an unprovable artifact advanced the schema"

# --- 2. cluster: the peer deploys before this node migrates ---------------------
run_step "cluster success" RUN_FINISH=1
state_is "rc=0"
state_is "finish_rc=0"
state_is "armed=1"
state_is "state=completed"
peer_line="$(first_line_of peer-deploy)"
leader_line="$(first_line_of leader-migrate)"
if [ -z "$peer_line" ] || [ -z "$leader_line" ] || [ "$peer_line" -gt "$leader_line" ]; then
    fail_test "$CURRENT_LABEL: the peer must deploy before the leader migrates; calls: $(tr '\n' '|' <"$FAKE_CALLS")"
fi
[ "$(calls_count peer-deploy)" = 1 ] || fail_test "$CURRENT_LABEL: the peer deployed $(calls_count peer-deploy) times"

# Only the doctor recheck sees the advance; the peer has migrated, so a later
# abort here must fail forward.
run_step "cluster, manifest quiet but doctor pending" FLOOR_ADVANCES=0
state_is "rc=0"
state_is "armed=1"
state_is "state=completed"

# Nothing advances for the binary built here (H1 embeds and the database holds
# 1-3), but origin/main has moved on to H2 with 0004: the peer still deploys after
# this node, and builds H1, so it never applies a migration this node lacks.
run_step "cluster, schema already current" FLOOR_ADVANCES=0 "DB_INIT=1 2 3" RUN_FINISH=1
state_is "rc=0"
state_is "finish_rc=0"
state_is "state="
peer_line="$(first_line_of peer-deploy)"
leader_line="$(first_line_of leader-migrate)"
if [ -z "$peer_line" ] || [ -z "$leader_line" ] || [ "$peer_line" -lt "$leader_line" ]; then
    fail_test "$CURRENT_LABEL: without a schema change the peer stays after this node; calls: $(tr '\n' '|' <"$FAKE_CALLS")"
fi
case "$(grep '^peer-deploy' "$FAKE_CALLS" | head -1)" in
    *"head=$H1 AGENTDESK_DEPLOY_TARGET_SHA=$H1"*) : ;;
    *) fail_test "$CURRENT_LABEL: the peer must build and be pinned to $H1; calls: $(tr '\n' '|' <"$FAKE_CALLS")" ;;
esac
! db_has 4 || fail_test "$CURRENT_LABEL: the peer applied migration 4, which the leader's binary does not embed"

# An external artifact has no source the peer could build, schema change or not.
run_step "external artifact, schema current" FLOOR_ADVANCES=0 "DB_INIT=1 2 3" RUN_FINISH=1 \
    "AGENTDESK_DEPLOY_BINARY=$TMP_ROOT/elsewhere" DEPLOY_BUILT_SOURCE_SHA=""
state_is "guard_rc=1"
state_is "rc=1"
! leader_migrated || fail_test "$CURRENT_LABEL: this node migrated although the peers cannot be pinned"
[ "$(calls_count peer-deploy)" = 0 ] || fail_test "$CURRENT_LABEL: the peer deployed an unpinned source"

# --- 3. the 2026-09-09 shape: the peer refuses before migrating -----------------
run_step "peer refused before migrating" PEER_APPLIES=none PROBE_MARKER=failure RUN_CLEANUP=1
state_is "rc=1"
state_is "armed=0"
state_is "state=failed"
! leader_migrated || fail_test "$CURRENT_LABEL: the leader migrated after the peer refused"
[ "$(tr '\n' ' ' <"$FAKE_DB")" = "1 " ] || fail_test "$CURRENT_LABEL: the shared schema moved: $(tr '\n' ' ' <"$FAKE_DB")"
grep -q "shared schema is unchanged" "$TMP_ROOT/step.out" || fail_test "$CURRENT_LABEL: the refusal must say the schema is unchanged: $(cat "$TMP_ROOT/step.out")"

# --- 4. the peer applied part of the schema, then failed ------------------------
# pending goes [2,3] -> [3]; pending is still non-empty, yet the old leader binary
# can no longer boot, so this node must finish forward and the run still fails.
run_step "peer applied one of two, then failed" PEER_APPLIES=first PROBE_MARKER=failure RUN_FINISH=1
state_is "rc=0"
state_is "armed=1"
state_is "peer_first_failed=1"
state_is "finish_rc=1"
leader_migrated || fail_test "$CURRENT_LABEL: the leader must finish forward once the schema moved"
[ "$(calls_count peer-deploy)" = 1 ] || fail_test "$CURRENT_LABEL: the failed peer leg was repeated $(calls_count peer-deploy) times"

run_step "peer migrated, verdict timed out" PROBE_MARKER=unknown RUN_FINISH=1
state_is "rc=0"
state_is "armed=1"
state_is "finish_rc=1"

# --- 5. unknown outcome: keep the binary, install nothing ------------------------
run_step "peer still running, schema unmoved" PEER_APPLIES=none PROBE_MARKER=unknown RUN_CLEANUP=1
state_is "rc=1"
state_is "state=unresolved"
state_is "armed=0"
! leader_migrated || fail_test "$CURRENT_LABEL: the leader migrated while the peer outcome was unknown"
[ -e "$ADK_REL/bin/agentdesk.migration-floor-recovery" ] \
    || fail_test "$CURRENT_LABEL: the staged binary must be kept for recovery: $(ls "$ADK_REL/bin")"
[ "$(calls_count recover-floor)" = 0 ] || fail_test "$CURRENT_LABEL: an unknown outcome must not install the staged binary"

# An unreadable schema is never read as "unchanged".
run_step "doctor unreadable" FAKE_DOCTOR_BROKEN=1 PEER_APPLIES=none PROBE_MARKER=failure RUN_CLEANUP=1
state_is "rc=1"
state_is "state=unresolved"
[ -e "$ADK_REL/bin/agentdesk.migration-floor-recovery" ] \
    || fail_test "$CURRENT_LABEL: an unreadable schema must keep the staged binary"


# --- 6. the peer builds exactly the staged source -------------------------------
run_step "origin/main moved past the built source" RUN_FINISH=1
peer_deploy="$(grep '^peer-deploy' "$FAKE_CALLS" | head -1)"
case "$peer_deploy" in
    *"head=$H1 AGENTDESK_DEPLOY_TARGET_SHA=$H1"*) : ;;
    *) fail_test "$CURRENT_LABEL: the peer must build and be pinned to $H1; got '$peer_deploy'" ;;
esac
! db_has 4 || fail_test "$CURRENT_LABEL: the peer applied migration 4, which the leader's binary does not embed"

run_step "workspace moved after the build" "DEPLOY_BUILT_SOURCE_SHA=$H0"
state_is "rc=1"
[ "$(calls_count peer-deploy)" = 0 ] || fail_test "$CURRENT_LABEL: a moved workspace must be refused before the peer deploys"

run_step "pinned target is not the built source" "DEPLOY_TARGET_SHA=$H2"
state_is "rc=1"
[ "$(calls_count peer-deploy)" = 0 ] || fail_test "$CURRENT_LABEL: a conflicting pin must be refused before the peer deploys"

# --- 7. a real TERM around the peer leg ------------------------------------------
# A child shell runs the migration step under the production EXIT/TERM traps and
# receives TERM on the first command after the condition in TERM_WHEN holds.
term_step() {
    local label="$1"
    shift
    reset_fixtures
    printf 'old\n' >"$ADK_REL/bin/agentdesk"
    local rc=0
    (
        set +e
        load_scenario "$@"
        REL_BINARY="$ADK_REL/bin/agentdesk"
        eval "$(extract_function _recover_or_preserve_past_migration_floor)"
        trap _cleanup_on_exit EXIT
        trap '_handle_cleanup_signal 143' TERM
        self_pid="$(exec sh -c 'echo $PPID')"
        depth="$BASH_SUBSHELL"
        fired=0
        set -o functrace
        trap '[ "$fired" = 1 ] || [ "$BASH_SUBSHELL" != "$depth" ] || ! eval "$TERM_WHEN" || { fired=1; kill -TERM "$self_pid"; }' DEBUG
        _release_migration_step
        echo "not interrupted"
    ) >"$TMP_ROOT/step.out" 2>&1 || rc=$?
    CURRENT_LABEL="$label"
    [ "$rc" = 143 ] || fail_test "$label: expected the TERM exit 143, got $rc: $(cat "$TMP_ROOT/step.out")"
    ! grep -q "not interrupted" "$TMP_ROOT/step.out" || fail_test "$label: TERM never fired"
    ! leader_migrated || fail_test "$label: this node migrated after the TERM"
    [ ! -e "$ADK_REL/bin/agentdesk.deploy.test" ] || fail_test "$label: the staged path was left behind"
}
same_as_staged() { cmp -s "$FAKE_BIN" "$1"; }

# Leaving "running" after the peer moved the schema: the floor must already be
# armed, so the binary that boots on the new schema is installed.
LEFT_RUNNING='[ "$SCHEMA_PEERS_FIRST_STATE" = running ] && seen_running=1; [ "${seen_running:-0}" = 1 ] && { [ "$SCHEMA_PEERS_FIRST_STATE" != running ] || [ "$MIGRATION_FLOOR_ARMED" = 1 ]; }'
for scenario in "peer succeeded|PEER_APPLIES=all PROBE_MARKER=success" \
    "peer moved the schema, then failed|PEER_APPLIES=first PROBE_MARKER=failure"; do
    # shellcheck disable=SC2086  # The settings are whitespace-free KEY=VALUE words.
    term_step "TERM as the peer leg settles: ${scenario%%|*}" "TERM_WHEN=$LEFT_RUNNING" ${scenario#*|}
    same_as_staged "$ADK_REL/bin/agentdesk" \
        || fail_test "$CURRENT_LABEL: the staged binary was not installed: $(cat "$TMP_ROOT/step.out")"
    [ "$(cat "$ADK_REL/bin/agentdesk.pre-migration-floor" 2>/dev/null)" = old ] \
        || fail_test "$CURRENT_LABEL: the replaced binary was not kept"
done

# Still inside the peer leg, the schema is unknown: keep the staged binary, install nothing.
term_step "TERM during the peer leg" 'TERM_WHEN=[ "$PEER_LEG_LAUNCHED" = 1 ]'
[ "$(cat "$ADK_REL/bin/agentdesk")" = old ] || fail_test "$CURRENT_LABEL: an unknown outcome installed the staged binary"
same_as_staged "$ADK_REL/bin/agentdesk.migration-floor-recovery" \
    || fail_test "$CURRENT_LABEL: the staged binary must be kept for recovery: $(ls "$ADK_REL/bin")"

# --- 8. the detached relaunch keeps the override ---------------------------------
HELPER_DIR="$TMP_ROOT/helper-scripts"
mkdir -p "$HELPER_DIR"
cat >"$HELPER_DIR/deploy-release.sh" <<'SH'
#!/usr/bin/env bash
echo "override=${AGENTDESK_DEPLOY_ALLOW_SCHEMA_AHEAD_OF_PEERS:-unset}" >"$HELPER_OUT"
SH
chmod +x "$HELPER_DIR/deploy-release.sh"
export HELPER_OUT="$TMP_ROOT/helper.out"
(
    # shellcheck disable=SC2329
    tmux() { printf '%s\n' "${@: -1}" >"$TMP_ROOT/helper-path"; }
    SCRIPT_DIR="$HELPER_DIR" REPORT_CHANNEL_ID=1 REPORT_PROVIDER=claude DEPLOY_DELAY_SECS=0
    DEPLOY_TEST_MODE=0 POST_DEPLOY_SMOKE_SCOPE=full DEPLOY_LOCK_FILE=x DEPLOY_LOCK_TIMEOUT_SECS=1
    BUNDLE_ID=x PLIST_REL=x
    export AGENTDESK_DEPLOY_ALLOW_SCHEMA_AHEAD_OF_PEERS=1
    _spawn_detached_helper >/dev/null 2>&1
)
if [ -s "$TMP_ROOT/helper-path" ]; then
    env -u AGENTDESK_DEPLOY_ALLOW_SCHEMA_AHEAD_OF_PEERS bash "$(cat "$TMP_ROOT/helper-path")" || true
fi
[ "$(cat "$HELPER_OUT" 2>/dev/null)" = "override=1" ] \
    || fail_test "the detached relaunch must keep AGENTDESK_DEPLOY_ALLOW_SCHEMA_AHEAD_OF_PEERS; got '$(cat "$HELPER_OUT" 2>/dev/null)'"

if [ "$failures" -ne 0 ]; then
    printf '%s\n' "test_deploy_schema_peer_order_5817: $failures assertion(s) failed" >&2
    exit 1
fi
printf '%s\n' "test_deploy_schema_peer_order_5817: all assertions passed"
