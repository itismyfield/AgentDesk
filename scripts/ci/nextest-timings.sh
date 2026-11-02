#!/usr/bin/env bash
# Print `--timings <junit.xml>` plan arguments from the newest CI Main run whose every nextest worker kept
# its JUnit report. On any gap it prints nothing and warns, so the plan splits by test count; the step never fails.
# Usage: nextest-timings.sh <work dir, emptied first> <workers>   (env: GITHUB_REPOSITORY, GH_TOKEN)
set -uo pipefail
shopt -s nullglob

work="$1"
workers="$2"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
summary="${GITHUB_STEP_SUMMARY:-/dev/null}"
max_age_days=7 # CI Main keeps the worker artifacts for 7 days

fallback() {
  echo "::warning title=nextest timing fallback::$1; the plan splits by test count" >&2
  echo "nextest timing fallback: $1; the plan splits by test count" >>"$summary"
  exit 0
}

rm -rf "$work"
mkdir -p "$work" || fallback "cannot create $work"
# Only main's own push and dispatch runs: a pull_request run of a changed ci-main.yml must not steer the plan.
runs="$(timeout 60 gh run list --repo "$GITHUB_REPOSITORY" --workflow ci-main.yml --branch main --limit 20 \
  --json databaseId,event,createdAt --jq ".[] | select(.event == \"push\" or .event == \"workflow_dispatch\")
    | select((.createdAt | fromdateiso8601) > now - $max_age_days * 86400) | .databaseId")" ||
  fallback "cannot list CI Main runs"
run=""
for candidate in $runs; do
  names="$(timeout 60 gh api "repos/$GITHUB_REPOSITORY/actions/runs/$candidate/artifacts?per_page=100" \
    --jq '.artifacts[] | select(.expired | not) | .name')" ||
    fallback "cannot list the artifacts of CI Main run $candidate"
  for ((i = 0; i < workers; i++)); do
    grep -Eqx "nextest-worker-$i-attempt-[0-9]+" <<<"$names" || continue 2
  done
  run="$candidate"
  break
done
[ -n "$run" ] || fallback "no CI Main push or dispatch run of the last $max_age_days days kept all $workers worker artifacts"
timeout 300 gh run download "$run" --repo "$GITHUB_REPOSITORY" --pattern 'nextest-worker-*' --dir "$work" >&2 ||
  fallback "cannot download the worker artifacts of CI Main run $run"

# Every attempt of a worker is passed on; the plan keeps each test's slowest observation.
reports=()
for ((i = 0; i < workers; i++)); do
  found=("$work/nextest-worker-$i-attempt-"*/junit.xml)
  [ "${#found[@]}" -gt 0 ] || fallback "CI Main run $run has no JUnit report for worker $i"
  reports+=("${found[@]}")
done
# The plan rejects an unreadable report, so each one is read here with the plan's own parser first.
bad="$(python3 -I - "$here" "${reports[@]}" 2>&1 <<'PY'
import sys
sys.path.insert(0, sys.argv[1])
import nextest_ci
for path in sys.argv[2:]:
    try:
        timings, _ = nextest_ci.load_timings([path])
    except nextest_ci.PlanError as exc:
        sys.exit(str(exc))
    if not timings:
        sys.exit(f"JUnit {path} has no timed testcase")
PY
)" || fallback "unusable JUnit from CI Main run $run: ${bad//$'\n'/ }"

echo "nextest timings: ${#reports[@]} JUnit reports from CI Main run $run" | tee -a "$summary" >&2
for report in "${reports[@]}"; do
  printf -- '--timings\n%s\n' "$report"
done
