#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "::error::$*" >&2
  exit 1
}

require_env() {
  local name="$1"
  if [ -z "${!name+x}" ]; then
    fail "required environment variable ${name} is not set"
  fi
}

require_env CHANGED_PATHS_RESULT
require_env FILTER_NAME
require_env FILTER_OUTPUT
require_env UPSTREAM_JOB_NAME
require_env UPSTREAM_RESULT

case "$CHANGED_PATHS_RESULT" in
  success)
    ;;
  failure|cancelled|skipped)
    fail "Changed paths result is '${CHANGED_PATHS_RESULT}'; failing closed instead of treating ${UPSTREAM_JOB_NAME} result '${UPSTREAM_RESULT}' as pass"
    ;;
  *)
    fail "Changed paths result is unexpected: '${CHANGED_PATHS_RESULT}'"
    ;;
esac

case "$FILTER_OUTPUT" in
  true)
    if [ "$UPSTREAM_RESULT" = "success" ]; then
      echo "${UPSTREAM_JOB_NAME} result: success; ${FILTER_NAME}=true"
      exit 0
    fi
    fail "${FILTER_NAME}=true requires ${UPSTREAM_JOB_NAME} to succeed; result was '${UPSTREAM_RESULT}'"
    ;;
  false)
    if [ "$UPSTREAM_RESULT" = "success" ] || [ "$UPSTREAM_RESULT" = "skipped" ]; then
      echo "${UPSTREAM_JOB_NAME} result: ${UPSTREAM_RESULT}; ${FILTER_NAME}=false (treated as pass)"
      exit 0
    fi
    fail "${FILTER_NAME}=false only permits '${UPSTREAM_JOB_NAME}' results success or skipped; result was '${UPSTREAM_RESULT}'"
    ;;
  *)
    fail "${FILTER_NAME} output is missing or invalid: '${FILTER_OUTPUT}'"
    ;;
esac
