#!/usr/bin/env bash
# ── Shared defaults loader ──────────────────────────────────────────────────
# Sources port/host from the project-root defaults.json (single source of truth).
# Intended to be sourced by other scripts: . "$SCRIPT_DIR/_defaults.sh"

_DEFAULTS_JSON="${_DEFAULTS_JSON:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/defaults.json}"

_read_default() {
  # Simple JSON value reader — no jq required.
  # Usage: _read_default key fallback
  local key="$1" fallback="$2"
  if [ -f "$_DEFAULTS_JSON" ]; then
    local val
    val=$(sed -n "s/.*\"$key\"[[:space:]]*:[[:space:]]*\"\{0,1\}\([^,\"]*\)\"\{0,1\}.*/\1/p" "$_DEFAULTS_JSON" | head -1)
    [ -n "$val" ] && echo "$val" && return
  fi
  echo "$fallback"
}

ADK_DEFAULT_PORT=$(_read_default port 8791)
ADK_DEFAULT_HOST=$(_read_default host "0.0.0.0")
ADK_DEFAULT_LOOPBACK=$(_read_default loopback "127.0.0.1")
export ADK_DEFAULT_PORT ADK_DEFAULT_HOST ADK_DEFAULT_LOOPBACK

_trim_whitespace() {
  local value="$1"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s' "$value"
}

_parse_launchd_env_line() {
  local line="$1"
  local key value first last

  line="${line//$'\r'/}"
  line=$(_trim_whitespace "$line")
  [ -n "$line" ] || return 1

  case "$line" in
    \#*) return 1 ;;
  esac

  if [[ "$line" == export[[:space:]]* ]]; then
    line="${line#export }"
    line=$(_trim_whitespace "$line")
  fi

  [[ "$line" == *=* ]] || return 1

  key="${line%%=*}"
  value="${line#*=}"
  key=$(_trim_whitespace "$key")
  value=$(_trim_whitespace "$value")

  [ -n "$key" ] || return 1
  [[ "$key" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || return 1

  if [ "${#value}" -ge 2 ]; then
    first="${value:0:1}"
    last="${value: -1}"
    if { [ "$first" = '"' ] && [ "$last" = '"' ]; } || { [ "$first" = "'" ] && [ "$last" = "'" ]; }; then
      value="${value:1:${#value}-2}"
    fi
  fi

  printf '%s\t%s\n' "$key" "$value"
}

_plistbuddy_escape_string() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '%s' "$value"
}

sync_launchd_plist_environment_from_file() {
  local plist_path="$1"
  local env_file="$2"
  local plistbuddy="/usr/libexec/PlistBuddy"
  local raw_line parsed key value escaped_value

  [ -f "$plist_path" ] || return 0
  [ -f "$env_file" ] || return 0
  [ -x "$plistbuddy" ] || return 0

  "$plistbuddy" -c "Print :EnvironmentVariables" "$plist_path" >/dev/null 2>&1 \
    || "$plistbuddy" -c "Add :EnvironmentVariables dict" "$plist_path" >/dev/null

  while IFS= read -r raw_line || [ -n "$raw_line" ]; do
    parsed=$(_parse_launchd_env_line "$raw_line") || continue
    key="${parsed%%$'\t'*}"
    value="${parsed#*$'\t'}"
    escaped_value=$(_plistbuddy_escape_string "$value")
    "$plistbuddy" -c "Delete :EnvironmentVariables:$key" "$plist_path" >/dev/null 2>&1 || true
    "$plistbuddy" -c "Add :EnvironmentVariables:$key string \"$escaped_value\"" "$plist_path" >/dev/null
  done < "$env_file"
}

_launchd_job_state() {
  local label="$1"
  launchctl print "gui/$(id -u)/$label" 2>/dev/null \
    | sed -n 's/^[[:space:]]*state = //p' \
    | head -n 1
}

_kickstart_launchd_job_if_needed() {
  local label="$1"
  local state
  state=$(_launchd_job_state "$label")
  if [ "$state" = "not running" ]; then
    echo "  ▸ launchd reports $label not running — kickstart"
    launchctl kickstart -k "gui/$(id -u)/$label" >/dev/null 2>&1 || true
    return 0
  fi
  return 1
}

_health_json_status() {
  local health_json="$1"
  [ -n "$health_json" ] || return 1
  printf '%s' "$health_json" | jq -r '.status // empty' 2>/dev/null
}

_health_json_reasons() {
  local health_json="$1"
  [ -n "$health_json" ] || return 1
  printf '%s' "$health_json" | jq -r '(.degraded_reasons // []) | join(",")' 2>/dev/null
}

_health_json_reconcile_only() {
  local health_json="$1"
  [ -n "$health_json" ] || return 1
  printf '%s' "$health_json" | jq -e '
    .status == "degraded"
    and (.db == true)
    and ((.degraded_reasons // []) | length > 0)
    and all((.degraded_reasons // [])[]; test("^provider:[^:]+:reconcile_in_progress$"))
  ' >/dev/null 2>&1
}

health_json_is_ready() {
  local health_json="$1"
  local require_dashboard="${2:-0}"
  local allow_reconcile_degraded="${3:-1}"

  [ -n "$health_json" ] || return 1
  printf '%s' "$health_json" | jq -e '.db == true' >/dev/null 2>&1 || return 1

  if [ "$require_dashboard" = "1" ]; then
    printf '%s' "$health_json" | jq -e '.dashboard == true' >/dev/null 2>&1 || return 1
  fi

  if printf '%s' "$health_json" | jq -e '.status == "healthy"' >/dev/null 2>&1; then
    return 0
  fi

  if [ "$allow_reconcile_degraded" = "1" ] && _health_json_reconcile_only "$health_json"; then
    return 0
  fi

  return 1
}

wait_for_http_service_health() {
  local label="$1"
  local port="$2"
  local retries="$3"
  local delay_secs="$4"
  local require_dashboard="${5:-0}"
  local allow_reconcile_degraded="${6:-1}"

  # shellcheck disable=SC2034 # Read by callers after the function returns.
  WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON=""

  local i health_json status reasons
  for i in $(seq 1 "$retries"); do
    health_json=$(curl -s --max-time 5 "http://${ADK_DEFAULT_LOOPBACK}:${port}/api/health" 2>/dev/null || true)
    # shellcheck disable=SC2034 # Read by callers after the function returns.
    WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON="$health_json"

    if health_json_is_ready "$health_json" "$require_dashboard" "$allow_reconcile_degraded"; then
      return 0
    fi

    _kickstart_launchd_job_if_needed "$label" || true

    status=$(_health_json_status "$health_json" || true)
    reasons=$(_health_json_reasons "$health_json" || true)
    if [ -n "$status" ]; then
      if [ -n "$reasons" ]; then
        echo "  ▸ Attempt $i/$retries — status=$status reasons=$reasons"
      else
        echo "  ▸ Attempt $i/$retries — status=$status"
      fi
    else
      echo "  ▸ Attempt $i/$retries — not healthy yet"
    fi

    if [ "$i" -lt "$retries" ]; then
      sleep "$delay_secs"
    fi
  done

  return 1
}
