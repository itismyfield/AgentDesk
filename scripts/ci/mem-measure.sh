#!/usr/bin/env bash
# Runs a command and reports its peak RSS (GNU time), the largest per-process peaks, memory and disk on stderr.
# Stdout and the exit status are the command's own, so pipelines keep their meaning.
set -uo pipefail

if [ "$#" -lt 3 ] || [ "$2" != "--" ]; then
  echo "usage: mem-measure.sh <label> -- <command> [args...]" >&2
  exit 2
fi
label="$1"
shift 2
time_bin="${MEM_MEASURE_TIME_BIN:-/usr/bin/time}"
proc_root="${MEM_MEASURE_PROC_ROOT:-/proc}"
tab="$(printf '\t')"

swapped_out_pages() {
  awk '$1 == "pswpout" { print $2 }' /proc/vmstat 2>/dev/null
}

# rustc rows name their crate, since max_rss alone cannot tell the lib test compile from a dependency.
rustc_label() {
  tr '\0' '\n' <"$proc_root/$1/cmdline" 2>/dev/null | awk '
    prev == "--crate-name" { crate = $0 }
    $0 == "--test" { kind = "+test" }
    { prev = $0 }
    END { print "rustc/" (crate == "" ? "?" : crate) kind }'
}

# Folds every live process's VmHWM into "$peaks" as tab-separated "pid kib name", keeping each pid's maximum.
# A bare or unresolved rustc name is replaced once a later sample reads that process's crate.
sample_peaks() {
  {
    cat "$peaks"
    # A process can exit between the glob and the read; that must not discard the whole sample.
    { cat "$proc_root"/[0-9]*/status 2>/dev/null || true; } | awk -v OFS='\t' '
      $1 == "Name:" { if (pid != "") print pid, hwm, name; name = $0; sub(/^Name:[ \t]*/, "", name); pid = ""; hwm = 0 }
      $1 == "Pid:" { pid = $2 }
      $1 == "VmHWM:" { hwm = $2 }
      END { if (pid != "") print pid, hwm, name }' |
      while IFS="$tab" read -r pid kib name; do
        if [ "$name" = rustc ] && [ "$kib" -ge 1048576 ]; then name="$(rustc_label "$pid")"; fi
        printf '%s\t%s\t%s\n' "$pid" "$kib" "$name"
      done
  } | awk -F '\t' -v OFS='\t' '
    function vague(n) { return n == "rustc" || n ~ /^rustc\/\?/ }
    !($1 in kib) || $2 + 0 > kib[$1] { kib[$1] = $2 + 0 }
    !($1 in name) || (vague(name[$1]) && !vague($3)) { name[$1] = $3 }
    END { for (p in kib) print p, kib[p], name[p] }' >"$peaks.next" && mv "$peaks.next" "$peaks"
}

# Stops the recorded process, then its descendants, so nothing it started outlives this script.
kill_tree() {
  local child
  kill -STOP "$1" 2>/dev/null
  for child in $(pgrep -P "$1" 2>/dev/null); do kill_tree "$child"; done
  kill -KILL "$1" 2>/dev/null
}

# An exited but not yet reaped child still answers kill -0, so a zombie counts as gone.
alive() {
  kill -0 "$1" 2>/dev/null || return 1
  case "$(ps -o stat= -p "$1" 2>/dev/null)" in Z*) return 1 ;; esac
}

# Asks the sampler to finish by signal (no file write needed); after 5 s it is killed and marked so.
stop_sampler() {
  local tries=0
  [ -n "$sampler" ] || return 0
  kill -TERM "$sampler" 2>/dev/null
  while alive "$sampler" && [ "$tries" -lt 50 ]; do
    sleep 0.1
    tries=$((tries + 1))
  done
  if alive "$sampler"; then
    kill_tree "$sampler"
    wait "$sampler" 2>/dev/null
    sampler_state="killed"
  elif wait "$sampler" 2>/dev/null; then
    sampler_state="ok"
  else
    sampler_state="incomplete"
  fi
  sampler=""
}

cleanup() {
  stop_sampler
  if [ -n "$stats" ]; then rm -f "$stats"; fi
  if [ -n "$peaks" ]; then rm -f "$peaks" "$peaks.next"; fi
}

stats="$(mktemp "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/mem-measure.XXXXXX" 2>/dev/null)" || stats=""
peaks="$(mktemp "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/mem-measure-peaks.XXXXXX" 2>/dev/null)" || peaks=""
sampler=""
sampler_state="off"
# A signal to this script is acted on once the command returns: same cleanup and summary, then the same signal.
caught=""
trap 'caught=TERM' TERM
trap 'caught=INT' INT
trap 'caught=HUP' HUP
trap cleanup EXIT
if [ -n "$peaks" ] && [ -d "$proc_root" ]; then
  # System-wide once a second; exits non-zero when a sample could not be recorded.
  (
    stopping="" nap="" failed=0
    trap 'stopping=1; kill "$nap" 2>/dev/null' TERM
    while [ -z "$stopping" ] && kill -0 "$$" 2>/dev/null; do
      sample_peaks || failed=1
      [ -z "$stopping" ] || break
      sleep 1 &
      nap=$!
      wait "$nap"
    done
    sample_peaks || failed=1
    exit "$failed"
  ) </dev/null >/dev/null 2>&1 &
  sampler=$!
fi
pswpout_before="$(swapped_out_pages)"
if [ -n "$stats" ] && "$time_bin" -v -o "$stats" true >/dev/null 2>&1; then
  "$time_bin" -v -o "$stats" -- "$@"
  rc=$?
else
  "$@"
  rc=$?
fi
pswpout_after="$(swapped_out_pages)"
stop_sampler
peak_procs="unavailable"
if [ "$sampler_state" != off ] && [ -n "$peaks" ]; then
  top="$(sort -t "$tab" -k2,2nr "$peaks" 2>/dev/null | awk -F '\t' 'NR <= 3 { printf "%s%s=%s", (NR > 1 ? "," : ""), $3, $2 }')"
  peak_procs="${top:-unavailable}"
fi

{
  max_rss=""
  elapsed=""
  if [ -n "$stats" ]; then
    max_rss="$(awk -F': ' '/Maximum resident set size/ { print $2 }' "$stats" 2>/dev/null)"
    elapsed="$(awk -F': ' '/Elapsed \(wall clock\) time/ { print $2 }' "$stats" 2>/dev/null)"
  fi
  swapped="unavailable"
  if [ -n "$pswpout_before" ] && [ -n "$pswpout_after" ]; then
    swapped=$((pswpout_after - pswpout_before))
  fi
  # peak_procs stays last: process names may contain spaces.
  echo "mem-measure ${label}: rc=${rc} max_rss_kib=${max_rss:-unavailable} elapsed=${elapsed:-unavailable} swapped_out_pages=${swapped} peak_sampler=${sampler_state} peak_procs=${peak_procs}"
  # Runs whatever the command's status, so a failed build still records memory and disk.
  bash "$(dirname "${BASH_SOURCE[0]}")/resource-snapshot.sh" "after-${label}" || true
} >&2
cleanup
trap - EXIT
if [ -n "$caught" ]; then
  trap - "$caught"
  kill -s "$caught" "$$"
fi
exit "$rc"
