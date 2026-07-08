#!/usr/bin/env bash
# Unit test for #4348 — deploy/rollback brick fixes in scripts/_defaults.sh.
#
# Defect 1 (deploy readiness): a serving leader-only / no-agent-session node is
# structurally `status=unhealthy` forever (no_provider_runtimes_registered) but
# must be treated as DEPLOY-READY, and ONLY for that exact cause.
# Defect 2 (rollback safety): the migration-advance comparison used to refuse a
# rollback that would strand the old binary behind an already-applied migration.
#
# All assertions run against the real helpers sourced from _defaults.sh, in both
# the jq and the jq-less fallback paths. Self-contained: no service, no launchd.

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
  # assert_rc "<label>" <expected_rc> <cmd...>
  local label="$1" expected="$2"; shift 2
  set +e
  "$@" >/dev/null 2>&1
  local rc=$?
  set -e
  if [ "$rc" = "$expected" ]; then pass "$label (rc=$rc)"; else fail "$label (expected rc=$expected, got rc=$rc)"; fi
}

assert_eq() {
  local label="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then pass "$label (= $expected)"; else fail "$label (expected=$expected actual=$actual)"; fi
}

[ -f "$DEFAULTS_SH" ] || { echo "FATAL: $DEFAULTS_SH missing"; exit 2; }
# shellcheck source=/dev/null
. "$DEFAULTS_SH"

# ── Fixtures — modelled on the real PUBLIC /api/health body shape ────────────
# Serving leader-only node: unhealthy SOLELY due to no provider runtimes.
NO_PROVIDER_BODY='{"ok":false,"status":"unhealthy","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":false,"cluster_standby":false,"degraded":true,"startup_status":"doctor_skipped","startup_degraded":false,"startup_degraded_reasons":[],"latest_startup_doctor":{"available":true,"doctor_status":"skipped","skipped":true,"skipped_reason":"no_provider_runtimes_registered"}}'
# DB down: server_up=false — must NEVER be rescued.
DB_DOWN_BODY='{"ok":false,"status":"unhealthy","version":"x","db":true,"dashboard":true,"server_up":false,"fully_recovered":false,"cluster_standby":false,"degraded":true,"startup_status":"doctor_skipped","latest_startup_doctor":{"skipped_reason":"no_provider_runtimes_registered"}}'
# Unhealthy for a DIFFERENT reason (doctor ran, providers present) — must fail.
OTHER_UNHEALTHY_BODY='{"ok":false,"status":"unhealthy","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"startup_status":"doctor_passed","latest_startup_doctor":{"doctor_status":"passed","skipped_reason":null}}'
# Fully healthy.
HEALTHY_BODY='{"ok":true,"status":"healthy","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":false,"startup_status":"doctor_passed"}'

run_gate_cases() {
  local mode="$1"
  echo "== Defect 1 gate — ${mode} path =="

  # allow_no_provider_runtimes=1 → no-provider node is deploy-ready.
  assert_rc "[$mode] no-provider unhealthy + allow=1 → READY" 0 \
    health_json_is_ready "$NO_PROVIDER_BODY" 1 1 1
  # Default (allow=0) preserves the strict semantics: unhealthy => not ready.
  assert_rc "[$mode] no-provider unhealthy + allow=0 (default) → NOT ready" 1 \
    health_json_is_ready "$NO_PROVIDER_BODY" 1 1
  # DB down must never be rescued even with allow=1.
  assert_rc "[$mode] db/server down + allow=1 → NOT ready" 1 \
    health_json_is_ready "$DB_DOWN_BODY" 1 1 1
  # A different unhealthy cause must still fail with allow=1.
  assert_rc "[$mode] other unhealthy cause + allow=1 → NOT ready" 1 \
    health_json_is_ready "$OTHER_UNHEALTHY_BODY" 1 1 1
  # Healthy node is ready regardless of the opt-in flag.
  assert_rc "[$mode] healthy + allow=1 → READY" 0 \
    health_json_is_ready "$HEALTHY_BODY" 1 1 1
  assert_rc "[$mode] healthy + allow=0 → READY" 0 \
    health_json_is_ready "$HEALTHY_BODY" 1 1

  # The predicate helper in isolation.
  assert_rc "[$mode] predicate matches no-provider body" 0 \
    _health_json_unhealthy_only_no_provider_runtimes "$NO_PROVIDER_BODY"
  assert_rc "[$mode] predicate rejects other-unhealthy body" 1 \
    _health_json_unhealthy_only_no_provider_runtimes "$OTHER_UNHEALTHY_BODY"
  assert_rc "[$mode] predicate rejects db-down body" 1 \
    _health_json_unhealthy_only_no_provider_runtimes "$DB_DOWN_BODY"
}

# jq path (jq is present in this environment).
run_gate_cases "jq"

# Force the jq-less fallback and re-run every case.
_health_json_has_jq() { return 1; }
run_gate_cases "jq-less"
unset -f _health_json_has_jq

echo "== Defect 1 gate — wait loop end-to-end (curl shim) =="
SHIM_DIR="$(mktemp -d)"
trap 'rm -rf "$SHIM_DIR"' EXIT
mkdir -p "$SHIM_DIR/bin"
BODY_FILE="$SHIM_DIR/body.json"
cat >"$SHIM_DIR/bin/curl" <<EOF
#!/usr/bin/env bash
cat "$BODY_FILE"
EOF
chmod +x "$SHIM_DIR/bin/curl"
printf '%s' "$NO_PROVIDER_BODY" >"$BODY_FILE"
# Run inside a subshell so the shim PATH (and any launchctl noise) is isolated;
# `env` cannot invoke a shell function, so scope PATH via the subshell instead.
_wait_with_shim() { ( PATH="$SHIM_DIR/bin:$PATH"; wait_for_http_service_health "$@" ); }
# 7th arg = 1 → the wait loop accepts the serving no-provider node on attempt 1.
assert_rc "wait loop accepts no-provider node when opted in (arg7=1)" 0 \
  _wait_with_shim "test.label" 0 1 0 1 1 1
# Without the opt-in the same body must fail (retries=1, delay=0 keeps it fast).
assert_rc "wait loop rejects no-provider node without opt-in (arg7 omitted)" 1 \
  _wait_with_shim "test.label" 0 1 0 1 1

echo "== Defect 2 — migration sequence parsing =="
assert_eq "seq of 0079_relay_dead_letter.sql" "79" "$(_migration_seq_from_name '0079_relay_dead_letter.sql')"
assert_eq "seq of 0080_intake_outbox_provider.sql (octal-safe)" "80" "$(_migration_seq_from_name '0080_intake_outbox_provider.sql')"
assert_eq "seq of 0100_x.sql" "100" "$(_migration_seq_from_name '0100_x.sql')"
assert_rc "seq of non-numeric name fails" 1 _migration_seq_from_name "garbage.sql"
assert_rc "seq of empty name fails" 1 _migration_seq_from_name ""

echo "== Defect 2 — rollback-would-brick decision (_migration_advanced) =="
# new > old → advanced → UNSAFE to roll back (return 0 = true).
assert_rc "new 79 vs old 78 → advanced (unsafe)" 0 _migration_advanced "0079_a.sql" "0078_b.sql"
# new == old → not advanced → safe.
assert_rc "new 78 vs old 78 → safe" 1 _migration_advanced "0078_a.sql" "0078_b.sql"
# new < old → not advanced → safe.
assert_rc "new 77 vs old 79 → safe" 1 _migration_advanced "0077_a.sql" "0079_b.sql"
# Fail closed on unresolved names → treat as advanced (unsafe).
assert_rc "unresolved new name → unsafe (fail closed)" 0 _migration_advanced "garbage" "0078_b.sql"
assert_rc "empty old name → unsafe (fail closed)" 0 _migration_advanced "0079_a.sql" ""

echo
echo "==== Results ===="
echo "  PASS: $PASS"
echo "  FAIL: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  printf '  failed: %s\n' "${FAIL_NAMES[@]}" >&2
  exit 1
fi
exit 0
