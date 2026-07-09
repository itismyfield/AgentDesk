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
  unset STUB_LOADAVG STUB_PRESSURE STUB_HIGHCPU STUB_TARGET_PIDS 2>/dev/null || true
  unset AGENTDESK_DEPLOY_FORCE_RESOURCE_PREFLIGHT \
        AGENTDESK_DEPLOY_MAX_LOADAVG \
        AGENTDESK_DEPLOY_MAX_MEM_PRESSURE_LEVEL \
        AGENTDESK_DEPLOY_HIGH_CPU_PCT 2>/dev/null || true
  # Fixed release root so _preflight_release_binary is deterministic and never
  # collides with the operator's real ~/.adk/release path. Read by
  # _preflight_release_binary in the sourced _defaults.sh, not by this file.
  # shellcheck disable=SC2034
  ADK_REL="/tmp/adk-preflight-test-release"
  _preflight_cpu_count() { printf '%s' "${STUB_NCPU:-8}"; }
  _preflight_loadavg_1min() { printf '%s' "${STUB_LOADAVG:-1.00}"; }
  _preflight_mem_pressure_level() { printf '%s' "${STUB_PRESSURE:-1}"; }
  # Deterministic: never shell out to the host's real launchctl.
  _preflight_deploy_target_pids() { [ -n "${STUB_TARGET_PIDS:-}" ] && printf '%s\n' "$STUB_TARGET_PIDS"; return 0; }
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

echo "== ps duration parser (_preflight_ps_duration_to_seconds) — fails open =="
assert_eq "MM:SS 5:03 → 303"                    "303"    "$(_preflight_ps_duration_to_seconds '5:03')"
assert_eq "HH:MM:SS 1:05:03 → 3903"             "3903"   "$(_preflight_ps_duration_to_seconds '1:05:03')"
assert_eq "DD-HH:MM:SS 2-03:04:05 → 183845"     "183845" "$(_preflight_ps_duration_to_seconds '2-03:04:05')"
assert_eq "SS-only 45 → 45"                     "45"     "$(_preflight_ps_duration_to_seconds '45')"
assert_eq "fractional 9:52.81 → 592 (frac dropped)" "592" "$(_preflight_ps_duration_to_seconds '9:52.81')"
assert_eq "octal-safe leading zeros 00:08 → 8"  "8"      "$(_preflight_ps_duration_to_seconds '00:08')"
assert_eq "malformed 'abc' → empty (fail open)" ""       "$(_preflight_ps_duration_to_seconds 'abc')"
assert_eq "malformed 4-field 1:2:3:4 → empty"   ""       "$(_preflight_ps_duration_to_seconds '1:2:3:4')"
assert_eq "empty input → empty"                 ""       "$(_preflight_ps_duration_to_seconds '')"

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
# Columns: pid pgid %cpu etime time comm. Row 2 shares the deploy's own pgid
# (24835) and MUST be excluded even at 99.9%.
STUB_PS_ROWS="$(printf '100 100 95.0 04:00:00 03:59:00 /usr/bin/ugrep\n200 24835 99.9 01:00 00:59 cargo\n300 300 10.0 10:00 00:30 /usr/bin/idle')"
ps() { printf '%s\n' "$STUB_PS_ROWS"; }
_preflight_self_pgid() { printf '%s' "24835"; }
assert_eq "high-CPU@90 → only the non-self hot proc (ugrep) with durations" \
  "100	95.0	04:00:00	03:59:00	/usr/bin/ugrep" "$(_preflight_high_cpu_processes 90)"
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

echo "== SUSTAINED runaway → HARD refuse ON ITS OWN (faithful 07-07, no pressure) =="
# 07-07: a zombie/runaway ugrep pegging ONE core. On a 14-core host that is
# loadavg ~1 (nowhere near the 21.00 ceiling) and memory is fine — NO system
# pressure. It refuses purely because it has been CPU-pegged for its whole long
# life (elapsed 4h, cpu-time ~4h → ratio ~1). This is the shape the old
# corroboration rule MISSED.
reset_clean_stubs
STUB_NCPU=14                # default ceiling 21.00; load 1.00 is far under it
STUB_LOADAVG="1.00"        # NO load pressure
STUB_PRESSURE="1"          # NO memory pressure
STUB_HIGHCPU="$(printf '99999\t95.0\t04:00:00\t03:59:00\tugrep')"
assert_rc "INCIDENT 07-07: sustained ugrep, no system pressure → refuse" 1 _preflight_resource_contention
assert_out_contains "07-07 refusal names ugrep" "ugrep" _preflight_resource_contention
assert_out_contains "07-07 refusal names the pid" "99999" _preflight_resource_contention
assert_out_contains "07-07 refusal says SUSTAINED runaway" "SUSTAINED" _preflight_resource_contention
reset_clean_stubs

echo "== Legitimate long encode (ffmpeg) — JUDGEMENT CALL: SHOULD block =="
# A 30-min ffmpeg pegged at 99% (ratio 0.9) is a sustained runaway by the rule.
# CALL: it SHOULD refuse — it will contend for the entire build; the operator
# can force through if they judge a single-thread encode harmless.
reset_clean_stubs
STUB_HIGHCPU="$(printf '888\t99.0\t30:00\t27:00\tffmpeg')"
assert_rc "ffmpeg 99%% 30min ratio 0.9 (sustained) → refuse" 1 _preflight_resource_contention
assert_out_contains "ffmpeg refusal names the process" "ffmpeg" _preflight_resource_contention
assert_out_contains "ffmpeg refusal says SUSTAINED" "SUSTAINED" _preflight_resource_contention
reset_clean_stubs

echo "== False-positive guards — must PROCEED (rc 0, advisory only) =="
# (a) fresh rust-analyzer reindex: high ratio but BELOW the min-elapsed floor.
reset_clean_stubs
STUB_HIGHCPU="$(printf '4242\t97.0\t01:30\t01:29\trust-analyzer')"
assert_rc "fresh rust-analyzer (90s, ratio ~1, below min-elapsed) → proceed" 0 _preflight_resource_contention
assert_out_contains "rust-analyzer surfaced as advisory" "advisory" _preflight_resource_contention
assert_out_contains "advisory names rust-analyzer" "rust-analyzer" _preflight_resource_contention
reset_clean_stubs
# (b) long-lived but BURSTY mdworker: 2h elapsed, only 6m CPU → ratio 0.05.
STUB_HIGHCPU="$(printf '777\t95.0\t02:00:00\t06:00\tmdworker')"
assert_rc "bursty mdworker (2h elapsed, ratio 0.05) → proceed" 0 _preflight_resource_contention
assert_out_contains "mdworker surfaced as advisory" "advisory" _preflight_resource_contention
reset_clean_stubs
# Unparseable durations → cannot classify as runaway → fail OPEN (advisory).
STUB_HIGHCPU="$(printf '555\t95.0\tabc\txyz\tmystery')"
assert_rc "unparseable etime/time + no pressure → proceed (fail open)" 0 _preflight_resource_contention
assert_out_contains "unparseable-duration proc surfaced as advisory" "advisory" _preflight_resource_contention
reset_clean_stubs

echo "== Corroboration path preserved: hot+BURSTY still refuses under system pressure =="
# A BURSTY hot process (ratio 0.1, NOT a sustained runaway) refuses only when the
# machine is under system-wide pressure — proving the multi-process saturation
# path still fires independently of the runaway rule.
export AGENTDESK_DEPLOY_MAX_LOADAVG="10"
STUB_LOADAVG="25.0"
STUB_HIGHCPU="$(printf '4321\t95.0\t02:00:00\t12:00\tbursty-hog')"
assert_rc "bursty hot proc + load over ceiling → refuse" 1 _preflight_resource_contention
assert_out_contains "load-corroborated refusal names the proc" "bursty-hog" _preflight_resource_contention
assert_out_contains "load-corroborated refusal cites system pressure" "system-wide" _preflight_resource_contention
reset_clean_stubs
STUB_PRESSURE="4"
STUB_HIGHCPU="$(printf '4321\t95.0\t02:00:00\t12:00\tbursty-hog')"
assert_rc "bursty hot proc + critical memory pressure → refuse" 1 _preflight_resource_contention
assert_out_contains "mem-corroborated refusal names the proc" "bursty-hog" _preflight_resource_contention
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

echo "== Deploy target (release dcserver) never refuses its own deploy (#4255 review r3) =="
# SELF-LOCK REGRESSION. The dcserver is multi-threaded: `ps` cumulative CPU time
# is summed across threads, so a merely busy daemon reaches cpu-time >= 0.8 ×
# elapsed with NO machine-wide load/memory pressure — indistinguishable, to the
# ratio test, from the 07-07 single-core zombie. The old code hard-refused there,
# so a busy release dcserver locked out its own deploy on every node. The deploy
# RESTARTS that process; its load is the thing being replaced, not contention.
reset_clean_stubs
STUB_NCPU=14; STUB_LOADAVG="1.00"; STUB_PRESSURE="1"   # no system pressure at all
TARGET_BIN="/tmp/adk-preflight-test-release/bin/agentdesk"

# `ps -ww -o args= -p <pid>` shim: the release binary is MULTI-COMMAND, so argv —
# not the executable path — decides whether a pid is the dcserver.
ps() {
  # `${*##* }` would strip the prefix from EACH positional param, not the joined
  # string — take the last argument (the pid) explicitly.
  local pid=""
  while [ "$#" -gt 0 ]; do pid="$1"; shift; done
  case "$pid" in
    94068|77777) printf '%s dcserver\n' "$TARGET_BIN" ;;
    66666)       printf '%s codex-tmux-wrapper\n' "$TARGET_BIN" ;;  # same path, NOT the dcserver
    31337)       printf '/Users/someone/.adk/dev/bin/agentdesk dcserver\n' ;;
    *)           return 1 ;;
  esac
}

# (a) matched by the launchd job PID (argv need not be consulted).
STUB_TARGET_PIDS="94068"
STUB_HIGHCPU="$(printf '94068\t99.0\t12:00\t11:50\t%s' "$TARGET_BIN")"
assert_rc "deploy target matched by launchd PID → proceed" 0 _preflight_resource_contention
assert_out_contains "target surfaced as advisory, not a refusal" "DEPLOY TARGET" _preflight_resource_contention
reset_clean_stubs

# (b) matched by EXACT executable path + `dcserver` argv when launchctl yields no
# PID (job loaded-but-not-running, or a tmux-fallback dcserver launchd does not own).
STUB_NCPU=14; STUB_LOADAVG="1.00"; STUB_PRESSURE="1"
STUB_HIGHCPU="$(printf '77777\t99.0\t12:00\t11:50\t%s' "$TARGET_BIN")"
assert_rc "deploy target matched by exact path + dcserver argv → proceed" 0 _preflight_resource_contention
reset_clean_stubs

# (b2) MULTI-COMMAND GUARD (#4255 review r4): a sustained runaway launched from the
# SAME release binary path but running a different subcommand is NOT the deploy
# target — the deploy does not restart it, so it must still hard-refuse.
STUB_NCPU=14; STUB_LOADAVG="1.00"; STUB_PRESSURE="1"
STUB_HIGHCPU="$(printf '66666\t99.0\t12:00\t11:50\t%s' "$TARGET_BIN")"
assert_rc "same release binary, non-dcserver subcommand → still refuse" 1 _preflight_resource_contention
assert_out_contains "non-dcserver subcommand refusal says SUSTAINED" "SUSTAINED" _preflight_resource_contention
reset_clean_stubs

# (c) OVER-MATCH GUARD: a dev-tree binary with the SAME basename `agentdesk` at a
# DIFFERENT path is NOT the deploy target and must still hard-refuse. This is why
# the whitelist keys on launchd PID / full path and never on `pgrep -x agentdesk`.
STUB_NCPU=14; STUB_LOADAVG="1.00"; STUB_PRESSURE="1"
STUB_HIGHCPU="$(printf '31337\t99.0\t12:00\t11:50\t/Users/someone/.adk/dev/bin/agentdesk')"
assert_rc "dev-tree agentdesk (same basename, other path) → still refuse" 1 _preflight_resource_contention
assert_out_contains "dev-tree agentdesk refusal says SUSTAINED" "SUSTAINED" _preflight_resource_contention
reset_clean_stubs

# (d) The whitelist exempts ONLY the target — a foreign runaway alongside it still refuses.
STUB_NCPU=14; STUB_LOADAVG="1.00"; STUB_PRESSURE="1"
STUB_HIGHCPU="$(printf '77777\t99.0\t12:00\t11:50\t%s\n99999\t95.0\t04:00:00\t03:59:00\tugrep' "$TARGET_BIN")"
assert_rc "deploy target + foreign sustained runaway → refuse" 1 _preflight_resource_contention
assert_out_contains "refusal names the foreign runaway, not the target" "ugrep" _preflight_resource_contention
reset_clean_stubs

# (e) The whitelist must NOT mask machine-wide signals: load over ceiling still refuses.
STUB_NCPU=14; STUB_LOADAVG="25.00"; STUB_PRESSURE="1"   # 25.00 > default 21.00 ceiling
STUB_HIGHCPU="$(printf '77777\t99.0\t12:00\t11:50\t%s' "$TARGET_BIN")"
assert_rc "deploy target hot + machine-wide load over ceiling → still refuse" 1 _preflight_resource_contention
assert_out_contains "machine-wide refusal cites load average" "load average" _preflight_resource_contention
reset_clean_stubs

echo "== Real launchd PID parser (launchctl shim) =="
# Restore the REAL _preflight_deploy_target_pids, then feed it a `launchctl` shim
# emitting the plist dump shape that `launchctl list <label>` actually prints.
# shellcheck source=/dev/null
. "$DEFAULTS_SH"
launchctl() {
  case "$*" in
    *"list com.agentdesk.release"*) printf '{\n\t"LimitLoadToSessionType" = "Aqua";\n\t"PID" = 94068;\n\t"LastExitStatus" = 0;\n}\n' ;;
    *"list com.agentdesk.loaded-not-running"*) printf '{\n\t"LastExitStatus" = 0;\n}\n' ;;
    *) return 1 ;;
  esac
}
assert_eq "launchd PID parsed from plist dump" "94068" "$(AGENTDESK_DCSERVER_LABEL=com.agentdesk.release _preflight_deploy_target_pids)"
assert_eq "loaded-but-not-running → empty (no PID key)" "" "$(AGENTDESK_DCSERVER_LABEL=com.agentdesk.loaded-not-running _preflight_deploy_target_pids)"
assert_eq "unknown label → empty (launchctl rc!=0)" "" "$(AGENTDESK_DCSERVER_LABEL=com.agentdesk.nope _preflight_deploy_target_pids)"
unset -f launchctl
assert_eq "release binary path honors ADK_REL" "/tmp/adk-preflight-test-release/bin/agentdesk" \
  "$(ADK_REL=/tmp/adk-preflight-test-release _preflight_release_binary)"
# Exact-match predicate, against the real helpers + the `ps` argv shim above.
assert_rc "_preflight_is_deploy_target: launchd pid match (argv not consulted)" 0 \
  _preflight_is_deploy_target "94068" "whatever" "$(printf '111\n94068\n')" "$TARGET_BIN"
assert_rc "_preflight_is_deploy_target: path + dcserver argv match" 0 \
  _preflight_is_deploy_target "77777" "$TARGET_BIN" "" "$TARGET_BIN"
assert_rc "_preflight_is_deploy_target: same path, other subcommand → no match" 1 \
  _preflight_is_deploy_target "66666" "$TARGET_BIN" "" "$TARGET_BIN"
assert_rc "_preflight_is_deploy_target: basename-only is NOT a match" 1 \
  _preflight_is_deploy_target "31337" "/Users/someone/.adk/dev/bin/agentdesk" "" "$TARGET_BIN"
assert_rc "_preflight_is_deploy_target: empty pid → no match" 1 \
  _preflight_is_deploy_target "" "$TARGET_BIN" "" "$TARGET_BIN"
assert_rc "_preflight_is_deploy_target: unreadable argv → fails CLOSED" 1 \
  _preflight_is_deploy_target "424242" "$TARGET_BIN" "" "$TARGET_BIN"
# argv predicate in isolation.
assert_rc "_preflight_process_is_release_dcserver: dcserver argv" 0 _preflight_process_is_release_dcserver "77777" "$TARGET_BIN"
assert_rc "_preflight_process_is_release_dcserver: wrapper subcommand" 1 _preflight_process_is_release_dcserver "66666" "$TARGET_BIN"
assert_rc "_preflight_process_is_release_dcserver: other binary" 1 _preflight_process_is_release_dcserver "31337" "$TARGET_BIN"
unset -f ps
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
