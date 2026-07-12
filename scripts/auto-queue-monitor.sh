#!/usr/bin/env bash
# Auto-queue monitor for durable STUCK/ANOMALY/REVIEW_LONG incident alerts.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/_defaults.sh
. "$SCRIPT_DIR/_defaults.sh"

REL_PORT="${AGENTDESK_REL_PORT:-$ADK_DEFAULT_PORT}"
API="http://${ADK_DEFAULT_LOOPBACK}:${REL_PORT}"
INTERVAL="${AQ_MONITOR_INTERVAL:-30}"
STUCK_THRESHOLD_MIN="${AQ_STUCK_THRESHOLD_MIN:-30}"
REVIEW_THRESHOLD_MIN="${AQ_REVIEW_THRESHOLD_MIN:-60}"
NOTIFY_CHANNEL="${AQ_MONITOR_CHANNEL:-1479671298497183835}"
AGENTDESK_BIN="${AGENTDESK_BIN:-agentdesk}"
PYTHON="${PYTHON:-python3}"
NOTIFY_KEY="${AQ_MONITOR_NOTIFY_KEY:-}"
NOTIFY_TOKEN_FILE="${AQ_MONITOR_NOTIFY_TOKEN_FILE:-${HOME}/.adk/release/credential/notify_bot_token}"
if [ -z "$NOTIFY_KEY" ] && [ -r "$NOTIFY_TOKEN_FILE" ]; then
  NOTIFY_KEY=$("$PYTHON" -c \
    'import hashlib, sys; token=sys.stdin.read().strip(); print("discord_" + hashlib.sha256(token.encode()).hexdigest()[:16])' \
    < "$NOTIFY_TOKEN_FILE")
fi
COOLDOWN_SECS="${AQ_MONITOR_COOLDOWN_SECS:-1800}"
STATE_FILE="${AQ_MONITOR_STATE_FILE:-${HOME}/.adk/release/data/auto-queue-monitor-state.json}"
STATE_HELPER="$SCRIPT_DIR/auto_queue_monitor_state.py"

api_get() {
  curl -sf "$API$1"
}

api_post_json() {
  local path="$1"
  local body="$2"
  curl -sf "$API$path" -X POST -H "Content-Type: application/json" -d "$body" >/dev/null
}

notify_anomaly() {
  local msg="$1"
  local action_id="$2"
  local action="$3"
  local incident_kind="$4"
  local body
  body=$(jq -n -c \
    --arg target "channel:$NOTIFY_CHANNEL" \
    --arg content "$msg" \
    --arg action_id "$action_id" \
    --arg action "$action" \
    --arg kind "$incident_kind" \
    '{target:$target, content:$content, action_id:$action_id, action:$action, kind:$kind}')
  if api_post_json "/api/message-outbox/monitor-alerts" "$body"; then
    return 0
  fi

  # STUCK/ANOMALY are actionable. Their durable row normally uses the
  # announce bot and wakes the operations-channel AgentDesk role. If the API
  # or PG is unavailable, preserve human visibility with a bot-token direct
  # post. REVIEW_LONG and all recovery notices stay notify-only and keep the
  # old durable retry semantics (#4449).
  if [ "$action" != "alert" ] \
    || { [ "$incident_kind" != "STUCK" ] && [ "$incident_kind" != "ANOMALY" ]; }; then
    return 1
  fi
  if [ -z "$NOTIFY_KEY" ]; then
    echo "auto-queue monitor: notify bot credential unavailable; direct fallback deferred" >&2
    return 1
  fi
  "$AGENTDESK_BIN" discord-sendmessage \
    --channel "$NOTIFY_CHANNEL" --key "$NOTIFY_KEY" --message "$msg"
}

entry_age_min() {
  local ref_ms="$1"
  local now_ms="${2:-$(($(date +%s) * 1000))}"
  if [ -z "$ref_ms" ] || [ "$ref_ms" = "null" ] \
    || ! [[ "$ref_ms" =~ ^[0-9]+$ ]] || [ "$ref_ms" -le 0 ]; then
    echo 0
    return
  fi
  echo $(((now_ms - ref_ms) / 60000))
}

dispatch_status_for_entry() {
  local dispatch_id="$1"
  local payload status
  if [ -z "$dispatch_id" ] || [ "$dispatch_id" = "null" ]; then
    echo ""
    return
  fi
  if ! payload=$(api_get "/api/dispatches/$dispatch_id" 2>/dev/null); then
    echo "__UNKNOWN__"
    return
  fi
  if ! status=$(printf '%s' "$payload" \
    | jq -er '.dispatch.status // .status | select(type == "string" and length > 0)' \
      2>/dev/null); then
    echo "__UNKNOWN__"
    return
  fi
  printf '%s\n' "$status"
}

session_statuses_for_dispatch() {
  local dispatch_id="$1"
  local sessions_json="$2"
  if [ -z "$dispatch_id" ] || [ "$dispatch_id" = "null" ]; then
    echo ""
    return
  fi
  printf '%s' "$sessions_json" \
    | jq -r --arg did "$dispatch_id" \
      '(.sessions // []) | map(select(.active_dispatch_id == $did) | .status) | join(",")'
}

append_condition() {
  local kind="$1"
  local key="$2"
  local alert="$3"
  local recovery="$4"
  jq -n -c \
    --arg kind "$kind" \
    --arg key "$key" \
    --arg alert "$alert" \
    --arg recovery "$recovery" \
    '{kind:$kind, key:$key, alert:$alert, recovery:$recovery}' \
    >> "$CONDITIONS_JSONL"
}

append_unknown_key() {
  local key="$1"
  printf '%s\n' "$key" | jq -R -c '.' >> "$UNKNOWN_JSONL"
}

collect_active_conditions() {
  local status_json="$1"
  local run_status="$2"
  local run_id="$3"
  local sessions_json="$4"
  local sessions_available="$5"
  local now_ms="$6"

  if [ "$run_status" != "active" ] \
    && [ "$run_status" != "pending" ] \
    && [ "$run_status" != "paused" ]; then
    return
  fi

  printf '%s' "$status_json" \
    | jq -c '.entries[]? | select(.status != "done" and .status != "skipped")' \
    | while IFS= read -r entry; do
      local issue card_status q_status ref_ms age_min dispatch_id dispatch_status
      local entry_id review_round alert recovery session_statuses
      local stuck_key anomaly_key review_key review_ref_ms review_age_min
      issue=$(printf '%s' "$entry" | jq -r '.github_issue_number // "unknown"')
      entry_id=$(printf '%s' "$entry" | jq -r '.id // .entry_id // ("issue-" + ((.github_issue_number // "unknown") | tostring))')
      card_status=$(printf '%s' "$entry" | jq -r '.card_status // ""')
      q_status=$(printf '%s' "$entry" | jq -r '.status // ""')
      ref_ms=$(printf '%s' "$entry" | jq -r '.dispatched_at // .created_at // 0')
      age_min=$(entry_age_min "$ref_ms" "$now_ms")
      dispatch_id=$(printf '%s' "$entry" | jq -r '.dispatch_history[-1] // ""')
      dispatch_status=$(dispatch_status_for_entry "$dispatch_id")

      stuck_key="STUCK|${run_id}|${entry_id}|${dispatch_id}"
      anomaly_key="ANOMALY|${run_id}|${entry_id}|${dispatch_id}"

      if [ "$q_status" = "dispatched" ] && [ "$dispatch_status" = "__UNKNOWN__" ]; then
        append_unknown_key "$stuck_key"
        append_unknown_key "$anomaly_key"
      fi

      if [ "$q_status" = "dispatched" ] \
        && [ "$dispatch_status" = "dispatched" ]; then
        if [ "$sessions_available" != "true" ]; then
          append_unknown_key "$stuck_key"
        elif [ "${age_min:-0}" -gt "$STUCK_THRESHOLD_MIN" ]; then
          session_statuses=$(session_statuses_for_dispatch "$dispatch_id" "$sessions_json")
          case ",$session_statuses," in
            *,working,*|*,turn_active,*|*,awaiting_bg,*|*,awaiting_user,*|*,running,*|*,active,*) ;;
            *)
              alert="[auto-queue monitor] STUCK: #${issue} dispatched ${age_min}min, no active session"
              recovery="[auto-queue monitor] RECOVERED: STUCK #${issue} (${entry_id})"
              append_condition "STUCK" "$stuck_key" "$alert" "$recovery"
              ;;
          esac
        fi
      fi

      if [ "$dispatch_status" = "completed" ] && [ "$q_status" = "dispatched" ]; then
        alert="[auto-queue monitor] ANOMALY: #${issue} dispatch completed but entry not updated (card=${card_status})"
        recovery="[auto-queue monitor] RECOVERED: ANOMALY #${issue} (${entry_id})"
        append_condition "ANOMALY" "$anomaly_key" "$alert" "$recovery"
      fi

      if [ "$card_status" = "review" ]; then
        review_round=$(printf '%s' "$entry" | jq -r '.review_round // 0')
        review_key="REVIEW_LONG|${run_id}|${entry_id}|round-${review_round}"
        review_ref_ms=$(printf '%s' "$entry" | jq -r '.review_entered_at // 0')
        if [ -z "$review_ref_ms" ] || [ "$review_ref_ms" = "null" ] \
          || ! [[ "$review_ref_ms" =~ ^[0-9]+$ ]] || [ "$review_ref_ms" -le 0 ]; then
          append_unknown_key "$review_key"
        else
          review_age_min=$(entry_age_min "$review_ref_ms" "$now_ms")
          if [ "${review_age_min:-0}" -gt "$REVIEW_THRESHOLD_MIN" ]; then
            alert="[auto-queue monitor] REVIEW_LONG: #${issue} review ${review_age_min}min elapsed (round=${review_round})"
            recovery="[auto-queue monitor] RECOVERED: REVIEW_LONG #${issue} round ${review_round} (${entry_id})"
            append_condition "REVIEW_LONG" "$review_key" "$alert" "$recovery"
          fi
        fi
      fi
    done
}

monitor_once_unlocked() {
  local status_json run_status run_id sessions_json sessions_available now_epoch now_ms
  local temp_dir active_file unknown_file actions_file action_file action action_id action_kind incident_kind message

  if ! status_json=$(api_get "/api/queue/status" 2>/dev/null); then
    echo "auto-queue monitor: status API unavailable; preserving incident state" >&2
    return 0
  fi
  if ! printf '%s' "$status_json" \
    | jq -e 'type == "object" and has("run") and ((.run == null) or (.run | type == "object"))' \
      >/dev/null 2>&1; then
    echo "auto-queue monitor: malformed status payload; preserving incident state" >&2
    return 0
  fi
  run_status=$(printf '%s' "$status_json" | jq -r '.run.status // "inactive"')
  run_id=$(printf '%s' "$status_json" | jq -r '.run.id // "unknown-run"')

  sessions_available=true
  if ! sessions_json=$(api_get "/api/sessions" 2>/dev/null); then
    sessions_json='{"sessions":[]}'
    sessions_available=false
  elif ! printf '%s' "$sessions_json" | jq -e '
      type == "object"
      and ((.sessions // []) | type == "array")
      and all((.sessions // [])[];
        type == "object"
        and (.status | type == "string")
        and (.active_dispatch_id == null or (.active_dispatch_id | type == "string")))
    ' >/dev/null 2>&1; then
    sessions_json='{"sessions":[]}'
    sessions_available=false
  fi

  now_epoch="${AQ_MONITOR_NOW_EPOCH:-$(date +%s)}"
  now_ms=$((now_epoch * 1000))
  temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-auto-queue-monitor.XXXXXX")
  active_file="$temp_dir/active.json"
  unknown_file="$temp_dir/unknown.json"
  actions_file="$temp_dir/actions.jsonl"
  action_file="$temp_dir/action.json"
  CONDITIONS_JSONL="$temp_dir/conditions.jsonl"
  UNKNOWN_JSONL="$temp_dir/unknown.jsonl"
  export CONDITIONS_JSONL UNKNOWN_JSONL
  : > "$CONDITIONS_JSONL"
  : > "$UNKNOWN_JSONL"

  collect_active_conditions \
    "$status_json" "$run_status" "$run_id" "$sessions_json" \
    "$sessions_available" "$now_ms"
  jq -s '.' "$CONDITIONS_JSONL" > "$active_file"
  jq -s 'unique' "$UNKNOWN_JSONL" > "$unknown_file"

  while true; do
    if ! "$PYTHON" "$STATE_HELPER" plan \
      --state-file "$STATE_FILE" \
      --active-file "$active_file" \
      --unknown-file "$unknown_file" \
      --now "$now_epoch" \
      --cooldown-secs "$COOLDOWN_SECS" > "$actions_file"; then
      echo "auto-queue monitor: state reconciliation failed; preserving state" >&2
      break
    fi
    action=$(head -n 1 "$actions_file")
    [ -n "$action" ] || break
    printf '%s\n' "$action" > "$action_file"
    action_kind=$(printf '%s' "$action" | jq -r '.action')
    incident_kind=$(printf '%s' "$action" | jq -r '.condition.kind')
    action_id=$(printf '%s' "$action" | jq -r '.action_id')
    if [ "$action_kind" = "recovery" ]; then
      message=$(printf '%s' "$action" | jq -r '.condition.recovery')
    else
      message=$(printf '%s' "$action" | jq -r '.condition.alert')
    fi
    echo "$message"
    if notify_anomaly "$message" "$action_id" "$action_kind" "$incident_kind"; then
      if ! "$PYTHON" "$STATE_HELPER" commit \
        --state-file "$STATE_FILE" --action-file "$action_file"; then
        echo "auto-queue monitor: durable notification queued but state commit lost CAS; will retry the same action ID" >&2
        break
      fi
    else
      echo "auto-queue monitor: durable notification enqueue failed; pending action preserved" >&2
      break
    fi
  done

  rm -rf "$temp_dir"
}

monitor_once() {
  "$PYTHON" "$STATE_HELPER" run-locked \
    --state-file "$STATE_FILE" -- \
    bash "$SCRIPT_DIR/auto-queue-monitor.sh" __monitor_once_unlocked
}

main() {
  while true; do
    monitor_once
    if [ "${AQ_MONITOR_ONCE:-0}" = "1" ]; then
      break
    fi
    sleep "$INTERVAL"
  done
}

if [ "${BASH_SOURCE[0]}" = "$0" ] && [ "${1:-}" = "__monitor_once_unlocked" ]; then
  monitor_once_unlocked
elif [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
