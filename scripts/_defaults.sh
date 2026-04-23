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

setup_sccache_env() {
  local homebrew_bin="/opt/homebrew/bin"
  local sccache_bin=""

  case ":${PATH:-}:" in
    *":$homebrew_bin:"*) ;;
    *)
      if [ -x "$homebrew_bin/sccache" ]; then
        export PATH="$homebrew_bin:${PATH:-}"
      fi
      ;;
  esac

  if command -v sccache >/dev/null 2>&1; then
    sccache_bin="$(command -v sccache)"
  else
    return 1
  fi

  export SCCACHE_DIR="${SCCACHE_DIR:-$HOME/.cache/sccache}"
  export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-10G}"
  export RUSTC_WRAPPER="${RUSTC_WRAPPER:-$sccache_bin}"
  mkdir -p "$SCCACHE_DIR"
}

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

_health_json_has_jq() {
  command -v jq >/dev/null 2>&1
}

_health_json_compact() {
  printf '%s' "$1" | tr -d '\n'
}

_health_json_get_string_field() {
  local health_json="$1"
  local key="$2"
  local match

  [ -n "$health_json" ] || return 1

  if _health_json_has_jq; then
    printf '%s' "$health_json" | jq -r ".$key // empty" 2>/dev/null
    return
  fi

  match=$(
    _health_json_compact "$health_json" \
      | grep -Eo "\"$key\"[[:space:]]*:[[:space:]]*\"[^\"]*\"" \
      | head -n 1 \
      || true
  )
  [ -n "$match" ] || return 0
  printf '%s' "$match" | sed -E 's/^[^:]*:[[:space:]]*"//; s/"$//'
}

_health_json_get_string_array_csv() {
  local health_json="$1"
  local key="$2"
  local match

  [ -n "$health_json" ] || return 1

  if _health_json_has_jq; then
    printf '%s' "$health_json" | jq -r "(.${key} // []) | join(\",\")" 2>/dev/null
    return
  fi

  match=$(
    _health_json_compact "$health_json" \
      | grep -Eo "\"$key\"[[:space:]]*:[[:space:]]*\\[[^]]*\\]" \
      | head -n 1 \
      || true
  )
  [ -n "$match" ] || return 0

  printf '%s' "$match" \
    | sed -E 's/^[^[]*\[//; s/\]$//; s/"[[:space:]]*,[[:space:]]*"/,/g; s/^"//; s/"$//'
}

_health_json_field_is_true() {
  local health_json="$1"
  local key="$2"

  [ -n "$health_json" ] || return 1

  if _health_json_has_jq; then
    printf '%s' "$health_json" | jq -e ".$key == true" >/dev/null 2>&1
    return
  fi

  _health_json_compact "$health_json" \
    | grep -Eq "\"$key\"[[:space:]]*:[[:space:]]*true([[:space:]]*[,}])"
}

_health_json_field_exists() {
  local health_json="$1"
  local key="$2"

  [ -n "$health_json" ] || return 1

  if _health_json_has_jq; then
    printf '%s' "$health_json" | jq -e "has(\"$key\")" >/dev/null 2>&1
    return
  fi

  _health_json_compact "$health_json" \
    | grep -Eq "\"$key\"[[:space:]]*:"
}

_health_json_status() {
  local health_json="$1"
  _health_json_get_string_field "$health_json" "status"
}

_health_json_reasons() {
  local health_json="$1"
  _health_json_get_string_array_csv "$health_json" "degraded_reasons"
}

_health_json_reconcile_only() {
  local health_json="$1"
  local reasons_csv reason
  [ -n "$health_json" ] || return 1

  if _health_json_has_jq; then
    printf '%s' "$health_json" | jq -e '
      .status == "degraded"
      and (.db == true)
      and ((.degraded_reasons // []) | length > 0)
      and all((.degraded_reasons // [])[]; test("^provider:[^:]+:reconcile_in_progress$"))
    ' >/dev/null 2>&1
    return
  fi

  [ "$(_health_json_status "$health_json")" = "degraded" ] || return 1
  _health_json_field_is_true "$health_json" "db" || return 1

  reasons_csv=$(_health_json_reasons "$health_json" || true)
  [ -n "$reasons_csv" ] || return 1

  while IFS=, read -r reason; do
    [ -n "$reason" ] || return 1
    [[ "$reason" =~ ^provider:[^:]+:reconcile_in_progress$ ]] || return 1
  done <<< "$reasons_csv"

  return 0
}

health_json_is_ready() {
  local health_json="$1"
  local require_dashboard="${2:-0}"
  local allow_reconcile_degraded="${3:-1}"
  local status=""

  [ -n "$health_json" ] || return 1
  _health_json_field_is_true "$health_json" "db" || return 1

  if [ "$require_dashboard" = "1" ]; then
    _health_json_field_is_true "$health_json" "dashboard" || return 1
  fi

  status=$(_health_json_status "$health_json")

  if _health_json_field_exists "$health_json" "server_up"; then
    _health_json_field_is_true "$health_json" "server_up" || return 1
    [ "$status" = "unhealthy" ] && return 1
    return 0
  fi

  if [ "$status" = "healthy" ]; then
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

health_turn_snapshot() {
  local port="$1"
  local health_json
  health_json=$(curl -sf --max-time 3 "http://${ADK_DEFAULT_LOOPBACK}:${port}/api/health" 2>/dev/null) || return 1

  if _health_json_has_jq; then
    printf '%s\n' "$health_json" | jq -r '
      [
        (.global_active // 0),
        (.global_finalizing // 0),
        (.queue_depth // 0)
      ] | @tsv
    ' 2>/dev/null | tr '\t' ' '
    return
  fi

  local active finalizing queue_depth
  active=$(printf '%s' "$health_json" | grep -o '"global_active":[0-9]*' | head -1 | cut -d: -f2)
  finalizing=$(printf '%s' "$health_json" | grep -o '"global_finalizing":[0-9]*' | head -1 | cut -d: -f2)
  queue_depth=$(printf '%s' "$health_json" | grep -o '"queue_depth":[0-9]*' | head -1 | cut -d: -f2)
  echo "${active:-0} ${finalizing:-0} ${queue_depth:-0}"
}

wait_for_live_turns_to_drain_or_fail() {
  local scope="$1"
  local label="$2"
  local port="$3"
  local max_wait="${4:-120}"
  local poll_secs="${5:-2}"
  # Turns themselves are preserved across restart via silent reattach (#43e3cacc);
  # this flag only skips the drain wait, at the cost of possibly truncating a
  # mid-stream Discord response during the SIGTERM window.
  #
  # #899: default is now `1` (bypass). In the self-hosted promote topology the
  # operator agent running the promote IS a live turn on release, so drain
  # would time out nearly always. The stream hiccup is acceptable and #826 /
  # #896 guarantee recovery via watcher silent-reattach + inflight rebind.
  # Set `AGENTDESK_SKIP_TURN_DRAIN=0` to force the classic drain-wait.
  local skip_drain="${AGENTDESK_SKIP_TURN_DRAIN:-1}"
  local waited=0
  local active=0 finalizing=0 queue_depth=0 live_turns=0 job_state=""

  if ! read -r active finalizing queue_depth <<EOF
$(health_turn_snapshot "$port")
EOF
  then
    job_state=$(_launchd_job_state "$label")
    if [ "$job_state" = "not running" ]; then
      echo "▸ [gate] ${scope} launchd job already not running — skipping live-turn drain check"
      return 0
    fi
    if [ "$skip_drain" = "1" ]; then
      echo "⚠ [gate] Unable to read ${scope} health on :${port} (launchd state: ${job_state:-unknown}) — proceeding due to AGENTDESK_SKIP_TURN_DRAIN=1"
      return 0
    fi
    echo "✗ [gate] Unable to confirm ${scope} turn drain on :${port} (launchd state: ${job_state:-unknown})"
    echo "  Refusing restart to avoid truncating mid-stream output."
    echo "  You opted into strict drain via AGENTDESK_SKIP_TURN_DRAIN=0;"
    echo "  remove that override (default=1) if a brief stream hiccup is acceptable."
    return 1
  fi

  live_turns=$(( active + finalizing ))
  if [ "$live_turns" -eq 0 ]; then
    if [ "${queue_depth:-0}" -gt 0 ]; then
      echo "▸ [gate] ${scope} has ${queue_depth} queued intervention(s) only — safe to restart"
    else
      echo "▸ [gate] ${scope} has no active/finalizing turns"
    fi
    return 0
  fi

  echo "▸ [gate] Waiting for ${scope} active/finalizing turns to drain (${live_turns} live; queued=${queue_depth})..."
  while [ "$live_turns" -gt 0 ] && [ "$waited" -lt "$max_wait" ]; do
    sleep "$poll_secs"
    waited=$(( waited + poll_secs ))
    if ! read -r active finalizing queue_depth <<EOF
$(health_turn_snapshot "$port")
EOF
    then
      job_state=$(_launchd_job_state "$label")
      if [ "$skip_drain" = "1" ]; then
        echo "⚠ [gate] Lost ${scope} health during drain wait after ${waited}s (launchd state: ${job_state:-unknown}) — proceeding due to AGENTDESK_SKIP_TURN_DRAIN=1"
        return 0
      fi
      echo "✗ [gate] Lost ${scope} health during drain wait after ${waited}s (launchd state: ${job_state:-unknown})"
      echo "  Refusing restart to avoid truncating mid-stream output."
      echo "  You opted into strict drain via AGENTDESK_SKIP_TURN_DRAIN=0;"
      echo "  remove that override (default=1) if a brief stream hiccup is acceptable."
      return 1
    fi
    live_turns=$(( active + finalizing ))
  done

  if [ "$live_turns" -gt 0 ]; then
    if [ "$skip_drain" = "1" ]; then
      echo "⚠ [gate] ${scope} still has ${live_turns} active/finalizing turn(s) after ${max_wait}s — proceeding due to AGENTDESK_SKIP_TURN_DRAIN=1 (queued=${queue_depth}); silent reattach will preserve turn state"
      return 0
    fi
    echo "✗ [gate] ${scope} still has ${live_turns} active/finalizing turn(s) after ${max_wait}s (queued=${queue_depth})"
    echo "  Refusing restart to avoid truncating mid-stream output."
    echo "  You opted into strict drain via AGENTDESK_SKIP_TURN_DRAIN=0;"
    echo "  retry after work finishes or remove that override (default=1) when a brief stream hiccup is acceptable."
    return 1
  fi

  echo "✓ [gate] ${scope} active/finalizing turns drained (${waited}s, queued=${queue_depth})"
  return 0
}
