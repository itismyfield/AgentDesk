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

_apply_launchd_env_file_to_shell() {
  local env_file="$1"
  local raw_line parsed key value

  [ -f "$env_file" ] || return 0

  while IFS= read -r raw_line || [ -n "$raw_line" ]; do
    parsed=$(_parse_launchd_env_line "$raw_line") || continue
    key="${parsed%%$'\t'*}"
    value="${parsed#*$'\t'}"
    export "$key=$value"
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

_health_json_field_is_false() {
  local health_json="$1"
  local key="$2"

  [ -n "$health_json" ] || return 1

  if _health_json_has_jq; then
    printf '%s' "$health_json" | jq -e ".$key == false" >/dev/null 2>&1
    return
  fi

  _health_json_compact "$health_json" \
    | grep -Eq "\"$key\"[[:space:]]*:[[:space:]]*false([[:space:]]*[,}])"
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
    [ "$status" = "healthy" ] && return 0
    if [ "$allow_reconcile_degraded" = "1" ] \
      && _health_json_field_exists "$health_json" "fully_recovered" \
      && _health_json_field_is_false "$health_json" "fully_recovered"; then
      return 0
    fi
    if [ "$allow_reconcile_degraded" = "1" ] && _health_json_reconcile_only "$health_json"; then
      return 0
    fi
    return 1
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
  # Use /api/health/detail (auth-aware via _curl_health_auth_args) so that
  # global_active / global_finalizing are present even when restart_pending
  # is armed — public_health_json strips the counters from the redacted
  # /api/health body (#1447 review iteration 4 P2). We also drop `-f` so the
  # 503 body served while restart_pending is armed remains observable.
  health_json=$(curl -s --max-time 3 -H "$(_health_origin_header)" \
    "http://${ADK_DEFAULT_LOOPBACK}:${port}/api/health/detail" 2>/dev/null) || return 1
  [ -n "$health_json" ] || return 1

  if _health_json_has_jq; then
    # Require global_active and global_finalizing to be PRESENT (not just
    # non-zero). If the body is missing them — for instance because we hit
    # the auth shim or a redacted endpoint — fail closed instead of letting
    # AGENTDESK_SKIP_TURN_DRAIN=0 callers incorrectly conclude "no turns".
    if ! printf '%s\n' "$health_json" | jq -e '
      (has("global_active")) and (has("global_finalizing"))
    ' >/dev/null 2>&1; then
      return 1
    fi
    printf '%s\n' "$health_json" | jq -r '
      [
        (.global_active // 0),
        (.global_finalizing // 0),
        (.queue_depth // 0)
      ] | @tsv
    ' 2>/dev/null | tr '\t' ' '
    return
  fi

  # jq-less fallback: require the field markers to be present in the body,
  # otherwise return 1 so callers do not silently default to "0 active".
  if ! printf '%s' "$health_json" | grep -q '"global_active":[0-9]'; then
    return 1
  fi
  if ! printf '%s' "$health_json" | grep -q '"global_finalizing":[0-9]'; then
    return 1
  fi
  local active finalizing queue_depth
  active=$(printf '%s' "$health_json" | grep -o '"global_active":[0-9]*' | head -1 | cut -d: -f2)
  finalizing=$(printf '%s' "$health_json" | grep -o '"global_finalizing":[0-9]*' | head -1 | cut -d: -f2)
  queue_depth=$(printf '%s' "$health_json" | grep -o '"queue_depth":[0-9]*' | head -1 | cut -d: -f2)
  echo "${active:-0} ${finalizing:-0} ${queue_depth:-0}"
}

assert_restart_helpers_loaded() {
  # Preflight contract for scripts that source _defaults.sh expecting the
  # restart-drain helpers. Returns non-zero (so callers can `if !` and exit 1)
  # instead of letting a missing function silently `command not found`. See
  # #1447: silent fail of agentdesk-restart when these helpers were absent.
  local missing=()
  local fn
  for fn in \
    request_restart_drain_mode_or_fail \
    wait_for_live_turns_to_drain_or_fail \
    clear_restart_drain_mode; do
    if ! declare -F "$fn" >/dev/null 2>&1; then
      missing+=("$fn")
    fi
  done
  if [ "${#missing[@]}" -gt 0 ]; then
    echo "✗ [gate] required restart helper(s) missing from _defaults.sh: ${missing[*]}" >&2
    echo "  Refusing restart to avoid bypassing live-turn drain protection (#1447)." >&2
    return 1
  fi
  return 0
}

clear_restart_drain_mode() {
  local runtime_root="$1"
  if [ -z "$runtime_root" ]; then
    echo "✗ [gate] runtime root is required to clear restart drain mode" >&2
    return 1
  fi
  rm -f "$runtime_root/restart_pending"
}

_health_origin_header() {
  # auth_middleware (src/server/routes/auth.rs) treats requests with a
  # same-origin Origin header as authenticated even when server.auth_token
  # is configured. The restart skill runs on the same host as dcserver so
  # this is always true; otherwise the helper would be locked out of
  # /api/health/detail on auth-enabled deployments (#1447 review iter 4 P2).
  printf 'Origin: http://%s' "${ADK_DEFAULT_LOOPBACK}"
}

_restart_pending_acknowledged() {
  local port="$1"
  local detail_json
  # NOTE: do NOT pass `-f`. The runtime serves /api/health/detail as HTTP 503
  # the moment `restart_pending` flips to true (build_health_snapshot returns
  # `unhealthy` for restart-pending — see src/services/discord/health.rs), and
  # `-f` would drop the body and report failure exactly when we need to read
  # the body to confirm the gate is armed (#1447 review P1, iteration 2).
  detail_json=$(curl -s --max-time 3 -H "$(_health_origin_header)" \
    "http://${ADK_DEFAULT_LOOPBACK}:${port}/api/health/detail" 2>/dev/null) || return 1
  [ -n "$detail_json" ] || return 1

  # restart_pending is per-provider. Require EVERY provider that exposes
  # the field to report true — otherwise a multi-provider runtime can
  # accept new turns on an unsynced provider while we proceed to bootout
  # (#1447 review P2).
  if _health_json_has_jq; then
    printf '%s\n' "$detail_json" | jq -e '
      (.providers // [])
      | map(select(.restart_pending != null))
      | (length > 0) and all(.restart_pending == true)
    ' >/dev/null 2>&1
    return $?
  fi

  # jq-less fallback: every restart_pending occurrence must be true. If any
  # is false we fail closed; if none are present we cannot confirm and fail.
  if printf '%s' "$detail_json" | grep -q '"restart_pending":false'; then
    return 1
  fi
  printf '%s' "$detail_json" | grep -q '"restart_pending":true'
}

request_restart_drain_mode_or_fail() {
  local scope="$1"
  local label="$2"
  local port="$3"
  local runtime_root="$4"
  local source="${5:-agentdesk-restart}"
  local ack_wait="${AGENTDESK_RESTART_DRAIN_ACK_WAIT:-20}"
  local waited=0
  local marker
  local tmp_marker
  local job_state

  if [ -z "$runtime_root" ]; then
    echo "✗ [gate] ${scope} runtime root is required for restart drain mode" >&2
    return 1
  fi

  mkdir -p "$runtime_root" || {
    echo "✗ [gate] failed to create ${scope} runtime root: $runtime_root" >&2
    return 1
  }

  marker="$runtime_root/restart_pending"
  tmp_marker="${marker}.$$"
  {
    printf 'source=%s\n' "$source"
    printf 'scope=%s\n' "$scope"
    printf 'label=%s\n' "$label"
    date -u '+requested_at=%Y-%m-%dT%H:%M:%SZ'
  } >"$tmp_marker" || {
    echo "✗ [gate] failed to write restart drain marker: $tmp_marker" >&2
    return 1
  }
  mv "$tmp_marker" "$marker" || {
    rm -f "$tmp_marker"
    echo "✗ [gate] failed to publish restart drain marker: $marker" >&2
    return 1
  }

  while [ "$waited" -lt "$ack_wait" ]; do
    if _restart_pending_acknowledged "$port"; then
      echo "✓ [gate] ${scope} restart drain mode acknowledged by runtime"
      return 0
    fi
    # #1447 review P2: idle runtime may consume the marker (restart_ctrl
    # deletes restart_pending and calls exit(0) once all turns drain) before
    # our 1s poll observes the in-memory flag. If the marker we just wrote
    # has disappeared, the runtime acknowledged it the only way it can.
    if [ ! -e "$marker" ]; then
      echo "▸ [gate] ${scope} restart drain marker consumed by runtime — treating as acknowledged"
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done

  job_state=$(_launchd_job_state "$label")
  if [ "$job_state" = "not running" ]; then
    # #1447 review iter 4 P2: leaving the marker on disk causes the next
    # cold boot to enter drain mode, observe zero turns, delete the marker,
    # and call exit(0) — flapping under KeepAlive. The service is not
    # running, so there is nothing to drain; clear the marker and report
    # success.
    rm -f "$marker" 2>/dev/null || true
    echo "▸ [gate] ${scope} launchd job is not running; cleared restart drain marker (no in-flight turns to drain)"
    return 0
  fi
  # Late-arriving consumption: marker may have been consumed between the
  # last poll and the post-loop launchd check. Same ack semantics as above.
  if [ ! -e "$marker" ]; then
    echo "▸ [gate] ${scope} restart drain marker consumed by runtime during timeout window — treating as acknowledged"
    return 0
  fi

  echo "✗ [gate] ${scope} restart drain mode was not acknowledged within ${ack_wait}s" >&2
  echo "  Refusing restart to avoid bypassing live-turn drain protection." >&2
  clear_restart_drain_mode "$runtime_root" || true
  return 1
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
  # #899: default is `1` (bypass). #1686: skip=1 now exits immediately after
  # a single snapshot instead of running the full max_wait timer — the prior
  # behaviour wasted the entire timeout on every self-hosted promote because
  # the operator agent's own turn is always live (it's the parent of the
  # deploy script). Set `AGENTDESK_SKIP_TURN_DRAIN=0` to force the classic
  # drain-wait when chunk-level integrity matters (external host, scheduled
  # maintenance window, post-incident strict mode).
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

  # #1686: self-hosted promote topology — when the deploy script is the
  # detached child of an operator agent's turn, that turn will never drain
  # during this run because IT is the deploy parent. Subtract one from the
  # live count so the strict path doesn't deadlock against itself, and so
  # the bypass path can report a meaningful "0 effective live" snapshot.
  local self_hosted_self_turn=0
  if [ "${AGENTDESK_DEPLOY_DETACHED_CHILD:-0}" = "1" ] && [ -n "${AGENTDESK_REPORT_CHANNEL_ID:-}" ]; then
    self_hosted_self_turn=1
  fi
  local effective_live=$(( live_turns - self_hosted_self_turn ))
  if [ "$effective_live" -lt 0 ]; then
    effective_live=0
  fi

  if [ "$effective_live" -eq 0 ]; then
    if [ "$live_turns" -gt 0 ]; then
      echo "▸ [gate] ${scope} has ${live_turns} live turn(s) all attributable to the operator's own deploy turn — safe to restart (queued=${queue_depth})"
    elif [ "${queue_depth:-0}" -gt 0 ]; then
      echo "▸ [gate] ${scope} has ${queue_depth} queued intervention(s) only — safe to restart"
    else
      echo "▸ [gate] ${scope} has no active/finalizing turns"
    fi
    return 0
  fi

  # #1686: skip=1 → single snapshot, no wait loop. The earlier implementation
  # waited the full max_wait before warning + proceeding, which wasted 120s
  # per self-hosted promote (the operator turn never drains in-process).
  if [ "$skip_drain" = "1" ]; then
    echo "⚠ [gate] ${scope} has ${effective_live} active/finalizing turn(s) (live=${live_turns}, self=${self_hosted_self_turn}, queued=${queue_depth}) — proceeding due to AGENTDESK_SKIP_TURN_DRAIN=1; silent reattach will preserve turn state"
    return 0
  fi

  echo "▸ [gate] Waiting for ${scope} active/finalizing turns to drain (${effective_live} live, self=${self_hosted_self_turn}; queued=${queue_depth})..."
  while [ "$effective_live" -gt 0 ] && [ "$waited" -lt "$max_wait" ]; do
    sleep "$poll_secs"
    waited=$(( waited + poll_secs ))
    if ! read -r active finalizing queue_depth <<EOF
$(health_turn_snapshot "$port")
EOF
    then
      job_state=$(_launchd_job_state "$label")
      echo "✗ [gate] Lost ${scope} health during drain wait after ${waited}s (launchd state: ${job_state:-unknown})"
      echo "  Refusing restart to avoid truncating mid-stream output."
      echo "  You opted into strict drain via AGENTDESK_SKIP_TURN_DRAIN=0;"
      echo "  remove that override (default=1) if a brief stream hiccup is acceptable."
      return 1
    fi
    live_turns=$(( active + finalizing ))
    effective_live=$(( live_turns - self_hosted_self_turn ))
    if [ "$effective_live" -lt 0 ]; then
      effective_live=0
    fi
  done

  if [ "$effective_live" -gt 0 ]; then
    echo "✗ [gate] ${scope} still has ${effective_live} active/finalizing turn(s) after ${max_wait}s (live=${live_turns}, self=${self_hosted_self_turn}, queued=${queue_depth})"
    echo "  Refusing restart to avoid truncating mid-stream output."
    echo "  You opted into strict drain via AGENTDESK_SKIP_TURN_DRAIN=0;"
    echo "  retry after work finishes or remove that override (default=1) when a brief stream hiccup is acceptable."
    return 1
  fi

  echo "✓ [gate] ${scope} active/finalizing turns drained (${waited}s, queued=${queue_depth})"
  return 0
}
