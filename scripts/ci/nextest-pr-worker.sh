#!/usr/bin/env bash
set -euo pipefail

worker="$1"
phase="${2:-full}"
nextest_dir="$RUNNER_TEMP/nextest"
result_dir="$RUNNER_TEMP/nextest-result"
extract_dir="$RUNNER_TEMP/nextest-extract"
mkdir -p "$extract_dir"

echo 'Running cargo nextest run --archive-file with the verified worker config'
start="$(date +%s)"
code=0
cargo nextest run --archive-file "$nextest_dir/lib-tests.tar.zst" --workspace-remap "$GITHUB_WORKSPACE" \
  --extract-to "$extract_dir" --extract-overwrite \
  --config-file "$nextest_dir/plan/worker-$worker.toml" --profile ci --no-tests fail || code=$?
echo "$code" > "$result_dir/exit-code"
junit="$(find "$extract_dir" "$GITHUB_WORKSPACE/target" -path '*/nextest/ci/junit.xml' 2>/dev/null | head -n 1 || true)"
[ -z "$junit" ] || cp "$junit" "$result_dir/junit.xml"
if [ "$phase" = scope ]; then
  printf '%s %s\n' "$start" "$(date +%s)" > "$result_dir/scope-time"
  audit_code=0
  python3 scripts/ci/nextest_ci.py audit-scope-worker --plan "$nextest_dir/plan/plan.json" \
    --worker "$worker" --record "$result_dir" || audit_code=$?
  if [ "$code" = 0 ]; then
    code="$audit_code"
  fi
fi
exit "$code"
