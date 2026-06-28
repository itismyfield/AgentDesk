#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FIXTURE="${ROOT_DIR}/tests/fixtures/auto-queue-preflight/basic.json"
REPORT="${TMPDIR:-/tmp}/agentdesk-auto-queue-preflight.json"

usage() {
  cat <<'USAGE'
Usage:
  scripts/e2e/auto-queue-preflight.sh [--fixture PATH] [--report PATH]

Runs the sandbox fixture-mode auto-queue E2E preflight harness against an
in-process test API and a temporary PostgreSQL database. Default mode does not
mutate production GitHub cards, issues, PRs, branches, dispatch channels, or
live sessions.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --fixture)
      FIXTURE="$2"
      shift 2
      ;;
    --report)
      REPORT="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

export AGENTDESK_AUTO_QUEUE_PREFLIGHT_FIXTURE="$FIXTURE"
export AGENTDESK_AUTO_QUEUE_PREFLIGHT_REPORT="$REPORT"

cd "$ROOT_DIR"
cargo test --lib auto_queue_preflight_fixture_sandbox_roundtrip -- --ignored

echo "auto-queue preflight report: $REPORT"
