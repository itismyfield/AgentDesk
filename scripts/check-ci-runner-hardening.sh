#!/usr/bin/env bash
set -euo pipefail

fail=0

error() {
  echo "ERROR: $*" >&2
  fail=1
}

trusted_workflow=".github/workflows/ci-macos-trusted.yml"

if [ ! -f "$trusted_workflow" ]; then
  error "missing $trusted_workflow"
fi

for workflow in .github/workflows/*.yml; do
  [ -f "$workflow" ] || continue

  if grep -Eq '^[[:space:]]+pull_request(_target)?:' "$workflow"; then
    if grep -Eq 'MACOS_RUNNER|self-hosted' "$workflow"; then
      error "$workflow is pull_request-triggered and must not reference self-hosted macOS routing"
    fi
  fi

  if [ "$workflow" != "$trusted_workflow" ] && grep -q 'MACOS_RUNNER' "$workflow"; then
    error "$workflow references MACOS_RUNNER outside $trusted_workflow"
  fi

  if grep -q 'RUSTC_WRAPPER=' "$workflow" && ! grep -q 'SCCACHE_GHA_ENABLED=' "$workflow"; then
    error "$workflow clears RUSTC_WRAPPER but not SCCACHE_GHA_ENABLED"
  fi
done

if [ -f "$trusted_workflow" ]; then
  if grep -Eq '^[[:space:]]+pull_request(_target)?:' "$trusted_workflow"; then
    error "$trusted_workflow must not have a pull_request or pull_request_target trigger"
  fi
  grep -Eq '^[[:space:]]+push:' "$trusted_workflow" \
    || error "$trusted_workflow must have a trusted push trigger"
  grep -Eq '^[[:space:]]+workflow_dispatch:' "$trusted_workflow" \
    || error "$trusted_workflow must have a trusted workflow_dispatch trigger"
  grep -Eq '^[[:space:]]+merge_group:' "$trusted_workflow" \
    || error "$trusted_workflow must have a merge_group trigger"
  grep -q 'MACOS_RUNNER_GROUP' "$trusted_workflow" \
    || error "$trusted_workflow must require MACOS_RUNNER_GROUP for self-hosted routing"
fi

exit "$fail"
