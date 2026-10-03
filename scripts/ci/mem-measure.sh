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

# Folds every live process's VmHWM into "$peaks" as "pid kib name", keeping each pid's maximum.
sample_peaks() {
  {
    cat "$peaks"
    cat "$proc_root"/[0-9]*/status 2>/dev/null | awk '
      $1 == "Name:" { if (pid != "") print pid, hwm, name; name = $2; pid = ""; hwm = 0 }
      $1 == "Pid:" { pid = $2 }
      $1 == "VmHWM:" { hwm = $2 }
      END { if (pid != "") print pid, hwm, name }' |
      while read -r pid kib name; do
        if [ "$name" = rustc ] && [ "$kib" -ge 1048576 ]; then name="$(rustc_label "$pid")"; fi
        echo "$pid $kib $name"
      done
  } | awk '
    !($1 in kib) || $2 + 0 > kib[$1] { kib[$1] = $2 + 0 }
    !($1 in name) || name[$1] == "rustc" { name[$1] = $3 }
    END { for (p in kib) print p, kib[p], name[p] }' >"$peaks.next" && mv "$peaks.next" "$peaks"
}

stats="$(mktemp "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/mem-measure.XXXXXX" 2>/dev/null)" || stats=""
peaks="$(mktemp "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/mem-measure-peaks.XXXXXX" 2>/dev/null)" || peaks=""
sampler=""
if [ -n "$peaks" ] && [ -d "$proc_root" ]; then
  # System-wide once a second; it stops on the stop file or when this script is gone.
  (
    while [ ! -e "$peaks.stop" ] && kill -0 "$$" 2>/dev/null; do
      sample_peaks
      sleep 1
    done
    sample_peaks
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
peak_procs="unavailable"
if [ -n "$sampler" ]; then
  touch "$peaks.stop"
  wait "$sampler" 2>/dev/null
  top="$(sort -k2,2nr "$peaks" | awk 'NR <= 3 { printf "%s%s=%s", (NR > 1 ? "," : ""), $3, $2 }')"
  peak_procs="${top:-unavailable}"
fi
[ -n "$peaks" ] && rm -f "$peaks" "$peaks.next" "$peaks.stop"

{
  max_rss=""
  elapsed=""
  if [ -n "$stats" ]; then
    max_rss="$(awk -F': ' '/Maximum resident set size/ { print $2 }' "$stats")"
    elapsed="$(awk -F': ' '/Elapsed \(wall clock\) time/ { print $2 }' "$stats")"
    rm -f "$stats"
  fi
  swapped="unavailable"
  if [ -n "$pswpout_before" ] && [ -n "$pswpout_after" ]; then
    swapped=$((pswpout_after - pswpout_before))
  fi
  echo "mem-measure ${label}: rc=${rc} max_rss_kib=${max_rss:-unavailable} elapsed=${elapsed:-unavailable} swapped_out_pages=${swapped} peak_procs=${peak_procs}"
  # Runs whatever the command's status, so a failed build still records memory and disk.
  bash "$(dirname "${BASH_SOURCE[0]}")/resource-snapshot.sh" "after-${label}" || true
} >&2
exit "$rc"
