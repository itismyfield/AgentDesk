#!/usr/bin/env bash
# Unit test for #5736 / PR #5795 r2 — the relay-verdict axis on the DEPLOY gate.
#
# #5736 made the polarity pass answer on the summary build, so the PUBLIC
# `/api/health` body that `wait_for_http_service_health` polls now reports
# `status: degraded` with `relay_verdict_<label>_<provider>_<channel_id>` reasons
# whenever the 4987 §5.1 switch is `composite`. `health_json_is_ready` had no
# branch tolerating those, so a serving node whose relay axis was merely
# UNOBSERVABLE failed the deploy and rollback gates in deploy-release.sh and
# deploy.sh. Under test: the summary stays honest and the gate stops blocking on
# the relay axis ALONE — one other degraded cause and it must still block.
#
# Real helpers sourced from _defaults.sh, both the jq and the jq-less paths.
# Self-contained: no service, no launchd.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEFAULTS_SH="$REPO_ROOT/scripts/_defaults.sh"

PASS=0
FAIL=0
FAIL_NAMES=()

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1" >&2; FAIL=$((FAIL + 1)); FAIL_NAMES+=("$1"); }

assert_rc() {
  local label="$1" expected="$2"; shift 2
  set +e
  "$@" >/dev/null 2>&1
  local rc=$?
  set -e
  if [ "$rc" = "$expected" ]; then pass "$label (rc=$rc)"; else fail "$label (expected rc=$expected, got rc=$rc)"; fi
}

[ -f "$DEFAULTS_SH" ] || { echo "FATAL: $DEFAULTS_SH missing"; exit 2; }
# shellcheck source=/dev/null
. "$DEFAULTS_SH"

# ── Fixtures — the real PUBLIC /api/health shape after #5736 ─────────────────
# Measured on the live release node 2026-09-09: `/api/health/detail` carried
# eight relay_verdict_* reasons under `relay_verdict_source: composite` while the
# pre-#5736 public body still said healthy.
RELAY_ONLY_BODY='{"ok":false,"status":"degraded","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"degraded_reasons":["relay_verdict_unknown_claude_1480015244062490774","relay_verdict_degraded_claude_1479671298497183835","relay_verdict_unknown_codex_1479671301387059200"]}'
RELAY_SINGLE_BODY='{"ok":false,"status":"degraded","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"degraded_reasons":["relay_verdict_unknown_codex_1479671301387059200"]}'
# Relay reasons PLUS an unrelated degraded cause — must still BLOCK.
RELAY_PLUS_DISK_BODY='{"ok":false,"status":"degraded","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"degraded_reasons":["relay_verdict_unknown_codex_1479671301387059200","disk_low_free_bytes:123"]}'
# A non-relay reason ordered FIRST — the element-wise test must not be fooled by
# position (the defect #5071 S0 r3 fixed twice in this file's sibling helpers).
DISK_FIRST_BODY='{"ok":false,"status":"degraded","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"degraded_reasons":["disk_low_free_bytes:123","relay_verdict_unknown_codex_1479671301387059200"]}'
RELAY_PLUS_STALLED_BODY='{"ok":false,"status":"degraded","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"degraded_reasons":["relay_verdict_unknown_codex_1479671301387059200","provider:codex:reconcile_stalled"]}'
RELAY_DB_DOWN_BODY='{"ok":false,"status":"degraded","version":"x","db":false,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"degraded_reasons":["relay_verdict_unknown_codex_1479671301387059200"]}'
# unhealthy/db-down/stalled variants: never rescued (the polarity pass only ever
# worsens to Degraded, so an unhealthy body is unhealthy for something else).
RELAY_UNHEALTHY_BODY='{"ok":false,"status":"unhealthy","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"degraded_reasons":["relay_verdict_unknown_codex_1479671301387059200"]}'
NEAR_MISS_BODY='{"ok":false,"status":"degraded","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"degraded_reasons":["relay_verdict"]}'

run_gate_cases() {
  local mode="$1"
  echo "== #5736 relay-verdict deploy allowance ($mode) =="
  # 4th arg = 1 keeps the #4348 no-provider opt-in on, matching deploy-release.sh:2868.
  assert_rc "[$mode] relay-only degraded body is deploy-ready" 0 \
    health_json_is_ready "$RELAY_ONLY_BODY" 1 1 1
  assert_rc "[$mode] single relay reason is deploy-ready" 0 \
    health_json_is_ready "$RELAY_SINGLE_BODY" 1 1 1
  assert_rc "[$mode] relay + unrelated degraded cause still blocks" 1 \
    health_json_is_ready "$RELAY_PLUS_DISK_BODY" 1 1 1
  assert_rc "[$mode] non-relay reason ordered first still blocks" 1 \
    health_json_is_ready "$DISK_FIRST_BODY" 1 1 1
  assert_rc "[$mode] relay + reconcile_stalled still blocks" 1 \
    health_json_is_ready "$RELAY_PLUS_STALLED_BODY" 1 1 1
  assert_rc "[$mode] relay reasons on a db-down node still blocks" 1 \
    health_json_is_ready "$RELAY_DB_DOWN_BODY" 1 1 1
  assert_rc "[$mode] relay reasons on an unhealthy node still blocks" 1 \
    health_json_is_ready "$RELAY_UNHEALTHY_BODY" 1 1 1

  echo "== #5736 predicate boundaries ($mode) =="
  assert_rc "[$mode] predicate accepts a relay-only body" 0 \
    _health_json_degraded_only_relay_verdict "$RELAY_ONLY_BODY"
  assert_rc "[$mode] predicate rejects a mixed body" 1 \
    _health_json_degraded_only_relay_verdict "$RELAY_PLUS_DISK_BODY"
  assert_rc "[$mode] predicate rejects an empty-reason body" 1 \
    _health_json_degraded_only_relay_verdict '{"status":"degraded","db":true,"server_up":true,"degraded_reasons":[]}'
  assert_rc "[$mode] predicate rejects a healthy body" 1 \
    _health_json_degraded_only_relay_verdict '{"status":"healthy","db":true,"server_up":true,"degraded_reasons":[]}'
  # `relay_verdict` with no suffix is not a reason the polarity pass can emit.
  assert_rc "[$mode] predicate rejects a bare relay_verdict token" 1 \
    _health_json_degraded_only_relay_verdict "$NEAR_MISS_BODY"
}

run_gate_cases "jq"

# Force the jq-less fallback and re-run every case: a controller without jq must
# reach the same verdict, or the gate diverges by host (#5071 S0b).
_health_json_has_jq() { return 1; }
run_gate_cases "jq-less"
unset -f _health_json_has_jq

echo
echo "==== Results ===="
echo "  PASS: $PASS"
echo "  FAIL: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  printf '  failed: %s\n' "${FAIL_NAMES[@]}" >&2
  exit 1
fi
exit 0
