#!/usr/bin/env bash
# The eval'd production functions read the globals each case assigns.
# shellcheck disable=SC2034
# Post-deploy smoke wiring for the E-50/E-51 `!clear` + continuous-turn runs.
# The production functions run against a fake driver and fake dcserver log; the
# preflight is the only other stub. The judge and config resolver are real.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPLOY_SH="${DEPLOY_SH_OVERRIDE:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-smoke-turns-test.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

extract_function() {
    awk -v start="^${1}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$DEPLOY_SH"
}
for fn in _post_deploy_smoke_note _post_deploy_smoke_fail \
    _post_deploy_smoke_log_identity_and_size _post_deploy_smoke_log_head_fingerprint \
    _post_deploy_smoke_classified_note _post_deploy_smoke_check_relay_round_trip \
    _post_deploy_smoke_run_turn_scenario \
    _post_deploy_smoke_check_turn_scenarios _report_post_deploy_smoke_failure; do
    body=$(extract_function "$fn")
    [ -n "$body" ] || { echo "FAIL: $fn missing from $DEPLOY_SH" >&2; exit 1; }
    eval "$body"
done

DISPOSITION=$(sed -n '/^# >>> BEGIN smoke-disposition region/,/^# <<< END smoke-disposition region/p' "$DEPLOY_SH")

FIXTURES="$REPO_ROOT/tests/e2e/tui_relay/fixtures/turn_smoke"
FAKE_DRIVER="$TMP_ROOT/fake_driver.py"
cat > "$FAKE_DRIVER" <<'PY'
"""Writes the report run_tui_relay would write and appends server log lines."""
import argparse, json, os, shutil, sys
from pathlib import Path

import yaml

parser = argparse.ArgumentParser()
for flag in ("--base-url", "--cell", "--channel-id", "--scenarios", "--filter", "--output",
             "--queue-runtime-root", "--phase-deadline-s", "--required-agent-mode",
             "--required-coverage-class", "--run-id"):
    parser.add_argument(flag)
parser.add_argument("--no-reset-before-each", action="store_true")
args = parser.parse_args()
with open(os.environ["FAKE_ARGV_LOG"], "a", encoding="utf-8") as handle:
    handle.write(" ".join(sys.argv[1:]) + "\n")
mode = os.environ.get(f"FAKE_MODE_{args.filter.replace('-', '_')}", "clean")
output = Path(args.output)
output.mkdir(parents=True, exist_ok=True)
run_id = args.run_id or output.name
scenario = next(
    data for path in sorted(Path(args.scenarios).glob("*.yaml"))
    if (data := yaml.safe_load(path.read_text(encoding="utf-8"))).get("id") == args.filter
)
counts = {m.replace("{run_id}", run_id): 1 for m in scenario.get("report_marker_counts", [])}
status, reason = "pass", None
if mode == "withheld":
    counts = {m: (0 if m.endswith(":AFTER_CLEAR]") else 1) for m in counts}
    status, reason = "fail", "assertion: timeout waiting for Discord text"
row = {"cell": args.cell, "channel_id": args.channel_id, "run_id": run_id, "id": args.filter, "status": status, "reason": reason, "marker_counts": counts}
if mode == "rotation":
    content = "세션을 초기화하지 못했어요: herdr clear refused: rotation_unsettled(59)"
    row.update(status="fail", reason="assertion: text !clear refused by the server: " + content,
               marker_counts={m: (0 if m.endswith(":AFTER_CLEAR]") else 1) for m in counts},
               clear_refusal={"stage": "clear_ack", "after_id": "100", "reply": {
                   "id": "101", "channel_id": args.channel_id, "author": {"bot": True}, "content": content}})
    status = "fail"
if mode in {"e1_gap", "e1_other"}:
    row.update(status="fail", reason="RuntimeError: turn/start HTTP 422: " + json.dumps({
        "status": "invalid_target", "error": "herdr turn refused: " + (
            "executor_not_wired" if mode == "e1_gap" else "other")}))
    status = "fail"
row["assertions"] = []
if status == "fail":
    row["failure_attribution"] = {"source": "exception" if args.filter == "E-1" else "assertion",
                                  "raw_reason": row["reason"].removeprefix("assertion: ")}
report = {"coverage_class_violations": [], "cell": args.cell, "channel_id": args.channel_id, "run_id": run_id, "scenarios": [row], "totals": {"pass": int(status == "pass")}}
(output / f"report.{args.cell}.json").write_text(json.dumps(report), encoding="utf-8")
log = Path(os.environ["FAKE_SERVER_LOG"])
fixture = Path(os.environ["FIXTURES"]) / (
    "codex-warm-followup-kill.dcserver.log" if mode == "warm_kill" else "turns-clean.dcserver.log"
)
if mode == "rotated":
    log.unlink()
elif mode == "rewritten":
    log.write_text("", encoding="utf-8")
with log.open("a", encoding="utf-8") as handle:
    handle.write(fixture.read_text(encoding="utf-8"))
sys.exit(0 if status == "pass" else 1)
PY

failures=0
fail_test() { printf 'FAIL: %s\n' "$1" >&2; failures=$((failures + 1)); }

# One isolated smoke run; prints evidence, coverage and the function rc.
run_case() {
    local name="$1" relay_channel="$2" config="$3" preflight_rc="$4"
    shift 4
    local root="$TMP_ROOT/$name"
    mkdir -p "$root/release/config" "$root/release/logs" "$root/tmp"
    printf '%s\n' "$config" > "$root/release/config/agentdesk.yaml"
    [ "$name" = log_absent ] || printf 'boot line\n' > "$root/release/logs/dcserver.stdout.log"
    (
        for assignment in "$@"; do export "${assignment?}"; done
        export FAKE_ARGV_LOG="$root/argv" FAKE_SERVER_LOG="$root/release/logs/dcserver.stdout.log" FIXTURES
        ADK_REL="$root/release"; REPO="$REPO_ROOT"; ADK_DEFAULT_LOOPBACK=127.0.0.1; REL_PORT=1
        POST_DEPLOY_SMOKE_STAMP=fixture; POST_DEPLOY_SMOKE_EVIDENCE="$root/evidence"
        POST_DEPLOY_SMOKE_TMP_DIR="$root/tmp"; POST_DEPLOY_SMOKE_LOG_PATH="$ADK_REL/logs/dcserver.stdout.log"
        POST_DEPLOY_SMOKE_LOG_FINGERPRINT_CAP=4096
        POST_DEPLOY_SMOKE_RELAY_CELL=claude-tui; POST_DEPLOY_SMOKE_RELAY_CHANNEL_ID="$relay_channel"
        POST_DEPLOY_SMOKE_CODEX_TURNS_CELL=codex-tui
        POST_DEPLOY_SMOKE_CLAUDE_TURNS_DEADLINE_S=120; POST_DEPLOY_SMOKE_CODEX_TURNS_DEADLINE_S=150
        POST_DEPLOY_SMOKE_FAILURES=(); POST_DEPLOY_SMOKE_KNOWN_GAPS=(); POST_DEPLOY_SMOKE_UNEXPECTED_PASSES=()
        POST_DEPLOY_SMOKE_TURN_COVERAGE='not run'
        POST_DEPLOY_SMOKE_CREATE_ISSUE=off
        POST_DEPLOY_SMOKE_WEDGE_COVERAGE=clean; POST_DEPLOY_SMOKE_DURABLE_COVERAGE=evaluated
        : > "$POST_DEPLOY_SMOKE_EVIDENCE"
        python3() {
            case "${1:-}" in
                scripts/e2e/post_deploy_turn_smoke.py)
                    if [ "${2:-}" = preflight ]; then
                        [ "$preflight_rc" -eq 0 ] && { echo idle; return 0; }
                        echo "busy: mailbox fixture active_turn"; return 1
                    fi ;;
                scripts/e2e/run_tui_relay.py) shift; command python3 "$FAKE_DRIVER" "$@"; return ;;
            esac
            if [ "${1:-}" = - ] && [[ "${2:-}" == http* ]]; then return 1; fi
            command python3 "$@"
        }
        if [ "$name" = no_fingerprint ]; then
            _post_deploy_smoke_log_head_fingerprint() { return 1; }
        fi
        _notify_channel() { printf '%s\n' "$1" > "$root/alert"; }
        hostname() { echo fixture-node; }
        rc=0
        if [[ "$name" == e1_* || "$name" = both_classifications ]]; then
            POST_DEPLOY_SMOKE_READY=true
            POST_DEPLOY_SMOKE_HEALTH_BODY="$root/health"
            POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY="$root/detail"
            POST_DEPLOY_SMOKE_SESSIONS_BODY="$root/sessions"
            printf '{"mailboxes": []}' > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
            printf '%s' "${FAKE_SESSIONS:-[]}" > "$POST_DEPLOY_SMOKE_SESSIONS_BODY"
            printf '{}' > "$POST_DEPLOY_SMOKE_HEALTH_BODY"
            _post_deploy_smoke_resolve_cluster_standby() { echo false; }
            _post_deploy_smoke_check_relay_round_trip || rc=$?
        else
            _post_deploy_smoke_check_turn_scenarios || rc=$?
        fi
        if [ "$name" = both_classifications ]; then
            _post_deploy_smoke_check_turn_scenarios || rc=$?
            _run_post_deploy_functional_smoke() { return "$rc"; }
            eval "$DISPOSITION"
        elif [[ "$name" == e1_turns_* ]]; then
            _post_deploy_smoke_check_turn_scenarios || rc=$?
        fi
        if [ "${#POST_DEPLOY_SMOKE_FAILURES[@]}" -gt 0 ]; then
            _report_post_deploy_smoke_failure > /dev/null
        fi
        printf 'RC=%s\nCOVERAGE=%s\n' "$rc" "$POST_DEPLOY_SMOKE_TURN_COVERAGE"
    ) > "$root/out" 2>&1 || true
    printf '%s' "$root"
}

# claude-tui runs on its own tmux agent; adk-claude-tui-e2e is the claude-herdr worker.
two_cells='agents:
  - id: adk-claude-tui-e2e
    channels: {claude: {id: "1509350490461180100"}}
  - id: adk-claude-tui-2-e2e
    channels: {claude: {id: "1509350490461180105"}}
  - id: adk-codex-tui-e2e
    channels: {codex: {id: "1509350778043895902"}}'
claude_only='agents:
  - id: adk-claude-tui-2-e2e
    channels: {claude: {id: "1509350490461180105"}}'
export FIXTURES

# 1. Healthy deploy: both scenarios run, each reports a pass line.
root=$(run_case clean 1509350490461180105 "$two_cells" 0)
grep -q '^RC=0$' "$root/out" || fail_test "clean: rc $(cat "$root/out")"
grep -q '^COVERAGE=E-50=pass E-51=pass$' "$root/out" || fail_test "clean: coverage $(cat "$root/out")"
grep -q '^relay E-50 cell=claude-tui result=pass deliveries T1=1 T2=1 AFTER_CLEAR=1 composer_not_detected=0 warm_followup_kill=0 cold_resume=0$' "$root/evidence" \
    || fail_test "clean: E-50 pass line missing"
grep -q '^relay E-51 cell=codex-tui result=pass ' "$root/evidence" || fail_test "clean: E-51 pass line missing"
grep -q -- '--cell claude-tui --channel-id 1509350490461180105 .*--filter E-50 --run-id fixture-E-50-claude-tui --no-reset-before-each --phase-deadline-s 120 ' "$root/argv" \
    || fail_test "clean: E-50 driver args $(cat "$root/argv")"
grep -q -- '--cell codex-tui --channel-id 1509350778043895902 .*--filter E-51 --run-id fixture-E-51-codex-tui --no-reset-before-each --phase-deadline-s 150 ' "$root/argv" \
    || fail_test "clean: E-51 driver args"
! grep -q '^FAIL:' "$root/evidence" || fail_test "clean: unexpected FAIL"

# 2. Codex warm follow-up kill: bodies arrived, the run log decides FAIL.
root=$(run_case warm_kill 1509350490461180105 "$two_cells" 0 FAKE_MODE_E_51=warm_kill)
grep -q '^RC=1$' "$root/out" || fail_test "warm_kill: rc"
grep -q '^COVERAGE=E-50=pass E-51=FAIL$' "$root/out" || fail_test "warm_kill: coverage $(cat "$root/out")"
grep -q '^FAIL: relay E-51 cell=codex-tui result=FAIL deliveries T1=1 T2=1 AFTER_CLEAR=1 composer_not_detected=2 warm_followup_kill=2 cold_resume=2;' "$root/evidence" \
    || fail_test "warm_kill: FAIL line missing $(cat "$root/evidence")"
grep -q 'relay E-51 cell=codex-tui result=FAIL' "$root/alert" || fail_test "warm_kill: alert lacks the scenario line"
grep -q 'turn scenarios: E-50=pass E-51=FAIL' "$root/alert" || fail_test "warm_kill: alert lacks turn coverage"
grep -q 'Turn scenarios: `E-50=pass E-51=FAIL`' "$root/release/logs/post-deploy-smoke-issue-draft-fixture.md" \
    || fail_test "warm_kill: issue draft lacks turn coverage"

# 3. Claude body withheld after !clear.
root=$(run_case withheld 1509350490461180105 "$two_cells" 0 FAKE_MODE_E_50=withheld)
grep -q '^COVERAGE=E-50=FAIL E-51=pass$' "$root/out" || fail_test "withheld: coverage"
grep -q '^FAIL: relay E-50 cell=claude-tui result=FAIL deliveries T1=1 T2=1 AFTER_CLEAR=0 .*AFTER_CLEAR body withheld after !clear (0 deliveries)' "$root/evidence" \
    || fail_test "withheld: FAIL line missing"

# 4. A log replaced mid-run leaves no excerpt: FAIL, never PASS. A recreated file
# may reuse the inode (ext4 does) and an in-place rewrite keeps it.
# No start watermark (log missing, no hash tool) is not evidence either.
for mode in rotated rewritten log_absent no_fingerprint; do
    root=$(run_case "$mode" 1509350490461180105 "$two_cells" 0 FAKE_MODE_E_50="$mode")
    grep -q '^FAIL: relay E-50 .*dcserver log excerpt unavailable' "$root/evidence" || fail_test "$mode: not fail-closed"
done

# 5. No Codex cell configured: E-51 is a skip note, not a failure.
root=$(run_case codex_absent 1509350490461180105 "$claude_only" 0)
grep -q '^RC=0$' "$root/out" || fail_test "codex_absent: rc"
grep -q '^COVERAGE=E-50=pass E-51=skipped$' "$root/out" || fail_test "codex_absent: coverage"
grep -q '^relay E-51=skipped: no codex-tui E2E cell configured$' "$root/evidence" || fail_test "codex_absent: note"

# 6. E-1 resolved no channel (standby/unconfigured) or the cell is busy: nothing is sent.
root=$(run_case standby "" "$two_cells" 0)
grep -q '^COVERAGE=E-50=skipped E-51=skipped$' "$root/out" || fail_test "standby: coverage"
[ ! -s "$root/argv" ] || fail_test "standby: driver ran"
root=$(run_case busy 1509350490461180105 "$two_cells" 1)
grep -q '^COVERAGE=E-50=skipped E-51=skipped$' "$root/out" || fail_test "busy: coverage $(cat "$root/out")"
[ ! -s "$root/argv" ] || fail_test "busy: driver ran"

herdr_cells="$two_cells
session_hosts:
  herdr:
    channels: {1509350490461180105: local}"
root=$(run_case rotation 1509350490461180105 "$herdr_cells" 0 FAKE_MODE_E_50=rotation)
grep -q '^RC=0$' "$root/out" || fail_test "rotation: rc"
grep -q '^COVERAGE=E-50=KNOWN_GAP E-51=pass$' "$root/out" || fail_test "rotation: coverage"
grep -q '^KNOWN_GAP: relay E-50 .*rotation_unsettled(59)$' "$root/evidence" || fail_test "rotation: evidence"
[ ! -f "$root/alert" ] || fail_test "rotation: false alarm"
root=$(run_case unexpected 1509350490461180105 "$herdr_cells" 0)
grep -q '^COVERAGE=E-50=UNEXPECTED_PASS E-51=pass$' "$root/out" || fail_test "unexpected: coverage"
root=$(run_case mixed 1509350490461180105 "$herdr_cells" 0 FAKE_MODE_E_50=rotation FAKE_MODE_E_51=warm_kill)
grep -q '^RC=1$' "$root/out" || fail_test "mixed: rc"
draft="$root/release/logs/post-deploy-smoke-issue-draft-fixture.md"
grep -A2 '^## Known gaps' "$draft" | grep -q 'KNOWN_GAP: relay E-50' || fail_test "mixed: gap section"
! sed -n '/## Findings/,/## Known gaps/p' "$draft" | grep -q 'KNOWN_GAP:' || fail_test "mixed: gap in findings"
root=$(run_case mixed_pass 1509350490461180105 "$herdr_cells" 0 FAKE_MODE_E_51=warm_kill)
grep -A2 '^## Unexpected passes' "$root/release/logs/post-deploy-smoke-issue-draft-fixture.md" | grep -q 'UNEXPECTED_PASS:' || fail_test "mixed_pass: section"
for mode in e1_gap e1_other; do
    root=$(run_case "$mode" 1509350490461180105 "$herdr_cells" 0 "FAKE_MODE_E_1=$mode")
    if [ "$mode" = e1_gap ]; then
        grep -q '^RC=0$' "$root/out" || fail_test "e1_gap: rc $(cat "$root/out")"
        grep -q '^KNOWN_GAP: relay E-1 .*executor_not_wired$' "$root/evidence" || fail_test "e1_gap: evidence"
    else
        grep -q '^RC=1$' "$root/out" || fail_test "e1_other: rc"
        grep -q '^FAIL: relay E-1 round-trip failed' "$root/evidence" || fail_test "e1_other: FAIL"
    fi
done

# The live layout maps only the Herdr worker's channel: claude-tui is a plain tmux PASS whose
# channel, tmux session and workspace tag all come from adk-claude-tui-2-e2e.
split_cells="$two_cells
session_hosts:
  herdr:
    channels: {1509350490461180100: local}"
herdr_agent_busy='[{"status":"turn_active","channel_id":"1509350490461180100","session_key":"claude/h/n:AgentDesk-claude-adk-claude-tui-e2e"}]'
root=$(run_case e1_turns_split "" "$split_cells" 0 "FAKE_SESSIONS=$herdr_agent_busy")
grep -q '^RC=0$' "$root/out" || fail_test "split: rc $(cat "$root/out")"
grep -q '^relay E-1 round-trip=pass$' "$root/evidence" || fail_test "split: the Herdr agent's session must not hold claude-tui's E-1"
grep -q -- '--cell claude-tui --channel-id 1509350490461180105 .*--filter E-1 ' "$root/argv" || fail_test "split: E-1 channel $(cat "$root/argv")"
grep -q -- '--cell claude-tui --channel-id 1509350490461180105 .*--filter E-50 ' "$root/argv" || fail_test "split: E-50 channel"
grep -q '^COVERAGE=E-50=pass E-51=pass$' "$root/out" || fail_test "split: coverage $(cat "$root/out")"
grep -q '^relay E-50 cell=claude-tui result=pass .* warm_followup_kill=0 ' "$root/evidence" \
    || fail_test "split: the Herdr agent's tmux kill counted against claude-tui"
root=$(run_case e1_tmux_agent_busy "" "$split_cells" 0 \
    'FAKE_SESSIONS=[{"status":"turn_active","channel_id":"999","session_key":"claude/h/n:AgentDesk-claude-adk-claude-tui-2-e2e-t999"}]')
grep -q '^relay E-1=skipped: foreign active turn on cell$' "$root/evidence" || fail_test "tmux agent busy: not detected"
[ ! -s "$root/argv" ] || fail_test "tmux agent busy: driver ran"
# Without its own agent, claude-tui fails instead of falling back to the Herdr worker's channel.
old_agent_only='agents:
  - id: adk-claude-tui-e2e
    channels: {claude: {id: "1509350490461180100"}}
  - id: adk-codex-tui-e2e
    channels: {codex: {id: "1509350778043895902"}}'
root=$(run_case e1_old_agent_only "" "$old_agent_only" 0)
grep -q '^RC=1$' "$root/out" || fail_test "old agent only: rc $(cat "$root/out")"
grep -q '^FAIL: relay E-1: could not resolve claude-tui channel' "$root/evidence" || fail_test "old agent only: not a FAIL"
grep -q "'adk-claude-tui-2-e2e' not found" "$root/evidence" || fail_test "old agent only: missing agent unnamed"
[ ! -s "$root/argv" ] || fail_test "old agent only: driver ran $(cat "$root/argv")"

root=$(run_case both_classifications 1509350490461180105 "$herdr_cells" 0 FAKE_MODE_E_1=e1_gap)
grep -q '^RC=0$' "$root/out" || fail_test "both classifications: rc"
grep -q '^△ Post-deploy functional smoke completed with known gaps ' "$root/out" || fail_test "both classifications: known gaps summary"
grep -q '^△ Post-deploy functional smoke completed with unexpected passes ' "$root/out" || fail_test "both classifications: unexpected passes summary"
grep -q '^  KNOWN_GAP: relay E-1 ' "$root/out" || fail_test "both classifications: E-1 summary"
grep -q '^  UNEXPECTED_PASS: relay E-50 ' "$root/out" || fail_test "both classifications: E-50 summary"
[ ! -f "$root/alert" ] || fail_test "both classifications: false alarm"

if [ "$failures" -ne 0 ]; then
    printf 'test_deploy_smoke_turn_scenarios_6561: %s assertion(s) failed\n' "$failures" >&2
    exit 1
fi
printf 'test_deploy_smoke_turn_scenarios_6561: all assertions passed\n'
