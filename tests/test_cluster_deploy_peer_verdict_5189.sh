#!/usr/bin/env bash
# Regression test for #5189, part 1: EVERY deploy run must end its transcript on a
# terminal marker, and that marker must be the LAST thing the run prints.
#
# What actually happened (from deploy-release.93281.log): a cluster peer refused
# promotion at the restart persistence gate and ended its log with NO terminal
# marker at all, because the `DEPLOY FAILED` echo was gated behind the
# detached-helper branch. A peer leg is neither a detached child nor report-channel
# bound, so nothing was printed and no log-based verdict had anything to match.
#
# The gate's refusal was CORRECT ("the in-flight delivery frontier is not durable").
# The defect is that the refusal never reached the report. §1 pins the marker onto
# every non-zero exit. §2 pins its POSITION: the script hands the operator a polling
# command built on `grep -qm1`, so a success marker printed before the cluster stage
# is read as the verdict for a deploy that has judged no peer at all.
#
# Later parts of this stack judge the peers themselves; this file is the contract
# they report through.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Overridable so a mutation run can point the same assertions at a patched copy.
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-cluster-verdict-test.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

extract_function() {
    local function_name="$1"
    awk -v start="^${function_name}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$DEPLOY_SH"
}

# The peer verdict's health axis is the shared readiness predicate, so the real
# one has to be in scope here exactly as deploy-release.sh has it in scope.
# Loading a copy would let this file go green against a predicate the deploy does
# not use, which is the class of split this section exists to close.
# shellcheck source=/dev/null
. "$REPO_ROOT/scripts/_defaults.sh"

# Exercise the production functions without executing the deploy script.
eval "$(extract_function _emit_terminal_deploy_marker)"
eval "$(extract_function _report_peer_verdict_failure)"
eval "$(extract_function _wait_for_peer_deploy_verdict)"

failures=0
fail_test() {
    printf 'FAIL: %s\n' "$1" >&2
    failures=$((failures + 1))
}

# --- 1. every failing run must leave a terminal marker --------------------
# The peer's log ended with no marker precisely because this echo was gated on
# the detached-helper branch. A peer leg is neither detached-child nor
# report-channel bound.
DEPLOY_DETACHED_CHILD=0
REPORT_CHANNEL_ID=""
export DEPLOY_DETACHED_CHILD REPORT_CHANNEL_ID
marker_out="$(_emit_terminal_deploy_marker 1)"
case "$marker_out" in
    *"DEPLOY FAILED (exit=1)"*) : ;;
    *) fail_test "a non-zero exit must print a terminal DEPLOY FAILED marker even outside the detached helper; got '$marker_out'" ;;
esac
marker_out="$(_emit_terminal_deploy_marker 0)"
if [ -n "$marker_out" ]; then
    fail_test "a successful exit must not print a failure marker; got '$marker_out'"
fi
# The emitter is only half the contract — the EXIT path has to CALL it. Asserting the
# function in isolation leaves "delete the call site" undetected, and that restores
# the original defect exactly: a marker nothing invokes is a silent non-zero exit.
cleanup_body="$(extract_function _cleanup_on_exit)"
if [ -z "$cleanup_body" ]; then
    fail_test "could not extract _cleanup_on_exit from $DEPLOY_SH"
fi
case "$cleanup_body" in
    *_emit_terminal_deploy_marker*) : ;;
    *) fail_test "_cleanup_on_exit must emit the terminal marker; a marker function nothing calls leaves every non-zero exit silent" ;;
esac

# --- 2. the terminal marker must be the LAST thing a run prints ----------
# The script hands the operator a polling command built on `grep -qm1`, which stops
# at the FIRST match. While the success marker was printed BEFORE the cluster stage,
# that command locked onto it and reported success for a deploy that had not judged a
# single peer — #5189's own defect on a second path. The window is seconds today and
# grows to the 10-25 minutes a peer leg takes once peers deploy in the ssh foreground.
marker_line="$(grep -n '^echo "═══ Deploy Complete ═══"$' "$DEPLOY_SH" | head -1 | cut -d: -f1)"
cluster_line="$(grep -n '^    _deploy_to_all_peers "\$@"$' "$DEPLOY_SH" | head -1 | cut -d: -f1)"
if [ -z "$marker_line" ] || [ -z "$cluster_line" ]; then
    fail_test "could not locate the terminal marker echo and the cluster-deploy call in $DEPLOY_SH"
elif [ "$marker_line" -lt "$cluster_line" ]; then
    fail_test "the success marker (line $marker_line) is printed BEFORE the cluster stage (line $cluster_line) — the advised poll will report it as the verdict while peers are still being judged"
fi

# The behavioural half: run the polling command this script actually prints, against
# a log that is still GROWING, and require it to wait for the cluster verdict.
run_bounded() {
    # (deadline_secs, out_path, command-string) -> rc, 124 on deadline.
    local deadline="$1" out="$2" cmd="$3"
    ( eval "$cmd" ) > "$out" 2>&1 &
    local pid=$! waited=0
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$waited" -ge "$deadline" ]; then
            kill -9 "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            return 124
        fi
        sleep 1
        waited=$((waited + 1))
    done
    wait "$pid" 2>/dev/null || return $?
    return 0
}

poll_stmt="$(grep -F 'until [ -f ' "$DEPLOY_SH" | head -1)"
if [ -z "$poll_stmt" ]; then
    fail_test "could not find the one-shot wait command the script prints for the operator"
else
    log_path="$TMP_ROOT/helper-growing.log"
    # shellcheck disable=SC2034  # log_path is expanded by the extracted echo statement
    poll_cmd="$(eval "$poll_stmt" | sed -E 's/^[[:space:]]+//')"
    # Pre-cluster output including a per-peer ✓ line, which really does quote the
    # marker (PEER_DEPLOY_VERDICT carries it). An unanchored match takes it as the verdict.
    {
        printf '%s\n' '✓ Post-deploy functional smoke passed'
        printf '%s\n' '═══ Cluster Deploy → Peers ═══'
        printf '%s\n' '  ✓ mac-air — ═══ Deploy Complete ═══; repo_head 4ee96e55e'
    } > "$log_path"
    (
        sleep 4
        {
            printf '%s\n' '✗ Cluster deploy: 1/2 peer(s) did not prove promotion: mac-mini'
            printf '%s\n' '═══ DEPLOY FAILED (exit=1) ═══'
        } >> "$log_path"
    ) &
    writer_pid=$!
    poll_rc=0
    run_bounded 40 "$TMP_ROOT/poll.out" "$poll_cmd" || poll_rc=$?
    wait "$writer_pid" 2>/dev/null || true
    if [ "$poll_rc" -eq 124 ]; then
        fail_test "the advised polling command never terminated on a log that reached a terminal marker"
    elif ! grep -q '═══ DEPLOY FAILED (exit=1) ═══' "$TMP_ROOT/poll.out"; then
        fail_test "an operator polling exactly as instructed must be handed the cluster verdict; got: $(cat "$TMP_ROOT/poll.out")"
    fi
    if grep -q 'repo_head 4ee96e55e' "$TMP_ROOT/poll.out"; then
        fail_test "the polling command matched a line that merely QUOTES the terminal marker — it must match terminal lines only"
    fi
fi

# --- 3. peer completion requires an observed three-axis verdict ------------
# Stub the peer probe so these checks cannot reach SSH or a live release API.
# shellcheck disable=SC2034  # Read by the production function loaded through eval.
DEPLOY_PEER_VERDICT_TIMEOUT_SECS=30
# shellcheck disable=SC2034  # Read by the production function loaded through eval.
DEPLOY_PEER_VERDICT_POLL_INTERVAL_SECS=1
# shellcheck disable=SC2329  # Invoked indirectly by the production wait function.
_probe_peer_deploy_state() {
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
        success '═══ Deploy Complete ═══' stale-head read true 'ok=true, status=healthy'
}

peer_verdict_rc=0
peer_verdict_out="$(_wait_for_peer_deploy_verdict \
    peer-stub /stub/deploy.log /stub/release-source.json 8791 target-head 2>&1)" \
    || peer_verdict_rc=$?
if [ "$peer_verdict_rc" -eq 0 ]; then
    fail_test "a terminal marker and healthy API must still be red when repo_head differs from the deploy target"
elif ! grep -q 'repo head does not match the deploy target' <<<"$peer_verdict_out"; then
    fail_test "a repo_head mismatch must identify the failing verdict axis; got: $peer_verdict_out"
fi

# shellcheck disable=SC2034  # Read by the production function loaded through eval.
DEPLOY_PEER_VERDICT_TIMEOUT_SECS=0
# shellcheck disable=SC2329  # Invoked indirectly by the production wait function.
_probe_peer_deploy_state() {
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
        missing 'no terminal marker' unavailable 'manifest unavailable' false 'request failed: stub'
}

peer_verdict_rc=0
peer_verdict_out="$(_wait_for_peer_deploy_verdict \
    peer-stub /stub/deploy.log /stub/release-source.json 8791 target-head 2>&1)" \
    || peer_verdict_rc=$?
if [ "$peer_verdict_rc" -eq 0 ]; then
    fail_test "a peer verdict timeout must be red"
elif ! grep -q 'timed out after 0s' <<<"$peer_verdict_out"; then
    fail_test "a timeout must be reported as the verdict failure reason; got: $peer_verdict_out"
elif ! grep -q 'terminal marker: missing' <<<"$peer_verdict_out" \
    || ! grep -q 'repo head: expected=target-head observed=unavailable' <<<"$peer_verdict_out" \
    || ! grep -q 'health: ok=false' <<<"$peer_verdict_out"; then
    fail_test "a timeout must report marker, repo head, and health observations; got: $peer_verdict_out"
fi

# --- 3d. the health axis must be the deploy's own readiness verdict ---------
# Measured on the mac-mini peer (2026-08-18) once the standby reconcile settled:
# the node reached its INTENDED shape -- degraded:true, every degraded_reason a
# `provider:<name>:gateway_standby`, cluster_standby:true, fully_recovered:true --
# and the verdict tested `health.get("ok") is True`. A standby node's correct
# answer to `ok` is false, so that test could not go green on one at any point in
# the 1800s timeout; the peer leg spent the whole window and reported
# "health: ok=false (status=degraded)" about a node that was exactly where the
# deploy had put it. health_json_is_ready -- the predicate the local restart gate
# already waits on, via its cluster_standby branch -- reads the same body as
# ready. The defect was two consumers of one judgement, so the fix is the verdict
# calling that predicate, and these cases pin it there.
#
# The body is the observed one rather than a modelled shape -- what made the split
# invisible is precisely that a body can be ready and NOT ok, and a reduction of
# the body is exactly where that stops being reproducible. This is the captured
# /api/health response in full, every top-level key it carried, so the jq-less
# reason scan runs over the real two-element degraded_reasons array (#5071 S0b r2
# F1) and the nested objects the real body carries are really present.
STANDBY_READY_BODY='{"auto_queue_cleanup":{"dead_lettered":0,"pending":0},"cluster_standby":true,"dashboard":true,"db":true,"degraded":true,"degraded_reasons":["provider:codex:gateway_standby","provider:claude:gateway_standby"],"delivery_record_rollout":{"authority_enabled":false,"configuration_warnings":["delivery_record_authority_disabled: durable frontiers are not the default committed-offset authority"],"dedup_authority":"in_memory_committed_offset","mode":"off","same_turn_backward_write_enforcement":"observe_only","shadow_enabled":false,"warning_count":1},"fully_recovered":true,"intake_routing":{"configuration_warnings":[],"enabled":true,"env_override":null,"mode":"enforce","owner_authority_allowlist_size":0,"owner_authority_config_state":"known","recent_decision_count":0,"source":"yaml","yaml":{"enabled":true,"forward_pre_claim_timeout_secs":12,"mode":"enforce","owner_authority_allowlist_size":0,"retry_authorization_secs":300,"stale_claim_recovery_secs":60}},"latest_startup_doctor":{"available":true,"completed_at":"2026-08-18T16:15:02.727483+09:00","detail_endpoint":"/api/doctor/startup/latest","doctor_status":"failed","failed_count":1,"skipped":false,"skipped_reason":null,"started_at":"2026-08-18T16:15:00.613767+09:00","summary":{"failed":1,"passed":28,"total":33,"warned":4},"warned_count":4},"ok":false,"release_source":{"deployed_latest_postgres_migration":"0110_auto_queue_cleanup_tasks_card_rollback.sql","deployed_repo_head":"40bb08d9806b11d6ec6f830fef93db54bc9e30fe","generated_at":"2026-08-18T07:15:03Z","observation_status":"observed"},"server_up":true,"startup_degraded":true,"startup_degraded_reasons":["startup_doctor_failed:1","startup_doctor_warned:4"],"startup_status":"doctor_failed","status":"degraded","version":"0.1.2"}'
# Serving, but genuinely not deploy-ready: unhealthy with providers present and no
# standby role to explain it. Raw `ok` is false here too, which is the point --
# the two bodies are told apart by the predicate, not by `ok`.
NOT_READY_BODY='{"ok":false,"status":"unhealthy","version":"0.1.2","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"degraded_reasons":["provider:codex:disconnected"],"startup_status":"doctor_passed"}'
HEALTHY_READY_BODY='{"ok":true,"status":"healthy","version":"0.1.2","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":false,"degraded_reasons":[],"startup_status":"doctor_passed"}'
# The OTHER direction of the ok/ready split, and the one the standby cases cannot
# show: `ok` is TRUE while the readiness predicate REFUSES. health_json_is_ready
# never reads `ok` at all -- it requires `db` before anything else -- so a node
# claiming ok=true with its database gone is not deploy-ready. Without this case
# every body in this file that the predicate rejects also has ok=false, and an
# implementation that simply forwarded `ok` would pass the whole section.
OK_TRUE_NOT_READY_BODY='{"ok":true,"status":"healthy","version":"0.1.2","db":false,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":false,"degraded_reasons":[],"startup_status":"doctor_passed"}'

# A zero timeout leaves the success path as the only way to rc=0: the readiness
# check is reached before the deadline check, so a green result here cannot come
# from the loop simply not having given up yet.
# shellcheck disable=SC2034  # Read by the production function loaded through eval.
DEPLOY_PEER_VERDICT_TIMEOUT_SECS=0

probe_stub_body=""
probe_stub_ok="false"
probe_stub_detail=""
# shellcheck disable=SC2329  # Invoked indirectly by the production wait function.
_probe_peer_deploy_state() {
    # Emits the body as the trailing field, as the production probe does; the
    # empty-body cases below leave it off entirely.
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        success '═══ Deploy Complete ═══' target-head read \
        "$probe_stub_ok" "$probe_stub_detail" "$probe_stub_body"
}

run_peer_verdict() {
    # (-> rc, output on stdout) same fixture on every call; only the stub varies.
    local rc=0 out
    out="$(_wait_for_peer_deploy_verdict \
        peer-stub /stub/deploy.log /stub/release-source.json 8791 target-head 2>&1)" || rc=$?
    printf '%s\n' "$rc" "$out"
}

probe_stub_ok="false"
probe_stub_detail='ok=false, status=degraded'
probe_stub_body="$STANDBY_READY_BODY"
standby_verdict="$(run_peer_verdict)"
standby_rc="${standby_verdict%%$'\n'*}"
standby_out="${standby_verdict#*$'\n'}"
if [ "$standby_rc" -ne 0 ]; then
    fail_test "a peer settled in the intended gateway_standby shape is deploy-ready by health_json_is_ready, so its verdict must be green; got rc=$standby_rc: $standby_out"
elif ! grep -q 'deploy verified' <<<"$standby_out"; then
    fail_test "a green standby verdict must say so; got: $standby_out"
fi
# Observability: the verdict must keep BOTH axes, because on this body they
# disagree and only the pair distinguishes a ready standby from a broken node.
if ! grep -q 'ready=true' <<<"$standby_out" || ! grep -q 'ok=false' <<<"$standby_out"; then
    fail_test "the standby verdict must report the raw ok AND the readiness judgement; got: $standby_out"
fi

probe_stub_body="$HEALTHY_READY_BODY"
probe_stub_ok="true"
probe_stub_detail='ok=true, status=healthy'
healthy_verdict="$(run_peer_verdict)"
healthy_rc="${healthy_verdict%%$'\n'*}"
if [ "$healthy_rc" -ne 0 ]; then
    fail_test "an ordinary healthy peer must still be verified green; got rc=$healthy_rc: ${healthy_verdict#*$'\n'}"
fi

probe_stub_body="$NOT_READY_BODY"
probe_stub_ok="false"
probe_stub_detail='ok=false, status=unhealthy'
not_ready_verdict="$(run_peer_verdict)"
not_ready_rc="${not_ready_verdict%%$'\n'*}"
not_ready_out="${not_ready_verdict#*$'\n'}"
if [ "$not_ready_rc" -eq 0 ]; then
    fail_test "a peer the readiness predicate rejects must stay red -- the standby allowance must not widen into any not-ok body"
elif ! grep -q 'ready=false' <<<"$not_ready_out"; then
    fail_test "a red health axis must report ready=false; got: $not_ready_out"
fi

# Fail-closed on a body that never arrived: an unreachable or unparseable
# /api/health leaves the field empty, and `ok` alone must not be able to rescue
# it. `ok=true` here is what makes this discriminating -- it is the exact input a
# reintroduced ok-based shortcut would turn green.
probe_stub_body=""
probe_stub_ok="true"
probe_stub_detail='ok=true, status=healthy'
no_body_verdict="$(run_peer_verdict)"
no_body_rc="${no_body_verdict%%$'\n'*}"
if [ "$no_body_rc" -eq 0 ]; then
    fail_test "a verdict with no health body must fail closed even when the probe reported ok=true; got: ${no_body_verdict#*$'\n'}"
fi

# ok=true with a body the predicate refuses. The standby cases pin that ready can
# outrun ok; this pins the converse, so the health axis is the predicate in both
# directions rather than a relabelled `ok`.
probe_stub_body="$OK_TRUE_NOT_READY_BODY"
probe_stub_ok="true"
probe_stub_detail='ok=true, status=healthy'
ok_true_verdict="$(run_peer_verdict)"
ok_true_rc="${ok_true_verdict%%$'\n'*}"
ok_true_out="${ok_true_verdict#*$'\n'}"
if [ "$ok_true_rc" -eq 0 ]; then
    fail_test "a body the readiness predicate refuses must stay red even when it claims ok=true; got: $ok_true_out"
elif ! grep -q 'ready=false' <<<"$ok_true_out" || ! grep -q 'ok=true' <<<"$ok_true_out"; then
    fail_test "the ok=true/not-ready verdict must report both axes as observed; got: $ok_true_out"
fi

# The allow flags must be the ones the local deploy readiness wait uses. A
# verdict judged by the same predicate under DIFFERENT flags is the same split
# again, one argument further down.
verdict_body="$(extract_function _wait_for_peer_deploy_verdict)"
case "$verdict_body" in
    *'health_json_is_ready "$health_body" 1 1 1'*) : ;;
    *) fail_test "the peer verdict must call health_json_is_ready with the same allow flags as the local readiness wait (require_dashboard, allow_reconcile_degraded, allow_no_provider_runtimes)" ;;
esac
if ! grep -q 'wait_for_http_service_health "$PLIST_REL" "$REL_PORT" "$DEPLOY_HEALTH_RETRIES" "$DEPLOY_HEALTH_DELAY_SECS" 1 1 1' "$DEPLOY_SH"; then
    fail_test "the local release readiness wait no longer uses flags 1 1 1 -- the peer verdict above is now judging by a different standard than the deploy it gates"
fi

# --- 3e. the health body must come from the deploy that wrote the marker ------
# #5071 S0b r2 F2. The verdict has three axes and they have to describe ONE
# thing. The marker and the repo head are read out of the target deploy (its log,
# its manifest), but the body used to be fetched on a port the deploy host had
# pre-read from the peer config over a separate ssh. Nothing tied that port to
# the deploy: a process left listening from an earlier release, or a config
# edited between the pre-read and the probe, supplies a ready body while the
# marker and head come from the run under judgement, and the composite reads as a
# verified deploy that was never observed. The port now comes out of the SAME log
# the marker comes from -- one deploy runs whole transcript, since the script
# re-execs itself detached with stdout redirected to AGENTDESK_DEPLOY_LOG.
probe_body="$(extract_function _probe_peer_deploy_state)"
if [ -z "$probe_body" ]; then
    fail_test "could not extract _probe_peer_deploy_state from $DEPLOY_SH"
fi
case "$probe_body" in
    *'Waiting for release health on :([0-9]+)'*) : ;;
    *) fail_test "the probe must read the health port out of the peer deploy log; without it the body is not bound to the deploy that wrote the marker" ;;
esac
case "$probe_body" in
    *'{health_port_from_log}/api/health'*) : ;;
    *) fail_test "the probe must request the port it parsed from the deploy log" ;;
esac
case "$probe_body" in
    *'{health_port}/api/health'*) fail_test "the probe must not request the pre-read config port -- falling back to it reopens the unbound composite for exactly the runs where the two ports disagree" ;;
    *) : ;;
esac
# The parser and the line it parses are one contract across two places in this
# file. If the deploy stops printing that exact line, the parse silently never
# matches and every peer verdict fails closed forever -- green tests, dead deploy.
if ! grep -qF 'echo "▸ Waiting for release health on :${REL_PORT}..."' "$DEPLOY_SH"; then
    fail_test "the deploy no longer prints the health port line the peer probe parses -- the two must change together"
fi

# The behavioural half: run the PRODUCTION probe. Everything above this section
# stubs _probe_peer_deploy_state wholesale, so the probe body itself -- the log
# scan, the port choice, the tab framing, the trailing body field -- has never
# been executed by a test. Here ssh is replaced with local execution (the probe
# hands ssh one `bash -lc <quoted>` string, so running that string locally is the
# same command the peer would run) and /api/health is served by real HTTP
# servers, which is what makes the port axis observable at all.
if ! command -v python3 >/dev/null 2>&1; then
    printf 'NOTE: python3 absent -- 3e executed-probe cases SKIPPED (structural pins above still ran)\n' >&2
else
    STUB_SERVER_PIDS=""
    trap 'kill $STUB_SERVER_PIDS 2>/dev/null || true; rm -rf "$TMP_ROOT"' EXIT

    cat >"$TMP_ROOT/serve.py" <<'PY'
import http.server
import sys

body = open(sys.argv[1], "rb").read()


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/api/health":
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
print(server.server_port, flush=True)
server.serve_forever()
PY

    start_health_stub() {
        # (body_string, name) -> echoes the bound port. Port 0 lets the kernel
        # pick, so these cases never race a hardcoded port.
        local body="$1" name="$2"
        local port_file="$TMP_ROOT/$name.port" waited=0 port=""
        printf '%s' "$body" >"$TMP_ROOT/$name.json"
        python3 "$TMP_ROOT/serve.py" "$TMP_ROOT/$name.json" >"$port_file" 2>/dev/null &
        STUB_SERVER_PIDS="$STUB_SERVER_PIDS $!"
        while [ "$waited" -lt 100 ]; do
            port="$(head -1 "$port_file" 2>/dev/null || true)"
            [ -z "$port" ] || break
            sleep 0.1
            waited=$((waited + 1))
        done
        printf '%s' "$port"
    }

    write_peer_log() {
        # (port, path) -- a peer transcript shaped like the real one: the health
        # port line the deploy prints, then the terminal marker.
        {
            printf '%s\n' '▸ Promoting release binary...'
            printf '%s\n' "▸ Waiting for release health on :${1}..."
            printf '%s\n' '✓ Release health check passed'
            printf '%s\n' '═══ Deploy Complete ═══'
        } >"$2"
    }

    READY_PORT="$(start_health_stub "$STANDBY_READY_BODY" ready)"
    NOT_READY_PORT="$(start_health_stub "$OK_TRUE_NOT_READY_BODY" notready)"
    if [ -z "$READY_PORT" ] || [ -z "$NOT_READY_PORT" ]; then
        fail_test "the local /api/health stub servers did not report a bound port"
    else
        printf '{"repo_head":"target-head"}' >"$TMP_ROOT/release-source.json"
        write_peer_log "$READY_PORT" "$TMP_ROOT/peer-ready.log"
        write_peer_log "$NOT_READY_PORT" "$TMP_ROOT/peer-notready.log"
        {
            printf '%s\n' '▸ Promoting release binary...'
            printf '%s\n' '═══ Deploy Complete ═══'
        } >"$TMP_ROOT/peer-noport.log"

        # The real probe, and the real wait loop calling it.
        eval "$(extract_function _probe_peer_deploy_state)"
        # shellcheck disable=SC2034  # Read by the production probe loaded through eval.
        DEPLOY_SSH_CONNECT_TIMEOUT=10
        # shellcheck disable=SC2329  # Invoked indirectly by the production probe.
        ssh() {
            # The probe ends its argument list with `bash -lc <%q-quoted script>`.
            # Run that script here instead of on a peer. `-c` rather than `-lc`:
            # a login shell would print profile output into the tab-delimited
            # channel this test is checking.
            local cmd="${*: -1}"
            cmd="${cmd#bash -lc }"
            eval "bash -c $cmd"
        }

        run_probed_verdict() {
            # (log_path, pre_read_port) -> rc on line 1, output after.
            local rc=0 out
            out="$(_wait_for_peer_deploy_verdict \
                peer-stub "$1" "$TMP_ROOT/release-source.json" "$2" target-head 2>&1)" || rc=$?
            printf '%s\n' "$rc" "$out"
        }

        # (a) The log names the port the target deploy health-checked, and the
        # pre-read names a DIFFERENT one. Green -- and green through the real
        # probe, which is what makes the emptied-body mutation
        # (`fields.append("")`) fail here rather than pass unnoticed.
        bound_verdict="$(run_probed_verdict "$TMP_ROOT/peer-ready.log" "$NOT_READY_PORT")"
        bound_rc="${bound_verdict%%$'\n'*}"
        bound_out="${bound_verdict#*$'\n'}"
        if [ "$bound_rc" -ne 0 ]; then
            fail_test "the production probe must carry back the body from the port the deploy log names; got rc=$bound_rc: $bound_out"
        elif ! grep -q 'deploy verified' <<<"$bound_out"; then
            fail_test "a peer whose logged port serves a ready body must be verified; got: $bound_out"
        fi

        # (b) The discriminating case, and the P1 shape itself: a ready body IS
        # reachable, on the pre-read port -- but the deploy under judgement
        # health-checked the other one, and that one is not ready. The old
        # composite went GREEN here off a body belonging to no observed deploy.
        # The not-ready body also reports ok=true, so neither the pre-read port
        # nor `ok` can rescue it.
        unbound_verdict="$(run_probed_verdict "$TMP_ROOT/peer-notready.log" "$READY_PORT")"
        unbound_rc="${unbound_verdict%%$'\n'*}"
        unbound_out="${unbound_verdict#*$'\n'}"
        if [ "$unbound_rc" -eq 0 ]; then
            fail_test "a ready body on the PRE-READ port must not verify a deploy that health-checked a different port; got: $unbound_out"
        elif ! grep -q "port=$NOT_READY_PORT" <<<"$unbound_out"; then
            fail_test "the verdict must report the port it actually requested (the logged one, $NOT_READY_PORT); got: $unbound_out"
        fi

        # (c) No port in the log at all: fail closed. A ready body is reachable
        # on the pre-read port, so this is exactly the input a reintroduced
        # fallback would turn green.
        noport_verdict="$(run_probed_verdict "$TMP_ROOT/peer-noport.log" "$READY_PORT")"
        noport_rc="${noport_verdict%%$'\n'*}"
        noport_out="${noport_verdict#*$'\n'}"
        if [ "$noport_rc" -eq 0 ]; then
            fail_test "a deploy log naming no health port must fail closed, not fall back to the pre-read port; got: $noport_out"
        elif ! grep -q 'health port not named in the peer deploy log' <<<"$noport_out"; then
            fail_test "an unbindable health port must be named as the reason; got: $noport_out"
        fi
    fi
fi

# --- 3f. the health body must not survive the round that fetched it -----------
# #5071 S0b r2 F3. `read` clears the trailing variables it has no fields for, so
# the per-round `health_body=""` reset is not what protects the SUCCESSFUL probe
# path -- it is what protects the FAILED one, where `read` never runs and every
# variable keeps the previous round's value. The marker is reset to "unknown"
# there, so a stale body cannot turn the verdict green; what it does corrupt is
# the report the operator reads, which would say `ready=true` about a peer this
# round could not reach at all. That is the assertion below.
probe_round_file="$TMP_ROOT/probe-round"
printf '0\n' >"$probe_round_file"
# shellcheck disable=SC2034  # Read by the production function loaded through eval.
# Five seconds, not one: the production loop computes deadline=$((SECONDS+timeout)),
# and SECONDS is an integer, so a 1s budget is really (0,1] — round 1's predicate
# work (the full-body fixture through the jq-less lane costs ~0.3s) can cross an
# integer-second boundary and end the loop after a single poll, turning the
# "needs two polls" guard below into a wall-clock flake (red in well over half
# of jq-less measurement runs). With the 5s budget the second poll lands ~1.3s
# in; the loop then keeps polling until the deadline (~5 polls, ~4.7s), which
# the assertions tolerate.
DEPLOY_PEER_VERDICT_TIMEOUT_SECS=5
# shellcheck disable=SC2034  # Read by the production function loaded through eval.
DEPLOY_PEER_VERDICT_POLL_INTERVAL_SECS=1
# shellcheck disable=SC2329  # Invoked indirectly by the production wait function.
_probe_peer_deploy_state() {
    # Round 1 answers with a ready body; every round after it FAILS, as an
    # unreachable peer does. A file holds the counter because each call runs
    # inside a command substitution of its own.
    local round
    round=$(cat "$probe_round_file")
    round=$((round + 1))
    printf '%s\n' "$round" >"$probe_round_file"
    if [ "$round" -eq 1 ]; then
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            missing 'no terminal marker' target-head read \
            false 'ok=false, status=degraded' "$STANDBY_READY_BODY"
        return 0
    fi
    return 255
}

stale_verdict="$(run_peer_verdict)"
stale_rc="${stale_verdict%%$'\n'*}"
stale_out="${stale_verdict#*$'\n'}"
if [ "$stale_rc" -eq 0 ]; then
    fail_test "a peer that stopped answering after one ready poll must not be verified; got: $stale_out"
elif [ "$(cat "$probe_round_file")" -lt 2 ]; then
    fail_test "this case needs at least two polls to mean anything; the loop ran $(cat "$probe_round_file")"
elif ! grep -q 'ready=false' <<<"$stale_out"; then
    fail_test "a round whose probe failed must report ready=false -- the previous round's body must not be re-judged and reported as this round's health; got: $stale_out"
fi

# --- 3c. peer leg rc propagation: _deploy_to_one_peer must use verdict rc ----
# If _wait_for_peer_deploy_verdict fails, _deploy_to_one_peer's rc must propagate it.
# Stub the verdict function and ssh/rsync to verify rc is not masked by unconditional
# success-returning statements (e.g. echo that should be before not after the verdict call).

# First, the structural check (text exists in the function).
peer_deploy_body="$(extract_function _deploy_to_one_peer)"
case "$peer_deploy_body" in
    *_wait_for_peer_deploy_verdict*) : ;;
    *) fail_test "_deploy_to_one_peer must use the observed peer verdict after SSH launches the deploy" ;;
esac
case "$peer_deploy_body" in
    *'deploy completed'*) fail_test "_deploy_to_one_peer must not describe SSH launch success as deploy completion" ;;
    *) : ;;
esac

# Now test rc propagation: load the function, stub its dependencies, and verify
# that when _wait_for_peer_deploy_verdict fails (rc=1), _deploy_to_one_peer also fails.
eval "$(extract_function _deploy_to_one_peer)"

# Stub git to match actual invocation: git -C "$REPO" rev-parse HEAD
# $1="-C", $2=$REPO path, $3="rev-parse", $4="HEAD"
git() {
    if [ "$3" = "rev-parse" ] && [ "$4" = "HEAD" ]; then
        echo "abc1234567890def"
    else
        return 0
    fi
}

# Stub ssh to handle different commands and return appropriate output
ssh() {
    # Parse the command to determine what output to return
    local cmd="${*: -1}"
    if [[ "$cmd" == *"AGENTDESK_ROOT_DIR"* ]]; then
        # Return peer's ADK_REL and port
        printf '%s\n' "/stub/.adk/release"
        printf '%s\n' "8791"
    else
        return 0
    fi
}

# Stub rsync (invoked only when routines directory exists on local machine)
rsync() {
    return 0
}

# Stub _deploy_peer_env_prelude to return empty string
_deploy_peer_env_prelude() {
    echo ""
}

# Stub _wait_for_peer_deploy_verdict to FAIL (return 1) and record that it was
# REACHED, with the arguments it was handed.
#
# The marker is what gives this case its discrimination. `_deploy_to_one_peer`
# returns 1 from six earlier paths too -- the pre-sync ssh, the port-resolving
# ssh, the empty-root and non-numeric-port validations, the routine rsync, and
# the ssh that launches the remote deploy -- so `rc != 0` alone is satisfied by a
# run in which the verdict call is never reached at all. Any stub going stale (an ssh invocation this stub does not
# answer, a new validation the fixture does not satisfy) would then leave the
# assertion below green while testing nothing. A file is used rather than a
# variable because the call under test runs inside a command substitution, and a
# subshell's variables do not survive.
PEER_VERDICT_STUB_MARKER="$TMP_ROOT/peer-verdict-reached"
_wait_for_peer_deploy_verdict() {
    printf '%s\n' "$*" >"$PEER_VERDICT_STUB_MARKER"
    return 1
}

# Stub global variables required by _deploy_to_one_peer
export REPO="/stub/repo"
export ADK_REL="/stub/.adk/release"
export DEPLOY_SSH_CONNECT_TIMEOUT=10

# Test: _deploy_to_one_peer with failing verdict should return non-zero
# Verify rc propagates from verdict call, not from an earlier failure path.
peer_deploy_rc=0
peer_deploy_out=$(_deploy_to_one_peer "test-peer" 2>&1) || peer_deploy_rc=$?
if [ "$peer_deploy_rc" -eq 0 ]; then
    fail_test "_deploy_to_one_peer must fail (rc≠0) when _wait_for_peer_deploy_verdict fails; got rc=$peer_deploy_rc"
elif grep -q 'deploy verified' <<<"$peer_deploy_out"; then
    fail_test "_deploy_to_one_peer failure must not claim verified success; got: $peer_deploy_out"
elif [ ! -f "$PEER_VERDICT_STUB_MARKER" ]; then
    fail_test "the rc≠0 above must come FROM the verdict call: the verdict stub was never reached, so an earlier failure path produced it; got: $peer_deploy_out"
else
    # The rc is the verdict's, and the values the verdict was judged against are
    # the ones the earlier steps actually resolved -- the peer, the port from the
    # remote config read, and the local repo head. A stub drifting into returning
    # nothing would surface here rather than as a still-green rc check.
    peer_verdict_args="$(cat "$PEER_VERDICT_STUB_MARKER")"
    case "$peer_verdict_args" in
        'test-peer '*' 8791 abc1234567890def') : ;;
        *) fail_test "the verdict must be handed the resolved peer, port, and expected repo head; got: $peer_verdict_args" ;;
    esac
fi

if grep -q 'Cluster Deploy Complete (all peers healthy)' "$DEPLOY_SH"; then
    fail_test "the cluster verdict must not claim all peers healthy without naming the verified verdict"
fi

if [ "$failures" -ne 0 ]; then
    printf '%s\n' "test_cluster_deploy_peer_verdict_5189: $failures assertion(s) failed" >&2
    exit 1
fi

printf '%s\n' "test_cluster_deploy_peer_verdict_5189: all assertions passed"
