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
    _post_deploy_smoke_run_turn_scenario \
    _post_deploy_smoke_check_turn_scenarios _report_post_deploy_smoke_failure; do
    body=$(extract_function "$fn")
    [ -n "$body" ] || { echo "FAIL: $fn missing from $DEPLOY_SH" >&2; exit 1; }
    eval "$body"
done

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
             "--required-coverage-class"):
    parser.add_argument(flag)
parser.add_argument("--no-reset-before-each", action="store_true")
args = parser.parse_args()
with open(os.environ["FAKE_ARGV_LOG"], "a", encoding="utf-8") as handle:
    handle.write(" ".join(sys.argv[1:]) + "\n")
mode = os.environ.get(f"FAKE_MODE_{args.filter.replace('-', '_')}", "clean")
output = Path(args.output)
output.mkdir(parents=True, exist_ok=True)
run_id = output.name
scenario = next(
    data for path in sorted(Path(args.scenarios).glob("*.yaml"))
    if (data := yaml.safe_load(path.read_text(encoding="utf-8"))).get("id") == args.filter
)
counts = {m.replace("{run_id}", run_id): 1 for m in scenario["report_marker_counts"]}
status, reason = "pass", None
if mode == "withheld":
    counts = {m: (0 if m.endswith(":AFTER_CLEAR]") else 1) for m in counts}
    status, reason = "fail", "assertion: timeout waiting for Discord text"
row = {"id": args.filter, "status": status, "reason": reason, "marker_counts": counts}
report = {"run_id": run_id, "scenarios": [row], "totals": {"pass": int(status == "pass")}}
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
        POST_DEPLOY_SMOKE_FAILURES=(); POST_DEPLOY_SMOKE_TURN_COVERAGE='not run'
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
            command python3 "$@"
        }
        if [ "$name" = no_fingerprint ]; then
            _post_deploy_smoke_log_head_fingerprint() { return 1; }
        fi
        _notify_channel() { printf '%s\n' "$1" > "$root/alert"; }
        hostname() { echo fixture-node; }
        rc=0
        _post_deploy_smoke_check_turn_scenarios || rc=$?
        if [ "${#POST_DEPLOY_SMOKE_FAILURES[@]}" -gt 0 ]; then
            _report_post_deploy_smoke_failure > /dev/null
        fi
        printf 'RC=%s\nCOVERAGE=%s\n' "$rc" "$POST_DEPLOY_SMOKE_TURN_COVERAGE"
    ) > "$root/out" 2>&1 || true
    printf '%s' "$root"
}

two_cells='agents:
  - id: adk-claude-tui-e2e
    channels: {claude: {id: "1509350490461180105"}}
  - id: adk-codex-tui-e2e
    channels: {codex: {id: "1509350778043895902"}}'
claude_only='agents:
  - id: adk-claude-tui-e2e
    channels: {claude: {id: "1509350490461180105"}}'
export FIXTURES

# 1. Healthy deploy: both scenarios run, each reports a pass line.
root=$(run_case clean 1509350490461180105 "$two_cells" 0)
grep -q '^RC=0$' "$root/out" || fail_test "clean: rc $(cat "$root/out")"
grep -q '^COVERAGE=E-50=pass E-51=pass$' "$root/out" || fail_test "clean: coverage $(cat "$root/out")"
grep -q '^relay E-50 cell=claude-tui result=pass deliveries T1=1 T2=1 AFTER_CLEAR=1 composer_not_detected=0 warm_followup_kill=0 cold_resume=0$' "$root/evidence" \
    || fail_test "clean: E-50 pass line missing"
grep -q '^relay E-51 cell=codex-tui result=pass ' "$root/evidence" || fail_test "clean: E-51 pass line missing"
grep -q -- '--cell claude-tui --channel-id 1509350490461180105 .*--filter E-50 --no-reset-before-each --phase-deadline-s 120 ' "$root/argv" \
    || fail_test "clean: E-50 driver args $(cat "$root/argv")"
grep -q -- '--cell codex-tui --channel-id 1509350778043895902 .*--filter E-51 --no-reset-before-each --phase-deadline-s 150 ' "$root/argv" \
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

if [ "$failures" -ne 0 ]; then
    printf 'test_deploy_smoke_turn_scenarios_6561: %s assertion(s) failed\n' "$failures" >&2
    exit 1
fi
printf 'test_deploy_smoke_turn_scenarios_6561: all assertions passed\n'
