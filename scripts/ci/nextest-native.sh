#!/usr/bin/env bash
# Native macOS/Windows lib tests through the PR worker path on one worker: archive, list, source
# compare, selection, plan, verify-archive, run with exit-code/JUnit record, audit and summary.
# Usage: nextest-native.sh <platform> <scope> pg-classifier|selectors:<file>
set -euo pipefail

platform="$1" scope="$2" selection="$3"
dir="$RUNNER_TEMP/nextest"
archive="$dir/lib-tests.tar.zst"
record="$dir/results/worker-0"
# The checked-out commit, which a manual trusted run may set apart from GITHUB_SHA.
sha="$(git rev-parse HEAD)"
ci() { python3 scripts/ci/nextest_ci.py "$@"; }
mkdir -p "$dir"

cargo nextest archive --lib --archive-file "$archive"
cargo nextest list --archive-file "$archive" --workspace-remap "$GITHUB_WORKSPACE" \
  --ignore-default-filter --message-format json >"$dir/universe.json"
python3 scripts/ci/rust_test_sources.py compare --platform "$platform" --list "$dir/universe.json" \
  --json-out "$dir/compare.json"
case "$selection" in
  pg-classifier)
    python3 scripts/ci/pg_test_classifier.py classify --list "$dir/universe.json" --json-out "$dir/selection.json"
    key=non_pg ;;
  selectors:*)
    ci select --universe "$dir/universe.json" --selectors "${selection#selectors:}" --json-out "$dir/selection.json"
    key=selected ;;
  *) echo "ERROR unknown selection $selection" >&2; exit 2 ;;
esac
ci plan --universe "$dir/universe.json" --only "$dir/selection.json" --only-key "$key" --archive "$archive" \
  --workers 1 --sha "$sha" --scope "$scope" --rust-version 1.94.1 \
  --nextest-version "$(cargo nextest --version | awk 'NR==1 {print $2}')" \
  --target "$(rustc -vV | sed -n 's/^host: //p')" --output "$dir/plan"
ci verify-archive --plan "$dir/plan/plan.json" --worker 0 --archive "$archive" \
  --config "$dir/plan/worker-0.toml" --sha "$sha" --record "$record"

code=0
env -u AGENTDESK_ROOT_DIR cargo nextest run --archive-file "$archive" --workspace-remap "$GITHUB_WORKSPACE" \
  --extract-to "$dir/extract" --extract-overwrite --config-file "$dir/plan/worker-0.toml" \
  --profile ci --no-tests fail || code=$?
echo "$code" >"$record/exit-code"
junit="$(find "$dir/extract" "$GITHUB_WORKSPACE/target" -path '*/nextest/ci/junit.xml' 2>/dev/null | head -n 1 || true)"
[ -z "$junit" ] || cp "$junit" "$record/junit.xml"

# The audit, not the run's exit code, decides: every planned test must have run and passed.
status=0
ci audit --plan "$dir/plan/plan.json" --results "$dir/results" --json-out "$dir/verdict.json" \
  --summary-out "$dir/summary.md" || status=$?
cat "$dir/summary.md" >>"${GITHUB_STEP_SUMMARY:-/dev/null}"
exit "$status"
