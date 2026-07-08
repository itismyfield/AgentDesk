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
  export RUSTC_WRAPPER="$sccache_bin"
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

_launchd_domain() {
  local uid domain
  uid="$(id -u 2>/dev/null)" || return 1
  for domain in "gui/$uid" "user/$uid"; do
    if launchctl print "$domain" >/dev/null 2>&1; then
      printf '%s\n' "$domain"
      return 0
    fi
  done
  printf 'gui/%s\n' "$uid"
}

_launchd_service_target() {
  local label="$1"
  local domain
  domain="$(_launchd_domain)" || return 1
  printf '%s/%s\n' "$domain" "$label"
}

_launchd_job_state() {
  local label="$1"
  local target
  target="$(_launchd_service_target "$label")" || return 0
  launchctl print "$target" 2>/dev/null \
    | sed -n 's/^[[:space:]]*state = //p' \
    | head -n 1
}

_kickstart_launchd_job_if_needed() {
  local label="$1"
  local state
  state=$(_launchd_job_state "$label")
  if [ "$state" = "not running" ]; then
    echo "  ▸ launchd reports $label not running — kickstart"
    launchctl kickstart -k "$(_launchd_service_target "$label")" >/dev/null 2>&1 || true
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

  # #4348 review finding #2: match the TOP-LEVEL field only (jq's `.key` is
  # top-level), so a nested `"status":"..."` cannot shadow the root value.
  match=$(
    _health_json_top_level_compact "$health_json" \
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
  local raw

  [ -n "$health_json" ] || return 1

  if _health_json_has_jq; then
    printf '%s' "$health_json" | jq -r "(.${key} // []) | join(\",\")" 2>/dev/null
    return
  fi

  # #4348 review finding #4: read the TOP-LEVEL array only (jq evaluates the
  # root `.${key}`). A naive grep over the whole compacted body would pick up a
  # same-named array nested inside another object (e.g. subsystem.degraded_reasons),
  # accepting reconcile-only reasons that jq — reading the ABSENT top-level array
  # as `[]` — correctly rejects.
  raw=$(_health_json_top_level_field_raw "$key" "$(_health_json_compact "$health_json")")
  # Only a genuine top-level ARRAY value contributes reasons; anything else
  # (absent key, null, scalar, object) is treated as an empty list, matching
  # jq's `(.key // []) | join(",")` for our reason-list callers.
  case "$raw" in
    *\[*\]*) ;;
    *) return 0 ;;
  esac

  printf '%s' "$raw" \
    | sed -E 's/^[^[]*\[//; s/\]$//; s/"[[:space:]]*,[[:space:]]*"/,/g; s/^"//; s/"$//'
}

_health_json_top_level_only() {
  # #4348 review finding #2: the jq-less field checks below must interrogate the
  # ROOT object only — jq's `.field` / `has("field")` are top-level, so the
  # grep fallback has to match top-level too. A naive grep over the compacted
  # body matches ANY occurrence, so a nested object carrying `"server_up":true`
  # (malformed / future-shape body) would satisfy a top-level `server_up` check
  # that jq correctly REJECTS — a false-ready deploy path.
  #
  # This helper emits ONLY the brace-depth-1 portion of the root object: the
  # contents of any nested object/array are elided while the top-level scalar
  # key:value pairs (and their `,`/`}` delimiters) are preserved, so the
  # existing grep patterns keep working but can no longer see nested keys. It is
  # a pure-bash scan (no jq/python) that tracks JSON string state so braces or
  # brackets inside string values never skew the depth count. NOTE: because
  # nested containers are elided, callers that need ARRAY/object contents (e.g.
  # degraded_reasons via _health_json_get_string_array_csv, or the legitimately
  # nested latest_startup_doctor.skipped_reason) must NOT route through here.
  local compact="$1"
  local n=${#compact}
  local i ch out="" depth=0 in_string=0 escaped=0

  for (( i = 0; i < n; i++ )); do
    ch="${compact:i:1}"
    if [ "$in_string" -eq 1 ]; then
      [ "$depth" -eq 1 ] && out+="$ch"
      if [ "$escaped" -eq 1 ]; then
        escaped=0
      elif [ "$ch" = '\' ]; then
        escaped=1
      elif [ "$ch" = '"' ]; then
        in_string=0
      fi
      continue
    fi
    case "$ch" in
      '{'|'[')
        depth=$((depth + 1))
        [ "$depth" -eq 1 ] && out+="$ch"
        ;;
      '}'|']')
        [ "$depth" -eq 1 ] && out+="$ch"
        depth=$((depth - 1))
        ;;
      '"')
        in_string=1
        [ "$depth" -eq 1 ] && out+="$ch"
        ;;
      *)
        [ "$depth" -eq 1 ] && out+="$ch"
        ;;
    esac
  done

  printf '%s' "$out"
}

_health_json_top_level_compact() {
  # Compact + top-level-only, in one place so every scalar field check shares
  # the same top-level view of the body (#4348 review finding #2).
  local health_json="$1"
  _health_json_top_level_only "$(_health_json_compact "$health_json")"
}

_health_json_top_level_field_raw() {
  # #4348 review findings #3/#4: emit the RAW top-level value token for <key>
  # from the root object (or nothing if <key> is absent at the top level),
  # preserving the value's own nested contents INTACT — unlike
  # _health_json_top_level_only, which elides all nested contents. This is what
  # lets the jq-less path read `.degraded_reasons` (a top-level array whose
  # elements matter) and `.latest_startup_doctor` (a top-level object we then
  # descend into for skipped_reason) at the SAME paths jq uses, so a same-named
  # key buried in some other nested object cannot shadow the root value.
  #
  # Pure-bash scan: finds a string that sits in KEY position at brace-depth 1
  # (a depth-1 string immediately followed by `:`) and, on a name match,
  # captures the following value up to the next depth-1 `,` / `}` / `]`. JSON
  # string state is tracked throughout so punctuation inside string values never
  # confuses key detection, depth accounting, or the value boundary.
  local key="$1"
  local compact="$2"
  local n=${#compact}
  local i ch
  local depth=0 in_string=0 escaped=0
  local cur_str="" pending_key="" awaiting_colon=0
  local capturing=0 value="" cap_base=0

  for (( i = 0; i < n; i++ )); do
    ch="${compact:i:1}"

    if [ "$capturing" -eq 1 ]; then
      if [ "$in_string" -eq 1 ]; then
        value+="$ch"
        if [ "$escaped" -eq 1 ]; then
          escaped=0
        elif [ "$ch" = '\' ]; then
          escaped=1
        elif [ "$ch" = '"' ]; then
          in_string=0
        fi
        continue
      fi
      case "$ch" in
        '"') in_string=1; value+="$ch" ;;
        '{'|'[') depth=$((depth + 1)); value+="$ch" ;;
        '}'|']')
          if [ "$depth" -le "$cap_base" ]; then
            printf '%s' "$value"
            return 0
          fi
          depth=$((depth - 1)); value+="$ch"
          ;;
        ',')
          if [ "$depth" -eq "$cap_base" ]; then
            printf '%s' "$value"
            return 0
          fi
          value+="$ch"
          ;;
        *) value+="$ch" ;;
      esac
      continue
    fi

    if [ "$in_string" -eq 1 ]; then
      if [ "$escaped" -eq 1 ]; then
        escaped=0; cur_str+="$ch"
      elif [ "$ch" = '\' ]; then
        escaped=1; cur_str+="$ch"
      elif [ "$ch" = '"' ]; then
        in_string=0
        if [ "$depth" -eq 1 ]; then
          pending_key="$cur_str"
          awaiting_colon=1
        fi
      else
        cur_str+="$ch"
      fi
      continue
    fi

    case "$ch" in
      '"') in_string=1; cur_str=""; awaiting_colon=0 ;;
      ':')
        if [ "$awaiting_colon" -eq 1 ] && [ "$depth" -eq 1 ] && [ "$pending_key" = "$key" ]; then
          capturing=1; cap_base="$depth"; value=""
        fi
        awaiting_colon=0
        ;;
      '{'|'[') depth=$((depth + 1)); awaiting_colon=0 ;;
      '}'|']') depth=$((depth - 1)); awaiting_colon=0 ;;
      ' '|$'\t') ;;
      *) awaiting_colon=0 ;;
    esac
  done

  return 0
}

_health_json_field_is_true() {
  local health_json="$1"
  local key="$2"

  [ -n "$health_json" ] || return 1

  if _health_json_has_jq; then
    printf '%s' "$health_json" | jq -e ".$key == true" >/dev/null 2>&1
    return
  fi

  _health_json_top_level_compact "$health_json" \
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

  _health_json_top_level_compact "$health_json" \
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

  _health_json_top_level_compact "$health_json" \
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

_health_json_unhealthy_only_no_provider_runtimes() {
  # #4348 DEPLOY/RESTART readiness rescue — NOT a runtime /health change.
  # Returns 0 when the node is provably SERVING the new binary (server_up + db +
  # dashboard all true) and its ONLY deploy-BLOCKING condition is that no
  # provider runtimes are registered (leader-only / no-agent-session topology):
  # providers.is_empty() emits `no_providers_registered`, the startup doctor is
  # skipped with skipped_reason=no_provider_runtimes_registered, and status is
  # pinned to `unhealthy` forever even though the server is fully up.
  #
  # NAME/SCOPE NOTE (#4348 review finding #1): the `_only_` here means the only
  # deploy-BLOCKING cause is no-providers — it does NOT claim no-providers is
  # the *sole* condition on the node. A serving no-provider node may ALSO carry
  # a DEGRADED-severity axis (disk-low / stale outbox / pipeline warnings /
  # opencode), and it still reports status=unhealthy (severity never downgrades
  # Unhealthy→Degraded) with server_up=true, so this predicate still fires. That
  # is INTENTIONAL and SAFE, not a false-ready:
  #   • server_up && db && dashboard already prove the new binary is serving, so
  #     no broken node is green-lit;
  #   • those extra axes are DEGRADED severity = NON-BLOCKING for deploy — a
  #     provider-present node with the same axis reports status=degraded and
  #     PASSES the deploy gate today, so rescuing a no-provider node with a
  #     co-existing degraded axis is CONSISTENT with the existing gate, not a
  #     new risk;
  #   • the PUBLIC /api/health body STRIPS degraded_reasons, so proving
  #     "solely no-providers" from this body is impossible without switching the
  #     gate to the detailed body — a larger change we deliberately do NOT make.
  # The runtime /health endpoint intentionally keeps reporting unhealthy for
  # monitoring; only the deploy/rollback readiness gate opts in to this rescue,
  # and only for this EXACT deploy-blocking cause (server_up=false /
  # db_unavailable / any other unhealthy DEPLOY-BLOCKING reason must still fail
  # the gate).
  local health_json="$1"
  [ -n "$health_json" ] || return 1

  if _health_json_has_jq; then
    printf '%s' "$health_json" | jq -e '
      (.server_up == true)
      and (.db == true)
      and (.dashboard == true)
      and (.status == "unhealthy")
      and (.startup_status == "doctor_skipped")
      and (.latest_startup_doctor.skipped_reason == "no_provider_runtimes_registered")
    ' >/dev/null 2>&1
    return
  fi

  # jq-less fallback. Every predicate must hold, at the SAME paths jq reads.
  _health_json_field_is_true "$health_json" "server_up" || return 1
  _health_json_field_is_true "$health_json" "db" || return 1
  _health_json_field_is_true "$health_json" "dashboard" || return 1
  [ "$(_health_json_status "$health_json")" = "unhealthy" ] || return 1
  # startup_status is a TOP-LEVEL field (jq: .startup_status).
  [ "$(_health_json_get_string_field "$health_json" "startup_status")" = "doctor_skipped" ] || return 1
  # #4348 review finding #3: skipped_reason must be read from the TOP-LEVEL
  # latest_startup_doctor object specifically (jq:
  # .latest_startup_doctor.skipped_reason), NOT grepped anywhere in the body —
  # a decoy `skipped_reason` in some OTHER nested object must not satisfy this
  # while the real latest_startup_doctor.skipped_reason differs. Extract the
  # top-level object, then read its own top-level skipped_reason.
  local lsd
  lsd=$(_health_json_top_level_field_raw "latest_startup_doctor" "$(_health_json_compact "$health_json")")
  [ -n "$lsd" ] || return 1
  [ "$(_health_json_get_string_field "$lsd" "skipped_reason")" = "no_provider_runtimes_registered" ]
}

_migration_seq_from_name() {
  # "0079_relay_dead_letter.sql" -> "79". Strips leading zeros so the result is a
  # base-10 integer (avoids octal interpretation in `-gt` tests). Returns
  # non-zero when the name has no leading numeric prefix. See #4348.
  local name="$1" num
  [ -n "$name" ] || return 1
  num=$(printf '%s' "$name" | sed -E 's/^0*([0-9]+).*/\1/')
  case "$num" in
    ''|*[!0-9]*) return 1 ;;
  esac
  printf '%s' "$num"
}

_migration_advanced() {
  # #4348: TRUE (return 0) when the new deploy's latest migration is strictly
  # AHEAD of the rollback target's latest migration — i.e. rolling back would
  # strand the old binary behind an already-applied migration and brick it.
  # Fails CLOSED: if EITHER name cannot be resolved to a sequence number, treat
  # it as advanced (unsafe to roll back) rather than gamble the node. Returns 1
  # (safe to roll back) only when both resolve AND new <= old.
  local new_name="$1" old_name="$2" new_seq old_seq
  new_seq=$(_migration_seq_from_name "$new_name") || return 0
  old_seq=$(_migration_seq_from_name "$old_name") || return 0
  [ "$new_seq" -gt "$old_seq" ] && return 0
  return 1
}

health_json_is_ready() {
  local health_json="$1"
  local require_dashboard="${2:-0}"
  local allow_reconcile_degraded="${3:-1}"
  # #4348: when 1, treat a serving node whose only deploy-BLOCKING condition is
  # no registered provider runtimes as DEPLOY-READY (co-existing degraded/
  # non-blocking axes are permitted — see
  # _health_json_unhealthy_only_no_provider_runtimes). Default 0 keeps every
  # existing (non-deploy) caller's semantics unchanged.
  local allow_no_provider_runtimes="${4:-0}"
  local status=""

  [ -n "$health_json" ] || return 1
  _health_json_field_is_true "$health_json" "db" || return 1

  if [ "$require_dashboard" = "1" ]; then
    _health_json_field_is_true "$health_json" "dashboard" || return 1
  fi

  status=$(_health_json_status "$health_json")

  if _health_json_field_exists "$health_json" "server_up"; then
    _health_json_field_is_true "$health_json" "server_up" || return 1
    if [ "$status" = "unhealthy" ]; then
      # #4348: rescue a serving leader-only / no-session node whose only
      # deploy-BLOCKING cause is no_provider_runtimes_registered (co-existing
      # degraded/non-blocking axes are allowed — same as a provider-present
      # degraded node that passes the gate). server_up is already confirmed true
      # above, so db_unavailable can never take this branch.
      if [ "$allow_no_provider_runtimes" = "1" ] \
        && _health_json_unhealthy_only_no_provider_runtimes "$health_json"; then
        return 0
      fi
      return 1
    fi
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
  # #4348: opt-in — accept a serving node whose only deploy-BLOCKING condition
  # is no registered provider runtimes (co-existing degraded/non-blocking axes
  # permitted). Default 0 preserves existing callers.
  local allow_no_provider_runtimes="${7:-0}"

  # shellcheck disable=SC2034 # Read by callers after the function returns.
  WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON=""

  local i health_json status reasons
  for i in $(seq 1 "$retries"); do
    health_json=$(curl -s --max-time 5 "http://${ADK_DEFAULT_LOOPBACK}:${port}/api/health" 2>/dev/null || true)
    # shellcheck disable=SC2034 # Read by callers after the function returns.
    WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON="$health_json"

    if health_json_is_ready "$health_json" "$require_dashboard" "$allow_reconcile_degraded" "$allow_no_provider_runtimes"; then
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
      def provider_active:
        [(.providers // [])[] | (.active_turns // 0)] | add // 0;
      def mailbox_active:
        [(.mailboxes // [])[] | select(
          (.has_cancel_token == true)
          or (.inflight_state_present == true)
          or (.relay_health.bridge_inflight_present == true)
          or (.relay_health.mailbox_has_cancel_token == true)
          or (.relay_stall_state == "active_foreground_stream")
        )] | length;
      [
        (.global_active // 0),
        (.global_finalizing // 0),
        (.queue_depth // 0),
        (if (provider_active + mailbox_active) > 0 then 1 else 0 end)
      ] | @tsv
    ' 2>/dev/null | tr '\t' ' '
    return
  fi

  # jq-less fallback: require the field markers to be present in the body,
  # otherwise return 1 so callers do not silently default to "0 active".
  if ! printf '%s' "$health_json" | grep -Eq '"global_active"[[:space:]]*:[[:space:]]*[0-9]'; then
    return 1
  fi
  if ! printf '%s' "$health_json" | grep -Eq '"global_finalizing"[[:space:]]*:[[:space:]]*[0-9]'; then
    return 1
  fi
  local active finalizing queue_depth runtime_active
  active=$(printf '%s' "$health_json" | grep -Eo '"global_active"[[:space:]]*:[[:space:]]*[0-9]*' | head -1 | cut -d: -f2 | tr -d '[:space:]')
  finalizing=$(printf '%s' "$health_json" | grep -Eo '"global_finalizing"[[:space:]]*:[[:space:]]*[0-9]*' | head -1 | cut -d: -f2 | tr -d '[:space:]')
  queue_depth=$(printf '%s' "$health_json" | grep -Eo '"queue_depth"[[:space:]]*:[[:space:]]*[0-9]*' | head -1 | cut -d: -f2 | tr -d '[:space:]')
  runtime_active=0
  if printf '%s' "$health_json" | grep -Eq '"active_turns"[[:space:]]*:[[:space:]]*[1-9][0-9]*|"has_cancel_token"[[:space:]]*:[[:space:]]*true|"inflight_state_present"[[:space:]]*:[[:space:]]*true|"bridge_inflight_present"[[:space:]]*:[[:space:]]*true|"mailbox_has_cancel_token"[[:space:]]*:[[:space:]]*true|"relay_stall_state"[[:space:]]*:[[:space:]]*"active_foreground_stream"'; then
    runtime_active=1
  fi
  echo "${active:-0} ${finalizing:-0} ${queue_depth:-0} ${runtime_active:-0}"
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

_foreign_active_turns_or_empty() {
  # Prints one session_key per line for sessions whose status is
  # turn_active/turn_busy/active AND whose channel_id is NOT in the
  # exempt list. Used to block restart_pending from triggering a
  # dcserver bounce that would wipe an unrelated channel's inflight
  # anchor (2026-05-26 adk-cdx incident). Best-effort: returns empty on
  # API failure so this is purely an additive guard, never blocks a
  # legitimate restart when the API is unreachable.
  local port="$1"
  local exempt_csv="$2"
  local origin
  origin="$(_health_origin_header)"
  curl -fsS --max-time 5 -H "$origin" "http://${ADK_DEFAULT_LOOPBACK}:${port}/api/sessions" 2>/dev/null \
    | python3 -c '
import json, os, sys
try:
    data = json.loads(sys.stdin.read())
except Exception:
    sys.exit(0)
items = data.get("sessions") if isinstance(data, dict) else data
exempt = {c.strip() for c in os.environ.get("EXEMPT_CSV", "").split(",") if c.strip()}
for s in items or []:
    status = str(s.get("status", "")).lower()
    if status not in {"turn_active", "turn_busy", "active"}:
        continue
    key = str(s.get("session_key") or "")
    chan = str(s.get("channel_id") or s.get("channelId") or "")
    if chan in exempt:
        continue
    if any(cid and cid in key for cid in exempt):
        continue
    print(key or chan or "<unknown>")
' 2>/dev/null \
    || true
}

guard_no_foreign_active_turns_or_warn() {
  # Returns 0 (allow restart) when no foreign live turns are detected OR
  # when AGENTDESK_RESTART_ALLOW_FOREIGN_TURNS=1 is set. Returns 1 (refuse)
  # only when foreign live turns exist AND the operator did not opt-in to
  # override. Logs the busy sessions to stderr in either case so the
  # incident is observable in deploy logs.
  local port="$1"
  local exempt_csv="${2:-}"
  local busy
  busy="$(EXEMPT_CSV="$exempt_csv" _foreign_active_turns_or_empty "$port" "$exempt_csv")"
  if [ -z "$busy" ]; then
    return 0
  fi
  echo "⚠ [gate] live turn(s) outside exempt channels (exempt=[${exempt_csv:-none}]):" >&2
  printf '    - %s\n' $busy >&2
  if [ "${AGENTDESK_RESTART_ALLOW_FOREIGN_TURNS:-0}" = "1" ]; then
    echo "▸ [gate] AGENTDESK_RESTART_ALLOW_FOREIGN_TURNS=1 set — proceeding anyway" >&2
    return 0
  fi
  echo "✗ [gate] refusing restart_pending — set AGENTDESK_RESTART_ALLOW_FOREIGN_TURNS=1 to override" >&2
  return 1
}

request_restart_drain_mode_or_fail() {
  local scope="$1"
  local label="$2"
  local port="$3"
  local runtime_root="$4"
  local source="${5:-agentdesk-restart}"
  local exempt_csv="${6:-${AGENTDESK_RESTART_EXEMPT_CHANNELS:-}}"
  local ack_wait="${AGENTDESK_RESTART_DRAIN_ACK_WAIT:-20}"
  local waited=0
  local marker
  local tmp_marker
  local job_state

  if [ -z "$runtime_root" ]; then
    echo "✗ [gate] ${scope} runtime root is required for restart drain mode" >&2
    return 1
  fi

  # 2026-05-26 adk-cdx incident: block restart_pending when any non-exempt
  # channel has a live turn. Without this, destructive E2E that restart
  # release dcserver from a bot-driven channel orphans the bot's own
  # in-flight response. Callers (e.g. e2e wrappers) pass their E2E
  # channels via `exempt_csv` or AGENTDESK_RESTART_EXEMPT_CHANNELS so the
  # E2E scenarios themselves still work.
  if ! guard_no_foreign_active_turns_or_warn "$port" "$exempt_csv"; then
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
  local active=0 finalizing=0 queue_depth=0 runtime_active=0 live_turns=0 job_state=""

  if ! read -r active finalizing queue_depth runtime_active <<EOF
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
  if [ "$effective_live" -eq 0 ] && [ "$live_turns" -eq 0 ] && [ "${runtime_active:-0}" -gt 0 ]; then
    effective_live="${runtime_active:-0}"
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
    echo "⚠ [gate] ${scope} has ${effective_live} active/finalizing/runtime-evidence turn(s) (live=${live_turns}, runtime=${runtime_active:-0}, self=${self_hosted_self_turn}, queued=${queue_depth}) — proceeding due to AGENTDESK_SKIP_TURN_DRAIN=1; silent reattach will preserve turn state"
    return 0
  fi

  echo "▸ [gate] Waiting for ${scope} active/finalizing turns to drain (${effective_live} live, runtime=${runtime_active:-0}, self=${self_hosted_self_turn}; queued=${queue_depth})..."
  while [ "$effective_live" -gt 0 ] && [ "$waited" -lt "$max_wait" ]; do
    sleep "$poll_secs"
    waited=$(( waited + poll_secs ))
    if ! read -r active finalizing queue_depth runtime_active <<EOF
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
    if [ "$effective_live" -eq 0 ] && [ "$live_turns" -eq 0 ] && [ "${runtime_active:-0}" -gt 0 ]; then
      effective_live="${runtime_active:-0}"
    fi
  done

  if [ "$effective_live" -gt 0 ]; then
    echo "✗ [gate] ${scope} still has ${effective_live} active/finalizing/runtime-evidence turn(s) after ${max_wait}s (live=${live_turns}, runtime=${runtime_active:-0}, self=${self_hosted_self_turn}, queued=${queue_depth})"
    echo "  Refusing restart to avoid truncating mid-stream output."
    echo "  You opted into strict drain via AGENTDESK_SKIP_TURN_DRAIN=0;"
    echo "  retry after work finishes or remove that override (default=1) when a brief stream hiccup is acceptable."
    return 1
  fi

  echo "✓ [gate] ${scope} active/finalizing turns drained (${waited}s, queued=${queue_depth})"
  return 0
}
