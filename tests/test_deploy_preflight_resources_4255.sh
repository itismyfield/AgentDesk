#!/usr/bin/env bash
# Unit test for #4255 — deploy pre-flight resource-contention guard in
# scripts/_defaults.sh (called from scripts/deploy-release.sh BEFORE any build).
#
# Two release deploys were KILLED mid-build by resource contention (07-05
# concurrent Unreal Engine build; 07-07 runaway ugrep). The guard refuses an
# expensive `cargo build --release` when the machine is already saturated, and
# must be a NO-OP on a clean machine.
#
# All assertions run against the real helpers sourced from _defaults.sh. Every
# OS probe (pgrep / sysctl-load / mem-pressure / ps) is stubbed so the suite is
# deterministic on ANY machine — including one that happens to have a real
# cargo/rustc build running while the test executes. Self-contained: no service,
# no launchd, no real process inspection.

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

assert_out_contains() {
  # assert_out_contains "<label>" "<needle>" <cmd...>
  local label="$1" needle="$2"; shift 2
  local out
  set +e
  out="$("$@" 2>&1)"
  set -e
  if printf '%s' "$out" | grep -qF -- "$needle"; then pass "$label"; else fail "$label (missing: $needle)"; fi
}

[ -f "$DEFAULTS_SH" ] || { echo "FATAL: $DEFAULTS_SH missing"; exit 2; }
# shellcheck source=/dev/null
. "$DEFAULTS_SH"

# Never inherit an operator's real override into the deterministic suite.
unset AGENTDESK_DEPLOY_FORCE_RESOURCE_PREFLIGHT \
      AGENTDESK_DEPLOY_MAX_LOADAVG \
      AGENTDESK_DEPLOY_MAX_MEM_PRESSURE_LEVEL \
      AGENTDESK_DEPLOY_HIGH_CPU_PCT 2>/dev/null || true

# ── Stubs ────────────────────────────────────────────────────────────────────
# `pgrep` shim: records every invocation's argv so the suite can PROVE the guard
# never uses `pgrep -f` (the self-match trap). Returns a pid ONLY for -x names
# listed in PGREP_MATCH. If the guard ever passed -f, this shim emits a sentinel
# pid to SIMULATE the self-match bug — so a regression flips the clean case red.
PGREP_MATCH=""
PGREP_LOG=""
pgrep() {
  [ -n "$PGREP_LOG" ] && printf '%s\n' "$*" >>"$PGREP_LOG"
  local mode="" name=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      -x) mode="x" ;;
      -f) mode="f" ;;
      -*) ;;
      *) name="$1" ;;
    esac
    shift
  done
  if [ "$mode" = "f" ]; then
    echo 66666   # sentinel: a -f self-match would have returned a pid
    return 0
  fi
  case " $PGREP_MATCH " in
    *" $name "*) echo 55555; return 0 ;;
  esac
  return 1
}

STUB_NCPU=8
reset_clean_stubs() {
  PGREP_MATCH=""
  PGREP_LOG=""
  STUB_NCPU=8
  unset STUB_LOADAVG STUB_PRESSURE STUB_HIGHCPU 2>/dev/null || true
  unset AGENTDESK_DEPLOY_FORCE_RESOURCE_PREFLIGHT \
        AGENTDESK_DEPLOY_MAX_LOADAVG \
        AGENTDESK_DEPLOY_MAX_MEM_PRESSURE_LEVEL \
        AGENTDESK_DEPLOY_HIGH_CPU_PCT 2>/dev/null || true
  _preflight_cpu_count() { printf '%s' "${STUB_NCPU:-8}"; }
  _preflight_loadavg_1min() { printf '%s' "${STUB_LOADAVG:-1.00}"; }
  _preflight_mem_pressure_level() { printf '%s' "${STUB_PRESSURE:-1}"; }
  _preflight_high_cpu_processes() {
    if [ -n "${STUB_HIGHCPU:-}" ]; then
      printf '%s\n' "$STUB_HIGHCPU"
    fi
    return 0
  }
}
reset_clean_stubs

# ── Pure helpers ─────────────────────────────────────────────────────────────
echo "== Pure numeric helpers =="
assert_rc "_preflight_num_gt 25 > 21 → true"            0 _preflight_num_gt "25" "21"
assert_rc "_preflight_num_gt 3.70 > 21 → false"         1 _preflight_num_gt "3.70" "21"
assert_rc "_preflight_num_gt 21.00 > 21.00 (equal) → false" 1 _preflight_num_gt "21.00" "21.00"
assert_rc "_preflight_num_gt abc > 21 (non-numeric) → false" 1 _preflight_num_gt "abc" "21"
assert_rc "_preflight_num_gt 25 > '' (empty) → false"   1 _preflight_num_gt "25" ""

echo "== Default load ceiling = 1.5 × logical CPUs (empty when count unreadable) =="
STUB_NCPU=8
assert_eq "default max loadavg for 8 cores" "12.00" "$(_preflight_default_max_loadavg)"
STUB_NCPU=14
assert_eq "default max loadavg for 14 cores" "21.00" "$(_preflight_default_max_loadavg)"
# Unreadable CPU count → NO fabricated ceiling (fail-open, #4255 review #2).
_preflight_cpu_count() { return 0; }
assert_eq "default ceiling is empty when CPU count is unreadable" "" "$(_preflight_default_max_loadavg)"
reset_clean_stubs

echo "== Real load-average parse (sysctl shim) =="
# Restore the REAL parsers, then feed them a low-level `sysctl` shim.
# shellcheck source=/dev/null
. "$DEFAULTS_SH"
sysctl() {
  case "$*" in
    *vm.loadavg*) echo "{ 3.70 3.15 3.03 }" ;;
    *hw.ncpu*) echo 8 ;;
    *memorystatus_vm_pressure_level*) echo 2 ;;
    *) return 1 ;;
  esac
}
assert_eq "loadavg parsed from '{ 3.70 ... }'" "3.70" "$(_preflight_loadavg_1min)"
assert_eq "cpu count parsed from sysctl hw.ncpu" "8" "$(_preflight_cpu_count)"
assert_eq "mem pressure level parsed from sysctl" "2" "$(_preflight_mem_pressure_level)"
unset -f sysctl
reset_clean_stubs

echo "== Real high-CPU scan (ps shim) — threshold filter + self-pgid exclusion =="
# Restore the REAL scanner, then feed it a low-level `ps` shim + fixed self pgid.
# shellcheck source=/dev/null
. "$DEFAULTS_SH"
# Row 2 shares the deploy's own pgid (24835) and MUST be excluded even at 99.9%.
STUB_PS_ROWS="$(printf '100 100 95.0 /usr/bin/ugrep\n200 24835 99.9 cargo\n300 300 10.0 /usr/bin/idle')"
ps() { printf '%s\n' "$STUB_PS_ROWS"; }
_preflight_self_pgid() { printf '%s' "24835"; }
assert_eq "high-CPU@90 → only the non-self hot proc (ugrep) reported" \
  "100	95.0	/usr/bin/ugrep" "$(_preflight_high_cpu_processes 90)"
assert_eq "high-CPU@99 → 95%% proc below threshold → empty" \
  "" "$(_preflight_high_cpu_processes 99)"
unset -f ps _preflight_self_pgid
reset_clean_stubs

echo "== Exact-name builder detection (pgrep -x shim) =="
PGREP_MATCH="cargo rustc"
assert_eq "_preflight_builder_pids cargo → pid" "55555" "$(_preflight_builder_pids cargo)"
assert_eq "_preflight_builder_pids sleep (absent) → empty" "" "$(_preflight_builder_pids sleep)"
reset_clean_stubs

# ── Orchestrator: _preflight_resource_contention ─────────────────────────────
echo "== Clean machine → NO-OP (must never block a normal deploy) =="
reset_clean_stubs
assert_rc "clean machine → pre-flight passes" 0 _preflight_resource_contention

echo "== Self-match trap: pgrep is used, but NEVER 'pgrep -f' =="
reset_clean_stubs
SELF_LOG="$(mktemp)"
PGREP_LOG="$SELF_LOG"
assert_rc "clean machine (with pgrep logging) → passes" 0 _preflight_resource_contention
if [ -s "$SELF_LOG" ] && ! grep -qE '(^| )-f( |$)' "$SELF_LOG"; then
  pass "builder detection invoked pgrep but never 'pgrep -f' (self-match trap avoided)"
else
  fail "builder detection skipped pgrep or used -f (self-match risk)"
fi
if grep -qE '(^| )-x( |$)' "$SELF_LOG"; then
  pass "builder detection used exact-name 'pgrep -x'"
else
  fail "builder detection did not use 'pgrep -x'"
fi
rm -f "$SELF_LOG"
reset_clean_stubs

echo "== Concurrent builders (cargo / rustc) → REFUSE with named cause =="
reset_clean_stubs
PGREP_MATCH="cargo"
assert_rc "cargo present → refuse" 1 _preflight_resource_contention
assert_out_contains "cargo refusal names the tool" "cargo" _preflight_resource_contention
assert_out_contains "cargo refusal names the pid" "55555" _preflight_resource_contention
reset_clean_stubs
PGREP_MATCH="rustc"
assert_rc "rustc present → refuse" 1 _preflight_resource_contention
assert_out_contains "rustc refusal names the tool" "rustc" _preflight_resource_contention
reset_clean_stubs
# 07-05 historical incident: a concurrent Unreal Engine build. The exact-name
# builder gate refuses it on its own, independent of load/memory corroboration.
PGREP_MATCH="UnrealEditor"
assert_rc "INCIDENT 07-05: UnrealEditor build present → refuse" 1 _preflight_resource_contention
assert_out_contains "07-05 refusal names UnrealEditor" "UnrealEditor" _preflight_resource_contention
reset_clean_stubs

echo "== Load-average gate + env-var threshold override =="
reset_clean_stubs
STUB_LOADAVG="25.0"
export AGENTDESK_DEPLOY_MAX_LOADAVG="10"
assert_rc "loadavg 25 > ceiling 10 → refuse" 1 _preflight_resource_contention
assert_out_contains "loadavg refusal names the metric" "load average" _preflight_resource_contention
export AGENTDESK_DEPLOY_MAX_LOADAVG="100"
assert_rc "loadavg 25 <= overridden ceiling 100 → pass" 0 _preflight_resource_contention
reset_clean_stubs

echo "== Fail-OPEN: unreadable CPU count SKIPS the load probe (never blocks) =="
reset_clean_stubs
_preflight_cpu_count() { return 0; }   # simulate unreadable hw.ncpu / nproc
STUB_LOADAVG="99.0"                     # very high load, but no ceiling to compare
assert_rc "unreadable ncpu + high load + no override → probe skipped, proceeds" 0 _preflight_resource_contention
assert_out_contains "clear line marks the load ceiling skipped" "skipped" _preflight_resource_contention
# An explicit operator ceiling needs no core count → the gate STILL evaluates.
export AGENTDESK_DEPLOY_MAX_LOADAVG="10"
assert_rc "unreadable ncpu + explicit ceiling 10 + load 99 → refuse" 1 _preflight_resource_contention
reset_clean_stubs

echo "== Memory-pressure gate + env-var threshold override =="
reset_clean_stubs
STUB_PRESSURE="4"   # critical
assert_rc "mem pressure 4 (critical) >= default ceiling 4 → refuse" 1 _preflight_resource_contention
assert_out_contains "mem-pressure refusal names the metric" "memory pressure" _preflight_resource_contention
STUB_PRESSURE="2"   # warn — below default critical ceiling
assert_rc "mem pressure 2 (warn) < default ceiling 4 → pass" 0 _preflight_resource_contention
STUB_PRESSURE="4"
export AGENTDESK_DEPLOY_MAX_MEM_PRESSURE_LEVEL="5"
assert_rc "mem pressure 4 < overridden ceiling 5 → pass" 0 _preflight_resource_contention
reset_clean_stubs

echo "== High-CPU process needs CORROBORATION (no lone-hot-core false positive) =="
# A lone hot process on an otherwise-idle machine is ADVISORY only — proceed.
reset_clean_stubs
STUB_HIGHCPU="$(printf '4242\t97.0\trust-analyzer')"
assert_rc "lone high-CPU proc, no load/mem pressure → advisory, proceeds" 0 _preflight_resource_contention
assert_out_contains "lone high-CPU proc surfaced as advisory" "advisory" _preflight_resource_contention
assert_out_contains "advisory still names the process" "rust-analyzer" _preflight_resource_contention
reset_clean_stubs

# 07-07 historical incident: a runaway/zombie ugrep that pegged CPU AND drove the
# machine into real contention. Corroborated by LOAD over ceiling → refuse, named.
export AGENTDESK_DEPLOY_MAX_LOADAVG="10"
STUB_LOADAVG="25.0"
STUB_HIGHCPU="$(printf '99999\t95.0\tugrep')"
assert_rc "INCIDENT 07-07: runaway ugrep + load over ceiling → refuse" 1 _preflight_resource_contention
assert_out_contains "07-07 refusal names ugrep" "ugrep" _preflight_resource_contention
assert_out_contains "07-07 refusal names the pid" "99999" _preflight_resource_contention
assert_out_contains "07-07 refusal cites the corroborating load" "load average" _preflight_resource_contention
reset_clean_stubs

# Same hot process corroborated by MEMORY pressure (critical) instead → refuse.
STUB_PRESSURE="4"
STUB_HIGHCPU="$(printf '99999\t95.0\tugrep')"
assert_rc "runaway ugrep + critical memory pressure → refuse" 1 _preflight_resource_contention
assert_out_contains "mem-corroborated refusal names ugrep" "ugrep" _preflight_resource_contention
reset_clean_stubs

echo "== Force escape hatch → proceed past a real finding (still warns) =="
reset_clean_stubs
PGREP_MATCH="cargo"
export AGENTDESK_DEPLOY_FORCE_RESOURCE_PREFLIGHT="1"
assert_rc "cargo present + FORCE=1 → proceed (rc 0)" 0 _preflight_resource_contention
assert_out_contains "force path still prints the finding" "cargo" _preflight_resource_contention
assert_out_contains "force path says proceeding anyway" "proceeding anyway" _preflight_resource_contention
reset_clean_stubs

echo "== No false positive from the deploy script's own process name =="
# The guard only ever asks for EXACT tool names (cargo/rustc/UE builders); the
# deploy script's comm is `bash`, and the ssh client / sshd / peer shell are
# `ssh`/`sshd`/`bash` — none of which are exact-name build tools. Simulate the
# deploy's own name being "present" and confirm it is NOT counted.
reset_clean_stubs
PGREP_MATCH="bash deploy-release.sh ssh sshd"
assert_rc "deploy-script / ssh / sshd names present but not build tools → pass" 0 _preflight_resource_contention
reset_clean_stubs

echo
echo "==== Results ===="
echo "  PASS: $PASS"
echo "  FAIL: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  printf '  failed: %s\n' "${FAIL_NAMES[@]}" >&2
  exit 1
fi
exit 0
