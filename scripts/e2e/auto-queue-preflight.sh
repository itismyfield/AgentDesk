#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FIXTURE=""
FIXTURE_SET=0
REPORT="${TMPDIR:-/tmp}/agentdesk-auto-queue-preflight.json"
REPORT_SET=0
REPORT_DIR="${TMPDIR:-/tmp}/agentdesk-auto-queue-preflight-reports"
SUITE=""

usage() {
  cat <<'USAGE'
Usage:
  scripts/e2e/auto-queue-preflight.sh [--fixture PATH] [--report PATH]
  scripts/e2e/auto-queue-preflight.sh --suite basic|advanced|all [--report-dir DIR]

Runs the sandbox fixture-mode auto-queue E2E preflight harness against an
in-process test API and a temporary PostgreSQL database. Default mode does not
mutate production GitHub cards, issues, PRs, branches, dispatch channels, or
live sessions.

Suite mode writes one JSON report per fixture into --report-dir.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --fixture)
      FIXTURE="$2"
      FIXTURE_SET=1
      shift 2
      ;;
    --report)
      REPORT="$2"
      REPORT_SET=1
      shift 2
      ;;
    --report-dir)
      REPORT_DIR="$2"
      shift 2
      ;;
    --suite)
      SUITE="$2"
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

if [[ "$FIXTURE_SET" -eq 1 && -n "$SUITE" ]]; then
  echo "--fixture and --suite are mutually exclusive" >&2
  exit 2
fi

declare -a FIXTURES
case "$SUITE" in
  "")
    FIXTURES=("${FIXTURE:-${ROOT_DIR}/tests/fixtures/auto-queue-preflight/basic.json}")
    ;;
  basic)
    FIXTURES=("${ROOT_DIR}/tests/fixtures/auto-queue-preflight/basic.json")
    ;;
  advanced)
    FIXTURES=(
      "${ROOT_DIR}/tests/fixtures/auto-queue-preflight/phase-gates.json"
      "${ROOT_DIR}/tests/fixtures/auto-queue-preflight/review.json"
      "${ROOT_DIR}/tests/fixtures/auto-queue-preflight/multislot-recovery.json"
      "${ROOT_DIR}/tests/fixtures/auto-queue-preflight/pipeline-compatibility.json"
    )
    ;;
  all)
    FIXTURES=(
      "${ROOT_DIR}/tests/fixtures/auto-queue-preflight/basic.json"
      "${ROOT_DIR}/tests/fixtures/auto-queue-preflight/phase-gates.json"
      "${ROOT_DIR}/tests/fixtures/auto-queue-preflight/review.json"
      "${ROOT_DIR}/tests/fixtures/auto-queue-preflight/multislot-recovery.json"
      "${ROOT_DIR}/tests/fixtures/auto-queue-preflight/pipeline-compatibility.json"
    )
    ;;
  *)
    echo "unknown suite: $SUITE" >&2
    usage >&2
    exit 2
    ;;
esac

if [[ "${#FIXTURES[@]}" -gt 1 && "$REPORT_SET" -eq 1 ]]; then
  echo "--report can only be used with a single fixture; use --report-dir for suites" >&2
  exit 2
fi

cd "$ROOT_DIR"

mkdir -p "$REPORT_DIR"

for fixture in "${FIXTURES[@]}"; do
  if [[ ! -f "$fixture" ]]; then
    echo "fixture not found: $fixture" >&2
    exit 2
  fi

  if [[ "${#FIXTURES[@]}" -eq 1 && ( "$REPORT_SET" -eq 1 || -z "$SUITE" ) ]]; then
    report="$REPORT"
  else
    name="$(basename "$fixture" .json)"
    report="${REPORT_DIR}/${name}.report.json"
  fi

  export AGENTDESK_AUTO_QUEUE_PREFLIGHT_FIXTURE="$fixture"
  export AGENTDESK_AUTO_QUEUE_PREFLIGHT_REPORT="$report"

  cargo test --lib auto_queue_preflight_fixture_sandbox_roundtrip -- --ignored
  echo "auto-queue preflight report: $report"
done
