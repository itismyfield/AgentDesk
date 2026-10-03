#!/usr/bin/env bash
# Runs a command and reports its peak RSS (GNU time) and system memory on stderr.
# Stdout and the exit status are the command's own, so pipelines keep their meaning.
set -uo pipefail

if [ "$#" -lt 3 ] || [ "$2" != "--" ]; then
  echo "usage: mem-measure.sh <label> -- <command> [args...]" >&2
  exit 2
fi
label="$1"
shift 2
time_bin="${MEM_MEASURE_TIME_BIN:-/usr/bin/time}"

swapped_out_pages() {
  awk '$1 == "pswpout" { print $2 }' /proc/vmstat 2>/dev/null
}

stats="$(mktemp "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/mem-measure.XXXXXX" 2>/dev/null)" || stats=""
pswpout_before="$(swapped_out_pages)"
if [ -n "$stats" ] && "$time_bin" -v -o "$stats" true >/dev/null 2>&1; then
  "$time_bin" -v -o "$stats" -- "$@"
  rc=$?
else
  "$@"
  rc=$?
fi
pswpout_after="$(swapped_out_pages)"

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
  echo "mem-measure ${label}: rc=${rc} max_rss_kib=${max_rss:-unavailable} elapsed=${elapsed:-unavailable} swapped_out_pages=${swapped}"
  free -m || true
} >&2
exit "$rc"
