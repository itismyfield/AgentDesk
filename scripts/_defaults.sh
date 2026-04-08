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
