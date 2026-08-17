#!/usr/bin/env bash
# scripts/lane_headroom.sh — the single EXECUTABLE authority for lane-admission
# resource judgment (#5438).
#
# CONTRACT (orchestrators and tests/test_lane_headroom_5438.sh depend on it):
#   • stdout line 1 is EXACTLY `HEADROOM: OK` or `HEADROOM: NO <reason>[ <reason>...]`
#     (reason tokens never contain a space, so line 1 stays parseable by field).
#   • rc 0 = OK (a new lane may start), rc 1 = NO (hold), rc 2 = usage error.
#   • stdout line 2+ is per-probe human detail. Machine callers read line 1 / rc only.
#
# WHY A SCRIPT AND NOT PROSE
# The same thresholds used to live as prose in two places (the shared prompt's
# ★ section and the issue-pipeline SKILL). Prose does not reach a running agent:
# a long-lived session freezes its system prompt at boot time, and skill bodies
# are truncated by compaction. The rule was correct and the DELIVERY failed —
# the same misread (see below) was measured twice. Numbers and measurement
# method now live here, in something an orchestrator can execute at the moment
# of the decision, regardless of session age. Do not restate these numbers in a
# prompt, skill, or doc; reference this path.
#
# ROLLOUT ORDER
# The vault prose substitution (replacing the canonical _shared.prompt.md ★
# section and the SKILL.md numbers section with a reference to this script's rc)
# is a coordinator step AFTER this script lands on main — substituting before the
# merge leaves a dangling pointer.
#
# WHY `top`'s "unused" IS BANNED AS THE MEMORY SIGNAL
# `top -l 1` prints e.g. `PhysMem: 47G used (9314M wired, 4330M compressor),
# 451M unused.` Read as "free memory", that 451M says a 48G machine has 0.9%
# headroom — while `memory_pressure`, on the same box in the same second,
# reports `System-wide memory free percentage: 71%`. top's "unused" counts only
# the free list; it excludes the file cache, purgeable, and inactive pages that
# macOS reclaims on demand, so it structurally UNDERSTATES available memory by
# an order of magnitude. That misread was measured on 2026-08-09 and again on
# 2026-08-17, and both times it withheld lanes from a machine with ample room.
# This script therefore takes free% from `memory_pressure` and the pressure
# level from `kern.memorystatus_vm_pressure_level`, and never parses top's
# PhysMem line. Do not "improve" this by adding a top-based memory check.
#
# Probe notes:
#   • CPU idle comes from a single `top -l 1 -n 0` sample. Cross-checked on
#     2026-08-17 against a `top -l 2 -s 1` delta and `vm.loadavg` on a 14-core
#     host: the single sample tracks the recent window (repeated reads moved
#     several points within seconds), it is not a since-boot constant.
#   • Swap is judged on the swapouts RATE, never the absolute counter or
#     `vm.swapusage`. Both of those are lifetime/high-water values: a box that
#     swapped hard days ago still reports a huge total while sitting idle now.
#     Two samples of the `vm_stat` / `memory_pressure` swapouts counter,
#     LANE_HEADROOM_SWAP_SAMPLE_SECONDS apart, answer the actual question —
#     is the machine swapping RIGHT NOW.
#   • `pgrep -x cargo|rustc` is reported as context, NOT as a gate. Builders are
#     the workload lanes exist to run; their CPU/RAM cost is already visible in
#     the four gated metrics, and gating on their presence would refuse every
#     lane on a working machine. `pgrep -x` (never `-f`) so this script and its
#     wrapper can not self-match.
#
# Unreadable probe = NO (fail CLOSED). This is deliberately the opposite of the
# #4255 deploy pre-flight, which fails OPEN: refusing to ADD a lane costs a wait,
# while the deploy gate blocking on an unreadable metric would stall shipping.
# "Unreadable" includes a probe command that EXITED NONZERO even when its stdout
# still looks well-formed: every metric is read through probe_run(), which
# discards the output of a failed command so the empty reading lands in the same
# `is_num` gate as unparseable output (one fail-closed path, no per-probe rc
# branch). The only rc that is not a failure is `pgrep`'s 1 = "no match", and
# builders are context-only anyway.

set -uo pipefail

SCRIPT_NAME="$(basename "${BASH_SOURCE[0]}")"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
  cat <<EOF
Usage: ${SCRIPT_NAME}

Measures lane-admission headroom and prints one verdict line:
  HEADROOM: OK                  (rc 0) — a new lane may start
  HEADROOM: NO <reasons>        (rc 1) — hold; each reason is metric=value(limit)
Lines 2+ are the measured values, for human review.

Thresholds (env overrides, defaults are the canonical values):
  LANE_HEADROOM_MIN_CPU_IDLE_PCT        (default 30)   CPU idle %, >= to pass
  LANE_HEADROOM_MIN_MEM_FREE_PCT        (default 25)   memory_pressure free %, >= to pass
  LANE_HEADROOM_MAX_MEM_PRESSURE_LEVEL  (default 1)    1=normal 2=warn 4=critical, <= to pass
  LANE_HEADROOM_MAX_SWAPOUT_RATE        (default 10)   swapouts pages/s, <= to pass (near zero)
  LANE_HEADROOM_MIN_DISK_FREE_GB        (default 50)   free GB, >= to pass
  LANE_HEADROOM_SWAP_SAMPLE_SECONDS     (default 10)   swapout sampling interval; 0 = back-to-back
  LANE_HEADROOM_DISK_PATH               (default repo root) filesystem to measure
EOF
}

case "${1:-}" in
  -h|--help) usage; exit 0 ;;
  "") ;;
  *) usage >&2; echo "${SCRIPT_NAME}: unknown argument: $1" >&2; exit 2 ;;
esac

MIN_CPU_IDLE_PCT="${LANE_HEADROOM_MIN_CPU_IDLE_PCT:-30}"
MIN_MEM_FREE_PCT="${LANE_HEADROOM_MIN_MEM_FREE_PCT:-25}"
MAX_MEM_PRESSURE_LEVEL="${LANE_HEADROOM_MAX_MEM_PRESSURE_LEVEL:-1}"
MAX_SWAPOUT_RATE="${LANE_HEADROOM_MAX_SWAPOUT_RATE:-10}"
MIN_DISK_FREE_GB="${LANE_HEADROOM_MIN_DISK_FREE_GB:-50}"
SWAP_SAMPLE_SECONDS="${LANE_HEADROOM_SWAP_SAMPLE_SECONDS:-10}"
DISK_PATH="${LANE_HEADROOM_DISK_PATH:-$REPO_ROOT}"

case "$SWAP_SAMPLE_SECONDS" in ''|*[!0-9]*) SWAP_SAMPLE_SECONDS=10 ;; esac

is_num() {
  # True only for a bare non-empty decimal number (no sign, no exponent).
  case "${1:-}" in
    ''|*[!0-9.]*) return 1 ;;
  esac
  return 0
}

num_lt() { awk -v a="$1" -v b="$2" 'BEGIN { exit !((a + 0) < (b + 0)) }'; }
num_gt() { awk -v a="$1" -v b="$2" 'BEGIN { exit !((a + 0) > (b + 0)) }'; }

probe_run() {
  # THE fail-closed convergence point. Runs a probe command, captures its rc,
  # and on a NONZERO rc discards the stdout and returns 1 — so a command that
  # printed a perfectly well-formed reading but failed hands the caller the same
  # empty value an unparseable reading gives. Both then meet the one gate below
  # (`is_num` fails → `<metric>=unreadable` reason → rc 1). Probes must read
  # their command through this and never through a bare `$(cmd)`, which throws
  # the rc away.
  local out rc
  out="$("$@" 2>/dev/null)"
  rc=$?
  [ "$rc" -eq 0 ] || return 1
  printf '%s\n' "$out"
}

probe_cpu_idle_pct() {
  # `CPU usage: 41.7% user, 16.42% sys, 42.49% idle` → 42.49
  command -v top >/dev/null 2>&1 || return 0
  probe_run top -l 1 -n 0 | awk '
    /CPU usage/ {
      for (i = 2; i <= NF; i++) {
        if ($i ~ /^idle/) { v = $(i - 1); sub(/%$/, "", v); last = v }
      }
    }
    END { if (last != "") print last }
  '
}

MEMORY_PRESSURE_OUT=""
capture_memory_pressure() {
  command -v memory_pressure >/dev/null 2>&1 || return 0
  MEMORY_PRESSURE_OUT="$(probe_run memory_pressure)"
}

probe_mem_free_pct() {
  # `System-wide memory free percentage: 71%` → 71. NEVER top's "unused".
  [ -n "$MEMORY_PRESSURE_OUT" ] || return 0
  printf '%s\n' "$MEMORY_PRESSURE_OUT" | awk -F: '
    /System-wide memory free percentage/ { v = $2; gsub(/[^0-9.]/, "", v); if (v != "") last = v }
    END { if (last != "") print last }
  '
}

probe_mem_pressure_level() {
  # 1 = normal, 2 = warn, 4 = critical.
  command -v sysctl >/dev/null 2>&1 || return 0
  local lvl
  lvl="$(probe_run sysctl -n kern.memorystatus_vm_pressure_level)"
  case "$lvl" in
    ''|*[!0-9]*) return 0 ;;
  esac
  printf '%s' "$lvl"
}

probe_swapouts_counter() {
  # Cumulative swapouts page counter. vm_stat first (instant); memory_pressure
  # is the fallback for a host that does not ship vm_stat at all. Re-invoked per
  # sample — never reuse a cached reading, or the delta is structurally 0.
  # A source that is INSTALLED and FAILS is not a cue to fall back: it is an
  # unreadable probe, so it returns empty into the one fail-closed gate. Only an
  # ABSENT source (or one that ran fine yet carries no Swapouts line) advances to
  # the next.
  local src out v=""
  for src in vm_stat memory_pressure; do
    command -v "$src" >/dev/null 2>&1 || continue
    out="$(probe_run "$src")" || return 0
    v="$(printf '%s\n' "$out" | awk -F: '/^Swapouts/ { gsub(/[^0-9]/, "", $2); if ($2 != "") { print $2; exit } }')"
    [ -n "$v" ] && break
  done
  printf '%s' "$v"
}

probe_disk_free_gb() {
  # -P forces one line per filesystem (a long device name otherwise wraps).
  local path="$1"
  command -v df >/dev/null 2>&1 || return 0
  probe_run df -Pk "$path" | awk 'NR == 2 && $4 ~ /^[0-9]+$/ { printf "%.1f", $4 / 1048576 }'
}

probe_builder_count() {
  # EXACT-name match only: `pgrep -f cargo` would match this script's own
  # invocation path and report a builder that does not exist.
  # Deliberately NOT read through probe_run: pgrep exits 1 to mean "no match",
  # which is the common healthy case, and this number is context — it can never
  # produce a reason, so it has nothing to fail closed on.
  local name="$1"
  command -v pgrep >/dev/null 2>&1 || { printf 'n/a'; return 0; }
  pgrep -x "$name" 2>/dev/null | awk 'END { print NR + 0 }'
}

reasons=()
details=()

capture_memory_pressure

# (1) CPU idle
cpu_idle="$(probe_cpu_idle_pct)"
if ! is_num "$cpu_idle"; then
  reasons+=("cpu-idle=unreadable")
  details+=("$(printf '  %-13s %s' 'cpu-idle' "unreadable (top -l 1 -n 0 failed or gave no 'CPU usage' line) — need >= ${MIN_CPU_IDLE_PCT}%")")
elif num_lt "$cpu_idle" "$MIN_CPU_IDLE_PCT"; then
  reasons+=("cpu-idle=${cpu_idle}%(min=${MIN_CPU_IDLE_PCT}%)")
  details+=("$(printf '  %-13s %s' 'cpu-idle' "${cpu_idle}% — BELOW the ${MIN_CPU_IDLE_PCT}% floor [top -l 1 -n 0]")")
else
  details+=("$(printf '  %-13s %s' 'cpu-idle' "${cpu_idle}% (need >= ${MIN_CPU_IDLE_PCT}%) [top -l 1 -n 0]")")
fi

# (2) Memory free % — memory_pressure only, never top's "unused"
mem_free="$(probe_mem_free_pct)"
if ! is_num "$mem_free"; then
  reasons+=("mem-free=unreadable")
  details+=("$(printf '  %-13s %s' 'mem-free' "unreadable (memory_pressure failed or gave no 'System-wide memory free percentage') — need >= ${MIN_MEM_FREE_PCT}%")")
elif num_lt "$mem_free" "$MIN_MEM_FREE_PCT"; then
  reasons+=("mem-free=${mem_free}%(min=${MIN_MEM_FREE_PCT}%)")
  details+=("$(printf '  %-13s %s' 'mem-free' "${mem_free}% — BELOW the ${MIN_MEM_FREE_PCT}% floor [memory_pressure system-wide free %]")")
else
  details+=("$(printf '  %-13s %s' 'mem-free' "${mem_free}% (need >= ${MIN_MEM_FREE_PCT}%) [memory_pressure system-wide free %]")")
fi

# (3) Memory pressure level
mem_level="$(probe_mem_pressure_level)"
if ! is_num "$mem_level"; then
  reasons+=("mem-pressure=unreadable")
  details+=("$(printf '  %-13s %s' 'mem-pressure' "unreadable (sysctl kern.memorystatus_vm_pressure_level failed or was non-numeric) — need <= ${MAX_MEM_PRESSURE_LEVEL}")")
elif num_gt "$mem_level" "$MAX_MEM_PRESSURE_LEVEL"; then
  reasons+=("mem-pressure=${mem_level}(max=${MAX_MEM_PRESSURE_LEVEL})")
  details+=("$(printf '  %-13s %s' 'mem-pressure' "${mem_level} — ABOVE the level-${MAX_MEM_PRESSURE_LEVEL} ceiling (1=normal 2=warn 4=critical)")")
else
  details+=("$(printf '  %-13s %s' 'mem-pressure' "${mem_level} (need <= ${MAX_MEM_PRESSURE_LEVEL}; 1=normal 2=warn 4=critical)")")
fi

# (4) Swapout RATE over LANE_HEADROOM_SWAP_SAMPLE_SECONDS (not the lifetime counter)
swap_before="$(probe_swapouts_counter)"
swap_t0="$(date +%s)"
if [ "$SWAP_SAMPLE_SECONDS" -gt 0 ]; then
  sleep "$SWAP_SAMPLE_SECONDS"
fi
swap_after="$(probe_swapouts_counter)"
swap_t1="$(date +%s)"
swap_elapsed=$((swap_t1 - swap_t0))
[ "$swap_elapsed" -lt 1 ] && swap_elapsed=1
swap_window="${swap_elapsed}s"
[ "$SWAP_SAMPLE_SECONDS" -eq 0 ] && swap_window="back-to-back"

if ! is_num "$swap_before" || ! is_num "$swap_after"; then
  reasons+=("swapout-rate=unreadable")
  details+=("$(printf '  %-13s %s' 'swapout-rate' "unreadable (vm_stat/memory_pressure failed or gave no swapouts counter) — need <= ${MAX_SWAPOUT_RATE} pages/s")")
else
  # A counter that went backwards means a reset (reboot), not negative swapping.
  swap_rate="$(awk -v a="$swap_before" -v b="$swap_after" -v e="$swap_elapsed" \
    'BEGIN { d = (b + 0) - (a + 0); if (d < 0) d = 0; printf "%.1f", d / e }')"
  if num_gt "$swap_rate" "$MAX_SWAPOUT_RATE"; then
    reasons+=("swapout-rate=${swap_rate}/s(max=${MAX_SWAPOUT_RATE}/s)")
    details+=("$(printf '  %-13s %s' 'swapout-rate' "${swap_rate} pages/s over ${swap_window} — ABOVE the ${MAX_SWAPOUT_RATE} pages/s near-zero ceiling (counter ${swap_before} -> ${swap_after})")")
  else
    details+=("$(printf '  %-13s %s' 'swapout-rate' "${swap_rate} pages/s over ${swap_window} (need <= ${MAX_SWAPOUT_RATE}) [counter ${swap_before} -> ${swap_after}]")")
  fi
fi

# (5) Disk free
disk_free="$(probe_disk_free_gb "$DISK_PATH")"
if ! is_num "$disk_free"; then
  reasons+=("disk-free=unreadable")
  details+=("$(printf '  %-13s %s' 'disk-free' "unreadable (df -Pk ${DISK_PATH} failed or gave no Available field) — need >= ${MIN_DISK_FREE_GB}GB")")
elif num_lt "$disk_free" "$MIN_DISK_FREE_GB"; then
  reasons+=("disk-free=${disk_free}GB(min=${MIN_DISK_FREE_GB}GB)")
  details+=("$(printf '  %-13s %s' 'disk-free' "${disk_free}GB on ${DISK_PATH} — BELOW the ${MIN_DISK_FREE_GB}GB floor")")
else
  details+=("$(printf '  %-13s %s' 'disk-free' "${disk_free}GB on ${DISK_PATH} (need >= ${MIN_DISK_FREE_GB}GB)")")
fi

# (6) Builders — CONTEXT ONLY, never a reason (see header).
cargo_n="$(probe_builder_count cargo)"
rustc_n="$(probe_builder_count rustc)"
details+=("$(printf '  %-13s %s' 'builders' "cargo=${cargo_n} rustc=${rustc_n} (context only — never a verdict input) [pgrep -x]")")

if [ "${#reasons[@]}" -eq 0 ]; then
  echo "HEADROOM: OK"
else
  echo "HEADROOM: NO ${reasons[*]}"
fi
printf '%s\n' "${details[@]}"

[ "${#reasons[@]}" -eq 0 ] && exit 0
exit 1
