#!/usr/bin/env bash
# Compare the head nextest plan with the comparison base: CI Main's nextest-baseline artifact for
# that exact SHA, or a base plan built here when the artifact is missing or not comparable. When that
# build fails with compiler errors, the nearest first-parent ancestor's CI Main baseline stands in.
# Usage: nextest-base-diff.sh <nextest dir holding plan/plan.json>
#   (env: PR_HEAD_SHA, GH_TOKEN; GITHUB_SHA and GITHUB_RUN_ID name the run that must have written the plan)
set -euo pipefail

NEXTEST_DIR="$1"
plan="$NEXTEST_DIR/plan/plan.json"
work="$NEXTEST_DIR/base"
summary="${GITHUB_STEP_SUMMARY:-/dev/null}"
ancestor_limit=30
mkdir -p "$work"
ci() { python3 scripts/ci/nextest_ci.py "$@"; }
phase=comparison-base
trap 'rc=$?; [ "$rc" -eq 0 ] || echo "nextest base comparison stopped in $phase (exit $rc)" >>"$summary"' EXIT

ci comparison-base --pr-head-sha "$PR_HEAD_SHA" --github-output "$work/base.env"
base_sha="$(sed -n 's/^base_sha=//p' "$work/base.env")"
[ -n "$base_sha" ] || { echo "ERROR no comparison base SHA" >&2; exit 1; }

# Linux removals come from the plans; macOS and Windows only from the base and head sources.
phase=sources
git worktree add --detach "$work/src" "$base_sha" >/dev/null
python3 scripts/ci/rust_test_sources.py removed --base-root "$work/src" --json-out "$work/source-removed.json"
missing="$(python3 -c 'import json,sys; print("; ".join(json.load(open(sys.argv[1])).get("base_missing_files", [])))' "$work/source-removed.json")"
[ -z "$missing" ] || echo "nextest base sources at $base_sha lack module files ($missing); its tests are read from the files it has" >>"$summary"
diff_args=(--plan "$plan" --head-sha "$GITHUB_SHA" --head-run-id "$GITHUB_RUN_ID" --comparison-base-sha "$base_sha"
  --justifications scripts/ci/test-removal-justifications.txt --source-removed "$work/source-removed.json"
  --json-out "$work/diff.json")

# CI Main uploads the baseline as soon as its build job has planned, so a run that is still testing, or
# failed or was cancelled later, carries the same IDs; base-diff still checks the SHA, scope and build.
fetch_main_baseline() {
  local sha="$1" runs run_id state
  runs="$(gh run list --repo "$GITHUB_REPOSITORY" --workflow ci-main.yml --commit "$sha" --limit 20 \
    --json databaseId,event,status,conclusion \
    --jq '.[] | select(.event == "push" or .event == "workflow_dispatch") | "\(.databaseId) \(.status)/\(.conclusion)"')" \
    || return 1
  while read -r run_id state; do
    [ -n "$run_id" ] || continue
    # gh refuses to overwrite files, so an earlier candidate's partial download must not remain.
    rm -rf "$work/main"
    if gh run download "$run_id" --repo "$GITHUB_REPOSITORY" --name nextest-baseline --dir "$work/main"; then
      echo "CI Main nextest-baseline for $sha from run $run_id ($state)" | tee -a "$summary"
      return 0
    fi
  done <<<"$runs"
  return 1
}

phase=main-baseline
reason="no CI Main run of $base_sha has uploaded its nextest-baseline artifact"
if fetch_main_baseline "$base_sha"; then
  phase=diff
  status=0
  ci base-diff --baseline "$work/main/plan.json" "${diff_args[@]}" || status=$?
  if [ "$status" -ne 2 ]; then
    echo "nextest base diff against CI Main's baseline at $base_sha: exit $status" >>"$summary"
    exit "$status"
  fi
  reason="CI Main baseline for $base_sha differs in SHA, scope or build identity"
fi

# Fallback: build the base plan with the head plan's scope and build flags. Only a compiler error in that
# build turns to an ancestor's baseline; any other failure fails the step.
phase=base-build
echo "::warning title=nextest baseline fallback::$reason; building the base plan at $base_sha"
echo "nextest baseline fallback: $reason; the base plan was built in this job" >>"$summary"
printf 'version = 1\n' >"$work/no-retries.toml"
set +e
(cd "$work/src" && CARGO_TARGET_DIR="$GITHUB_WORKSPACE/target" cargo nextest archive --lib \
  --archive-file "$work/base-tests.tar.zst") 2>&1 | tee "$work/base-build.log"
codes=("${PIPESTATUS[@]}")
set -e
[ "${codes[1]}" -eq 0 ] || { echo "ERROR cannot record the base build log" >&2; exit 1; }
build_status="${codes[0]}"

# The ancestor fallback takes only builds that stopped on compiler errors; it excludes linker failures and
# signal deaths, whatever their cause.
compile_failure() {
  [ "$build_status" -eq 101 ] \
    && grep -Eq 'could not compile .* due to [0-9]+ previous error' "$work/base-build.log" \
    && ! grep -Eq 'linking with .* failed|signal: [0-9]+' "$work/base-build.log"
}

if [ "$build_status" -ne 0 ]; then
  if ! compile_failure; then
    echo "nextest base build failed at $base_sha (exit $build_status); the ancestor fallback takes only compiler errors without a linker or signal trace" >>"$summary"
    exit "$build_status"
  fi
  echo "::warning title=nextest base build failed::$base_sha does not compile (cargo nextest archive exit 101); a base failure, not the head's"
  echo "nextest base build failed at $base_sha: compiler errors (cargo nextest archive exit 101)" >>"$summary"
  phase=ancestor-baseline
  if [ "$(git rev-parse --is-shallow-repository)" = true ]; then
    git fetch --no-tags --quiet --depth=$((ancestor_limit + 1)) origin "$base_sha" \
      || { echo "nextest ancestor lookup failed: cannot fetch the first-parent line of $base_sha" >>"$summary"; exit 1; }
  fi
  ancestors="$(git rev-list --first-parent --skip=1 --max-count="$ancestor_limit" "$base_sha")" \
    || { echo "nextest ancestor lookup failed: cannot list the first-parent line of $base_sha" >>"$summary"; exit 1; }
  distance=0
  for sha in $ancestors; do
    distance=$((distance + 1))
    fetch_main_baseline "$sha" || continue
    echo "::warning title=nextest ancestor baseline::comparing with $sha, $distance first-parent commits below $base_sha"
    echo "nextest comparison baseline: ancestor $sha ($distance first-parent commits below $base_sha); tests $base_sha has and the head lacks are also checked from the sources" >>"$summary"
    phase=diff
    status=0
    ci base-diff --baseline "$work/main/plan.json" --baseline-sha "$sha" "${diff_args[@]}" || status=$?
    echo "nextest base diff against the ancestor baseline at $sha for base $base_sha: exit $status" >>"$summary"
    # 2: not comparable; the walk stops here by policy rather than trying an older baseline.
    [ "$status" -ne 2 ] || { echo "nextest base build failed at $base_sha and no comparable baseline was found within $ancestor_limit first-parent ancestors" >>"$summary"; exit 1; }
    exit "$status"
  done
  echo "nextest base build failed at $base_sha and no comparable baseline was found within $ancestor_limit first-parent ancestors" >>"$summary"
  exit "$build_status"
fi
(
  cd "$work/src"
  cargo nextest list --archive-file "$work/base-tests.tar.zst" --workspace-remap "$work/src" \
    --ignore-default-filter --message-format json >"$work/base-universe.json"
)
ci plan --universe "$work/base-universe.json" --archive "$work/base-tests.tar.zst" --workers 1 --sha "$base_sha" \
  --identity-from "$plan" --retry-policy "$work/no-retries.toml" --output "$work/built"
phase=diff
status=0
ci base-diff --baseline "$work/built/plan.json" "${diff_args[@]}" || status=$?
echo "nextest base diff against the fallback base plan at $base_sha: exit $status" >>"$summary"
exit "$status"
