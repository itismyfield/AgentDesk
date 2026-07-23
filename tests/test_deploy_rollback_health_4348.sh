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
# Finding #2 (jq-less top-level parity): malformed / future-shape body where the
# TOP-LEVEL server_up/db/dashboard are FALSE but a NESTED object carries them
# true (and a nested status="healthy"). jq matches the root only and REJECTS;
# the jq-less grep fallback must agree — a naive "any occurrence" grep would be
# fooled by the nested keys and green-light a broken node (false-ready).
NESTED_FIELD_MALFORMED_BODY='{"ok":false,"status":"unhealthy","version":"x","db":false,"dashboard":false,"server_up":false,"fully_recovered":false,"startup_status":"doctor_skipped","subsystem":{"server_up":true,"db":true,"dashboard":true,"status":"healthy"},"latest_startup_doctor":{"available":true,"doctor_status":"skipped","skipped":true,"skipped_reason":"no_provider_runtimes_registered"}}'
# Finding #1 (adjudicated SAFE): the NO_PROVIDER body PLUS a co-existing
# DEGRADED/non-blocking axis (disk_low true, stale outbox age). Severity never
# downgrades Unhealthy→Degraded so status stays "unhealthy" with server_up=true.
# This must STILL be deploy-ready under opt-in — exactly as a provider-present
# degraded node passes the gate today.
DEGRADED_AXIS_NO_PROVIDER_BODY='{"ok":false,"status":"unhealthy","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":false,"cluster_standby":false,"degraded":true,"disk_low":true,"outbox_oldest_age_secs":1800,"startup_status":"doctor_skipped","startup_degraded":false,"startup_degraded_reasons":[],"latest_startup_doctor":{"available":true,"doctor_status":"skipped","skipped":true,"skipped_reason":"no_provider_runtimes_registered"}}'
# Finding #3 (jq-less skipped_reason path parity): a DECOY `skipped_reason` with
# the rescue value lives in an UNRELATED nested object, while the REAL
# latest_startup_doctor.skipped_reason is something else. jq reads
# .latest_startup_doctor.skipped_reason (→ reject); a jq-less grep that matches
# skipped_reason ANYWHERE would be fooled by the decoy (→ false accept).
DECOY_SKIPPED_REASON_BODY='{"ok":false,"status":"unhealthy","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":false,"startup_status":"doctor_skipped","other":{"skipped_reason":"no_provider_runtimes_registered"},"latest_startup_doctor":{"available":true,"doctor_status":"passed","skipped":false,"skipped_reason":"providers_present"}}'
# Finding #3 positive control: the REAL latest_startup_doctor.skipped_reason is
# the rescue value while a decoy elsewhere differs — must STILL be accepted, so
# the fix targets the right path in BOTH directions (not "any decoy rejects").
REAL_SKIPPED_REASON_WITH_DECOY_BODY='{"ok":false,"status":"unhealthy","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":false,"startup_status":"doctor_skipped","other":{"skipped_reason":"something_else"},"latest_startup_doctor":{"available":true,"doctor_status":"skipped","skipped":true,"skipped_reason":"no_provider_runtimes_registered"}}'
# Finding #4 (jq-less degraded_reasons path parity): NO top-level
# degraded_reasons, but a nested object carries a reconcile-only array. jq reads
# the TOP-LEVEL .degraded_reasons (absent → [] → reconcile gate REJECTS); a
# jq-less grep matching degraded_reasons ANYWHERE would accept the nested array.
NESTED_ONLY_DEGRADED_REASONS_BODY='{"ok":false,"status":"degraded","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"subsystem":{"degraded_reasons":["provider:codex:reconcile_in_progress"]}}'
# Finding #4 positive control: a genuine TOP-LEVEL reconcile-only degraded body
# must STILL be accepted by the reconcile gate — the fix must not break the
# legitimate reconcile-in-progress path.
TOP_LEVEL_RECONCILE_BODY='{"ok":false,"status":"degraded","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"degraded_reasons":["provider:codex:reconcile_in_progress"]}'
# R2 whitespace regression: a VALID top-level degraded_reasons array with
# insignificant whitespace after `]` (space before the closing `}`). jq ignores
# it and ACCEPTS via the reconcile path; the jq-less raw extractor must trim the
# trailing space so the array cleanup still ends the value at `]` (was returning
# a malformed CSV → reconcile REJECT → jq/jq-less parity break).
WS_RECONCILE_BODY='{"ok":false,"status":"degraded","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"degraded_reasons":["provider:codex:reconcile_in_progress"] }'
# Scalar whitespace tolerance: spaces around every top-level value AND before
# delimiters, plus a nested latest_startup_doctor object with inner spacing.
# Every scalar read (status / db / dashboard / server_up / startup_status /
# latest_startup_doctor.skipped_reason) must parse correctly in both modes.
WS_SCALAR_NO_PROVIDER_BODY='{ "ok": false , "status": "unhealthy" , "db": true , "dashboard": true , "server_up": true , "fully_recovered": false , "startup_status": "doctor_skipped" , "latest_startup_doctor": { "available": true , "skipped_reason": "no_provider_runtimes_registered" } }'
GATEWAY_STANDBY_BODY='{"ok":false,"status":"degraded","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":true,"degraded":true,"degraded_reasons":["provider:codex:gateway_standby"]}'
GATEWAY_STANDBY_MIXED_BODY='{"ok":false,"status":"degraded","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":true,"degraded":true,"degraded_reasons":["provider:codex:gateway_standby","disk_low_free_bytes:1"]}'
GATEWAY_STANDBY_MIXED_UNRECOVERED_BODY='{"ok":false,"status":"degraded","db":true,"dashboard":true,"server_up":true,"fully_recovered":false,"cluster_standby":true,"degraded":true,"degraded_reasons":["provider:codex:gateway_standby","disk_low_free_bytes:1"]}'
GATEWAY_REASON_WITHOUT_STANDBY_BODY='{"ok":false,"status":"degraded","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":false,"degraded":true,"degraded_reasons":["gateway_standby"]}'
HEALTHY_STANDBY_MIXED_BODY='{"ok":true,"status":"healthy","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":true,"degraded":false,"degraded_reasons":["gateway_standby","disk_low_free_bytes:1"]}'
HEALTHY_STANDBY_EMPTY_BODY='{"ok":true,"status":"healthy","db":true,"dashboard":true,"server_up":true,"fully_recovered":true,"cluster_standby":true,"degraded":false,"degraded_reasons":[]}'

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

  # Finding #2 — nested-field malformed body: jq matches TOP-LEVEL only and
  # rejects; the jq-less grep fallback MUST agree (must not be fooled by nested
  # server_up/db/dashboard=true). Both the predicate and the gate must reject in
  # BOTH modes — this negative case is what caught the false-ready path.
  assert_rc "[$mode] nested-field malformed body → predicate REJECTS" 1 \
    _health_json_unhealthy_only_no_provider_runtimes "$NESTED_FIELD_MALFORMED_BODY"
  assert_rc "[$mode] nested-field malformed body + allow=1 → NOT ready" 1 \
    health_json_is_ready "$NESTED_FIELD_MALFORMED_BODY" 1 1 1
  # Direct field-helper parity: a top-level FALSE with a nested TRUE must read as
  # false/absent at the top level (mirrors jq's `.field` / `has()`).
  assert_rc "[$mode] field_is_true(server_up) ignores nested true" 1 \
    _health_json_field_is_true "$NESTED_FIELD_MALFORMED_BODY" "server_up"

  # Finding #1 (adjudicated SAFE) — a serving no-provider node with a co-existing
  # DEGRADED/non-blocking axis (disk_low / stale outbox) is STILL deploy-ready
  # under opt-in, and the predicate still fires. Documents the intended behavior.
  assert_rc "[$mode] no-provider + degraded axis + allow=1 → READY" 0 \
    health_json_is_ready "$DEGRADED_AXIS_NO_PROVIDER_BODY" 1 1 1
  assert_rc "[$mode] no-provider + degraded axis → predicate matches" 0 \
    _health_json_unhealthy_only_no_provider_runtimes "$DEGRADED_AXIS_NO_PROVIDER_BODY"

  # Finding #3 — skipped_reason must be read from the TOP-LEVEL
  # latest_startup_doctor object (jq: .latest_startup_doctor.skipped_reason).
  # A decoy skipped_reason in another nested object must NOT satisfy the rescue
  # while the real one differs; both modes must reject. Positive control proves
  # the real path is still accepted when a differing decoy is present.
  assert_rc "[$mode] decoy skipped_reason (real differs) → predicate REJECTS" 1 \
    _health_json_unhealthy_only_no_provider_runtimes "$DECOY_SKIPPED_REASON_BODY"
  assert_rc "[$mode] decoy skipped_reason + allow=1 → NOT ready" 1 \
    health_json_is_ready "$DECOY_SKIPPED_REASON_BODY" 1 1 1
  assert_rc "[$mode] real skipped_reason (decoy differs) → predicate matches" 0 \
    _health_json_unhealthy_only_no_provider_runtimes "$REAL_SKIPPED_REASON_WITH_DECOY_BODY"

  # Finding #4 — degraded_reasons must be read from the TOP-LEVEL array
  # (jq: .degraded_reasons). A nested-only reconcile array must NOT satisfy the
  # reconcile gate; both modes must reject. Positive control keeps the genuine
  # top-level reconcile-in-progress path working.
  assert_rc "[$mode] nested-only degraded_reasons → reconcile_only REJECTS" 1 \
    _health_json_reconcile_only "$NESTED_ONLY_DEGRADED_REASONS_BODY"
  assert_rc "[$mode] nested-only degraded_reasons → NOT ready" 1 \
    health_json_is_ready "$NESTED_ONLY_DEGRADED_REASONS_BODY" 1 1
  assert_rc "[$mode] top-level reconcile degraded_reasons → reconcile_only matches" 0 \
    _health_json_reconcile_only "$TOP_LEVEL_RECONCILE_BODY"
  assert_rc "[$mode] top-level reconcile degraded_reasons → READY" 0 \
    health_json_is_ready "$TOP_LEVEL_RECONCILE_BODY" 1 1

  # R2 whitespace regression — a valid top-level degraded_reasons array with a
  # space after `]` must still be accepted via the reconcile path in BOTH modes
  # (the raw extractor now trims the token, so the array cleanup ends at `]`).
  assert_rc "[$mode] degraded_reasons array + trailing ws → reconcile_only matches" 0 \
    _health_json_reconcile_only "$WS_RECONCILE_BODY"
  assert_rc "[$mode] degraded_reasons array + trailing ws → READY" 0 \
    health_json_is_ready "$WS_RECONCILE_BODY" 1 1

  # Scalar whitespace tolerance — surrounding spaces on every top-level value
  # must not break scalar reads; the whole no-provider rescue predicate (which
  # exercises status/db/dashboard/server_up/startup_status/skipped_reason) must
  # still match, and the direct status read must parse cleanly.
  assert_eq "[$mode] status read tolerates surrounding whitespace" \
    "unhealthy" "$(_health_json_status "$WS_SCALAR_NO_PROVIDER_BODY")"
  assert_rc "[$mode] whitespace-padded no-provider body → predicate matches" 0 \
    _health_json_unhealthy_only_no_provider_runtimes "$WS_SCALAR_NO_PROVIDER_BODY"

  assert_rc "[$mode] gateway-only standby degradation → READY" 0 \
    health_json_is_ready "$GATEWAY_STANDBY_BODY" 1 1
  assert_rc "[$mode] gateway-only standby predicate matches" 0 \
    _health_json_gateway_standby_only "$GATEWAY_STANDBY_BODY"
  assert_rc "[$mode] standby plus another degraded reason → NOT ready" 1 \
    health_json_is_ready "$GATEWAY_STANDBY_MIXED_BODY" 1 1
  assert_rc "[$mode] unrecovered standby plus another reason → NOT ready" 1 \
    health_json_is_ready "$GATEWAY_STANDBY_MIXED_UNRECOVERED_BODY" 1 1
  assert_rc "[$mode] gateway reason without cluster_standby → NOT ready" 1 \
    health_json_is_ready "$GATEWAY_REASON_WITHOUT_STANDBY_BODY" 1 1
  assert_rc "[$mode] contradictory healthy standby with mixed reasons → NOT ready" 1 \
    health_json_is_ready "$HEALTHY_STANDBY_MIXED_BODY" 1 1
  assert_rc "[$mode] contradictory healthy standby with empty reasons → NOT ready" 1 \
    health_json_is_ready "$HEALTHY_STANDBY_EMPTY_BODY" 1 1
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
