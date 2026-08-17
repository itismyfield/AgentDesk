#!/usr/bin/env bash
# Unit test for #5438 — scripts/lane_headroom.sh, the executable single
# authority for lane-admission resource judgment.
#
# What this pins:
#   • the OUTPUT CONTRACT: stdout line 1 is exactly `HEADROOM: OK` or
#     `HEADROOM: NO <space-separated reason tokens>` (tokens never contain a
#     space, so a caller can field-split line 1),
#   • rc semantics (0 = OK, 1 = NO, 2 = usage error),
#   • the presence of every failure-reason string, one per gated metric, plus
#     the fail-CLOSED `unreadable` reasons — for a probe that printed nothing
#     AND for one that printed a parseable reading but exited nonzero,
#   • the canonical thresholds at their exact boundaries (idle 30 / free 25 /
#     pressure 1 / swap-rate 10 / disk 50) — this file is where a silent
#     threshold edit gets caught,
#   • the #5438 regression itself: a `top` PhysMem line reporting almost no
#     "unused" memory must NOT influence the memory verdict.
#
# EVERY OS probe (top / memory_pressure / sysctl / vm_stat / df / pgrep) is
# stubbed on PATH, so the suite is deterministic on any host and asserts
# nothing about the real machine — including a Linux CI runner that has no
# memory_pressure at all.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LANE_HEADROOM="$REPO_ROOT/scripts/lane_headroom.sh"

PASS=0
FAIL=0
FAIL_NAMES=()

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1" >&2; FAIL=$((FAIL + 1)); FAIL_NAMES+=("$1"); }

assert_eq() {
  local label="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then pass "$label"; else fail "$label (expected='$expected' actual='$actual')"; fi
}

assert_contains() {
  local label="$1" needle="$2" haystack="$3"
  if printf '%s' "$haystack" | grep -qF -- "$needle"; then pass "$label"; else fail "$label (missing '$needle' in: $haystack)"; fi
}

assert_not_contains() {
  local label="$1" needle="$2" haystack="$3"
  if printf '%s' "$haystack" | grep -qF -- "$needle"; then fail "$label (unexpected '$needle' in: $haystack)"; else pass "$label"; fi
}

[ -f "$LANE_HEADROOM" ] || { echo "FATAL: $LANE_HEADROOM missing"; exit 2; }

TMP_ROOT="$(mktemp -d)"
trap 'rm -rf "$TMP_ROOT"' EXIT
STUB_DIR="$TMP_ROOT/bin"
STATE_DIR="$TMP_ROOT/state"
mkdir -p "$STUB_DIR" "$STATE_DIR"

write_stub() {
  local name="$1"
  cat > "$STUB_DIR/$name"
  chmod +x "$STUB_DIR/$name"
}

# Two distinct failure knobs per probe, because they are different bugs:
#   STUB_<X>_BROKEN=1 — exits 1 printing NOTHING (the probe is dead).
#   STUB_<X>_RC=1     — prints its NORMAL, fully parseable output and THEN
#                       exits 1 (the probe lied about succeeding). Section 13
#                       pins that this second shape also fails closed; before
#                       the #5438 rc fix, all five of these returned OK.

# `CPU usage:` idle field is the only thing the script may read from top. The
# PhysMem line is emitted verbatim in its real (misleading) shape so the
# #5438 regression test below has something to trip on.
write_stub top <<'STUB'
#!/usr/bin/env bash
echo "Processes: 1175 total, 7 running, 1168 sleeping, 11506 threads"
echo "Load Avg: 7.77, 10.06, 10.19"
echo "CPU usage: 10.0% user, 5.0% sys, ${STUB_CPU_IDLE:-85.0}% idle"
echo "PhysMem: 47G used (9314M wired, 4330M compressor), ${STUB_TOP_UNUSED:-451M} unused."
exit "${STUB_TOP_RC:-0}"
STUB

write_stub memory_pressure <<'STUB'
#!/usr/bin/env bash
[ "${STUB_MEMORY_PRESSURE_BROKEN:-0}" = "1" ] && exit 1
echo "The system has 51539607552 (3145728 pages with a page size of 16384)."
echo ""
echo "Swap I/O:"
echo "Swapins: 39350080 "
echo "Swapouts: ${STUB_MP_SWAPOUTS:-1000} "
echo ""
echo "System-wide memory free percentage: ${STUB_MEM_FREE:-71}%"
exit "${STUB_MEMORY_PRESSURE_RC:-0}"
STUB

write_stub sysctl <<'STUB'
#!/usr/bin/env bash
case "$*" in
  *kern.memorystatus_vm_pressure_level*)
    [ "${STUB_SYSCTL_BROKEN:-0}" = "1" ] && exit 1
    echo "${STUB_MEM_PRESSURE_LEVEL:-1}"
    exit "${STUB_SYSCTL_RC:-0}"
    ;;
  *) exit 1 ;;
esac
STUB

# Cumulative counter: each invocation advances by STUB_SWAPOUTS_STEP, so the
# script's two samples see a real delta without any sleep.
write_stub vm_stat <<'STUB'
#!/usr/bin/env bash
[ "${STUB_VM_STAT_BROKEN:-0}" = "1" ] && exit 1
state="${STUB_STATE_DIR:-/tmp}/vm_stat_calls"
n="$(cat "$state" 2>/dev/null || echo 0)"
printf '%s' "$((n + 1))" > "$state"
echo "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
echo "Pages free:                                     9733."
echo "Swapins:                                    39350080."
echo "Swapouts:                                   $(( ${STUB_SWAPOUTS_BASE:-42620705} + n * ${STUB_SWAPOUTS_STEP:-0} ))."
exit "${STUB_VM_STAT_RC:-0}"
STUB

write_stub df <<'STUB'
#!/usr/bin/env bash
[ "${STUB_DF_BROKEN:-0}" = "1" ] && exit 1
echo "Filesystem     1024-blocks      Used Available Capacity iused      ifree %iused  Mounted on"
echo "/dev/stub        971350180  16240912 ${STUB_DISK_AVAIL_KB:-182742968}     9%  480435 1827429680    0%   /"
exit "${STUB_DF_RC:-0}"
STUB

write_stub pgrep <<'STUB'
#!/usr/bin/env bash
[ "${STUB_PGREP_BROKEN:-0}" = "1" ] && exit 1
name="${2:-}"
case "$name" in
  cargo) n="${STUB_CARGO_N:-0}" ;;
  rustc) n="${STUB_RUSTC_N:-0}" ;;
  *) n=0 ;;
esac
[ "$n" -gt 0 ] || exit 1
i=1
while [ "$i" -le "$n" ]; do echo "$((1000 + i))"; i=$((i + 1)); done
STUB

# A "vm_stat is GONE" case needs the command genuinely off PATH: an exit-1
# vm_stat is a FAILED probe (section 13 — fails closed), not a missing one. So
# section 8 runs against a sanitized PATH — the stubs minus vm_stat, plus
# symlinks to exactly the real utilities lane_headroom.sh itself calls. Without
# the sanitizing, /usr/bin/vm_stat would answer and the case would assert
# nothing about the fallback.
SYSBIN="$TMP_ROOT/sysbin"
STUB_DIR_NO_VM_STAT="$TMP_ROOT/bin-no-vm_stat"
mkdir -p "$SYSBIN" "$STUB_DIR_NO_VM_STAT"
for util in awk bash basename cat date dirname sleep; do
  real="$(command -v "$util")" || { echo "FATAL: $util not on PATH"; exit 2; }
  ln -sf "$real" "$SYSBIN/$util"
done
for stub in "$STUB_DIR"/*; do
  [ "$(basename "$stub")" = "vm_stat" ] || ln -sf "$stub" "$STUB_DIR_NO_VM_STAT/"
done
NO_VM_STAT_PATH="$STUB_DIR_NO_VM_STAT:$SYSBIN"

OUT=""
RC=0
LINE1=""
HEADROOM_PATH=""
run_headroom() {
  # run_headroom [VAR=VAL ...] — every override is a stub knob or a
  # LANE_HEADROOM_* threshold. Captures OUT / RC / LINE1. Set HEADROOM_PATH to
  # run against a PATH other than the full stub set (see section 8).
  rm -f "$STATE_DIR/vm_stat_calls"
  OUT="$(env "PATH=${HEADROOM_PATH:-$STUB_DIR:$PATH}" \
    "STUB_STATE_DIR=$STATE_DIR" \
    "LANE_HEADROOM_SWAP_SAMPLE_SECONDS=${SWAP_SAMPLE_SECONDS:-0}" \
    "$@" bash "$LANE_HEADROOM" 2>&1)"
  RC=$?
  LINE1="$(printf '%s\n' "$OUT" | head -1)"
}

echo "== 1. clean machine → OK contract =="
run_headroom
assert_eq "clean verdict line is exactly 'HEADROOM: OK'" "HEADROOM: OK" "$LINE1"
assert_eq "clean rc is 0" "0" "$RC"
assert_eq "exactly one HEADROOM: line" "1" "$(printf '%s\n' "$OUT" | grep -c '^HEADROOM:')"
for label in cpu-idle mem-free mem-pressure swapout-rate disk-free builders; do
  assert_contains "detail line present for $label" "$label" "$(printf '%s\n' "$OUT" | tail -n +2)"
done

echo "== 2. line-1 regex contract (both verdicts) =="
if [[ "$LINE1" =~ ^HEADROOM:\ OK$ ]]; then pass "OK line matches ^HEADROOM: OK\$"; else fail "OK line regex ($LINE1)"; fi
run_headroom STUB_CPU_IDLE=5.0 STUB_MEM_FREE=9 STUB_MEM_PRESSURE_LEVEL=4
if [[ "$LINE1" =~ ^HEADROOM:\ NO(\ [^[:space:]]+)+$ ]]; then
  pass "NO line matches ^HEADROOM: NO( <token>)+\$"
else
  fail "NO line regex ($LINE1)"
fi
assert_eq "saturated rc is 1" "1" "$RC"
assert_eq "NO verdict is still a single line" "1" "$(printf '%s\n' "$OUT" | grep -c '^HEADROOM:')"

echo "== 3. one reason string per gated metric =="
run_headroom STUB_CPU_IDLE=12.5
assert_contains "low CPU idle names cpu-idle" "cpu-idle=12.5%(min=30%)" "$LINE1"
assert_eq "low CPU idle rc" "1" "$RC"

run_headroom STUB_MEM_FREE=18
assert_contains "low memory free names mem-free" "mem-free=18%(min=25%)" "$LINE1"
assert_eq "low memory free rc" "1" "$RC"

run_headroom STUB_MEM_PRESSURE_LEVEL=2
assert_contains "warn pressure names mem-pressure" "mem-pressure=2(max=1)" "$LINE1"
assert_eq "warn pressure rc" "1" "$RC"

run_headroom STUB_SWAPOUTS_STEP=5000
assert_contains "rising swapouts names swapout-rate" "swapout-rate=" "$LINE1"
assert_contains "swapout reason carries the ceiling" "(max=10/s)" "$LINE1"
assert_eq "rising swapouts rc" "1" "$RC"

run_headroom STUB_DISK_AVAIL_KB=31457280   # 30 GB
assert_contains "low disk names disk-free" "disk-free=30.0GB(min=50GB)" "$LINE1"
assert_eq "low disk rc" "1" "$RC"

echo "== 4. every reason accumulates on one line =="
run_headroom STUB_CPU_IDLE=1.0 STUB_MEM_FREE=3 STUB_MEM_PRESSURE_LEVEL=4 \
  STUB_SWAPOUTS_STEP=90000 STUB_DISK_AVAIL_KB=1048576
for token in "cpu-idle=" "mem-free=" "mem-pressure=" "swapout-rate=" "disk-free="; do
  assert_contains "all-bad line carries $token" "$token" "$LINE1"
done
assert_eq "all-bad verdict is one line" "1" "$(printf '%s\n' "$OUT" | grep -c '^HEADROOM:')"
assert_eq "all-bad rc" "1" "$RC"

echo "== 5. unreadable probe fails CLOSED =="
run_headroom STUB_MEMORY_PRESSURE_BROKEN=1 STUB_SYSCTL_BROKEN=1 \
  STUB_VM_STAT_BROKEN=1 STUB_DF_BROKEN=1
assert_contains "unreadable memory free" "mem-free=unreadable" "$LINE1"
assert_contains "unreadable pressure level" "mem-pressure=unreadable" "$LINE1"
assert_contains "unreadable swapout rate" "swapout-rate=unreadable" "$LINE1"
assert_contains "unreadable disk" "disk-free=unreadable" "$LINE1"
assert_eq "unreadable probes → rc 1 (never OK)" "1" "$RC"

echo "== 6. #5438 regression: top's 'unused' must not drive the memory verdict =="
# top says 451M unused on a 48G box (≈0.9%); memory_pressure says 71% free.
# Reading the former as free memory is the 2026-08-09 / 2026-08-17 misread.
run_headroom STUB_TOP_UNUSED=451M STUB_MEM_FREE=71
assert_eq "tiny top 'unused' with healthy memory_pressure still OK" "HEADROOM: OK" "$LINE1"
assert_eq "tiny top 'unused' rc" "0" "$RC"
assert_not_contains "no PhysMem-derived detail line" "unused" "$OUT"

echo "== 7. builders are context, never a verdict input =="
run_headroom STUB_CARGO_N=8 STUB_RUSTC_N=7
assert_eq "8 cargo + 7 rustc on a healthy box is still OK" "HEADROOM: OK" "$LINE1"
assert_eq "builder-heavy rc" "0" "$RC"
assert_contains "builder counts reported in detail" "cargo=8 rustc=7" "$OUT"

echo "== 8. swapouts counter falls back to memory_pressure when vm_stat is gone =="
if env "PATH=$NO_VM_STAT_PATH" bash -c 'command -v vm_stat >/dev/null 2>&1'; then
  fail "sanitized PATH still resolves a vm_stat (case would test nothing)"
else
  pass "sanitized PATH has no vm_stat at all (absence, not failure)"
fi
HEADROOM_PATH="$NO_VM_STAT_PATH"
run_headroom STUB_MP_SWAPOUTS=1000
assert_eq "fallback swapouts counter yields a verdict" "HEADROOM: OK" "$LINE1"
assert_contains "fallback rate reported" "swapout-rate" "$OUT"
assert_eq "absent vm_stat is NOT an unreadable probe" "0" "$RC"
HEADROOM_PATH=""

echo "== 9. canonical thresholds hold at their boundaries =="
run_headroom STUB_CPU_IDLE=30.0
assert_eq "CPU idle exactly 30% passes" "HEADROOM: OK" "$LINE1"
run_headroom STUB_CPU_IDLE=29.9
assert_contains "CPU idle 29.9% fails" "cpu-idle=" "$LINE1"
run_headroom STUB_MEM_FREE=25
assert_eq "memory free exactly 25% passes" "HEADROOM: OK" "$LINE1"
run_headroom STUB_MEM_FREE=24
assert_contains "memory free 24% fails" "mem-free=" "$LINE1"
run_headroom STUB_MEM_PRESSURE_LEVEL=1
assert_eq "pressure level 1 passes" "HEADROOM: OK" "$LINE1"
run_headroom STUB_MEM_PRESSURE_LEVEL=2
assert_contains "pressure level 2 fails" "mem-pressure=" "$LINE1"
run_headroom STUB_DISK_AVAIL_KB=52428800   # 50.0 GB
assert_eq "disk exactly 50GB passes" "HEADROOM: OK" "$LINE1"
run_headroom STUB_DISK_AVAIL_KB=51380224   # 49.0 GB
assert_contains "disk 49GB fails" "disk-free=" "$LINE1"

echo "== 10. thresholds are overridable (and the defaults are the ones above) =="
run_headroom LANE_HEADROOM_MIN_CPU_IDLE_PCT=90 STUB_CPU_IDLE=85.0
assert_contains "raised CPU floor turns a passing box into NO" "cpu-idle=85.0%(min=90%)" "$LINE1"
run_headroom LANE_HEADROOM_MIN_DISK_FREE_GB=10 STUB_DISK_AVAIL_KB=31457280
assert_eq "lowered disk floor admits 30GB" "HEADROOM: OK" "$LINE1"

echo "== 11. real sampling interval is honoured =="
SWAP_SAMPLE_SECONDS=1 run_headroom
assert_contains "1s window reported in detail" "over 1s" "$OUT"
assert_eq "1s window verdict" "HEADROOM: OK" "$LINE1"
SWAP_SAMPLE_SECONDS=0 run_headroom
assert_contains "0s window labelled back-to-back" "back-to-back" "$OUT"

echo "== 12. usage handling =="
set +e
help_out="$(env "PATH=$STUB_DIR:$PATH" bash "$LANE_HEADROOM" --help 2>&1)"
help_rc=$?
bad_out="$(env "PATH=$STUB_DIR:$PATH" bash "$LANE_HEADROOM" --nope 2>&1)"
bad_rc=$?
set +e   # restore the suite's real mode: it runs without -e (line 23), and
         # turning -e ON here would abort the next section on its first NO rc.
assert_eq "--help rc 0" "0" "$help_rc"
assert_contains "--help documents the verdict contract" "HEADROOM: OK" "$help_out"
assert_eq "unknown argument rc 2 (distinct from a NO verdict)" "2" "$bad_rc"
assert_contains "unknown argument names itself" "unknown argument" "$bad_out"

echo "== 13. a probe that exits NONZERO is unreadable even when its stdout parses =="
# The reviewed #5438 defect: every probe read its command through a bare
# `$(cmd)`, so the rc was thrown away and well-formed stdout from a FAILING
# command was accepted as a live reading — all five cases below returned
# `HEADROOM: OK` rc 0 while the header promised fail-CLOSED. Each stub here
# prints its normal, fully parseable output and then exits 1.
assert_probe_rc_fails_closed() {
  local label="$1" reason="$2"
  shift 2
  run_headroom "$@"
  assert_contains "$label names its probe ($reason)" "$reason" "$LINE1"
  assert_contains "$label verdict is NO" "HEADROOM: NO " "$LINE1"
  assert_eq "$label rc is 1" "1" "$RC"
}
assert_probe_rc_fails_closed "top rc 1" "cpu-idle=unreadable" STUB_TOP_RC=1
assert_probe_rc_fails_closed "memory_pressure rc 1" "mem-free=unreadable" STUB_MEMORY_PRESSURE_RC=1
assert_probe_rc_fails_closed "sysctl rc 1" "mem-pressure=unreadable" STUB_SYSCTL_RC=1
assert_probe_rc_fails_closed "vm_stat rc 1" "swapout-rate=unreadable" STUB_VM_STAT_RC=1
assert_probe_rc_fails_closed "df rc 1" "disk-free=unreadable" STUB_DF_RC=1
# An installed-but-failing vm_stat must NOT quietly borrow memory_pressure's
# counter: the fallback is for an ABSENT vm_stat (section 8), and a failing probe
# on a machine whose VM stats are broken is exactly what fail-closed is for.
run_headroom STUB_VM_STAT_RC=1 STUB_MP_SWAPOUTS=1000
assert_contains "failing vm_stat does not fall back to memory_pressure" "swapout-rate=unreadable" "$LINE1"

echo
echo "PASS=$PASS FAIL=$FAIL"
if [ "$FAIL" -ne 0 ]; then
  printf 'failed: %s\n' "${FAIL_NAMES[@]}" >&2
  exit 1
fi
echo "OK: scripts/lane_headroom.sh contract holds (#5438)"
