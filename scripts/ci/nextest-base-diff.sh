#!/usr/bin/env bash
# Compare the head nextest plan with the comparison base: CI Main's nextest-baseline artifact for
# that exact SHA, or a base plan built here when the artifact is missing or not comparable.
# Usage: nextest-base-diff.sh <nextest dir holding plan/plan.json>   (env: PR_HEAD_SHA, GH_TOKEN)
set -euo pipefail

NEXTEST_DIR="$1"
plan="$NEXTEST_DIR/plan/plan.json"
work="$NEXTEST_DIR/base"
summary="${GITHUB_STEP_SUMMARY:-/dev/null}"
mkdir -p "$work"
ci() { python3 scripts/ci/nextest_ci.py "$@"; }

ci comparison-base --pr-head-sha "$PR_HEAD_SHA" --github-output "$work/base.env"
base_sha="$(sed -n 's/^base_sha=//p' "$work/base.env")"
[ -n "$base_sha" ] || { echo "ERROR no comparison base SHA" >&2; exit 1; }

# Linux removals come from the plans; macOS and Windows only from the base and head sources.
git worktree add --detach "$work/src" "$base_sha" >/dev/null
python3 scripts/ci/rust_test_sources.py removed --base-root "$work/src" --json-out "$work/source-removed.json"
diff_args=(--plan "$plan" --comparison-base-sha "$base_sha" --justifications scripts/ci/test-removal-justifications.txt
  --source-removed "$work/source-removed.json" --json-out "$work/diff.json")

fetch_main_baseline() {
  local run_id
  run_id="$(gh run list --repo "$GITHUB_REPOSITORY" --workflow ci-main.yml --commit "$base_sha" --status success \
    --limit 1 --json databaseId --jq '.[0].databaseId // empty')" || return 1
  [ -n "$run_id" ] || return 1
  gh run download "$run_id" --repo "$GITHUB_REPOSITORY" --name nextest-baseline --dir "$work/main" || return 1
}

reason="no successful CI Main nextest-baseline artifact for $base_sha"
if fetch_main_baseline; then
  status=0
  ci base-diff --baseline "$work/main/plan.json" "${diff_args[@]}" || status=$?
  if [ "$status" -ne 2 ]; then
    echo "nextest base diff against CI Main's baseline at $base_sha: exit $status" >>"$summary"
    exit "$status"
  fi
  reason="CI Main baseline for $base_sha differs in SHA, scope or build identity"
fi

# Fallback: build the base plan with the head plan's scope and build flags. Any failure here fails the step.
echo "::warning title=nextest baseline fallback::$reason; building the base plan at $base_sha"
echo "nextest baseline fallback: $reason; the base plan was built in this job" >>"$summary"
printf 'version = 1\n' >"$work/no-retries.toml"
(
  cd "$work/src"
  CARGO_TARGET_DIR="$GITHUB_WORKSPACE/target" cargo nextest archive --lib --archive-file "$work/base-tests.tar.zst"
  cargo nextest list --archive-file "$work/base-tests.tar.zst" --workspace-remap "$work/src" \
    --ignore-default-filter --message-format json >"$work/base-universe.json"
)
ci plan --universe "$work/base-universe.json" --archive "$work/base-tests.tar.zst" --workers 1 --sha "$base_sha" \
  --identity-from "$plan" --retry-policy "$work/no-retries.toml" --output "$work/built"
status=0
ci base-diff --baseline "$work/built/plan.json" "${diff_args[@]}" || status=$?
echo "nextest base diff against the fallback base plan at $base_sha: exit $status" >>"$summary"
exit "$status"
