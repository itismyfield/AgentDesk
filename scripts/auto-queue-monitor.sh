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
COOLDOWN_SECS="${AQ_MONITOR_COOLDOWN_SECS:-1800}"
STATE_FILE="${AQ_MONITOR_STATE_FILE:-${HOME}/.adk/release/data/auto-queue-monitor-state.json}"
STATE_HELPER="$SCRIPT_DIR/auto_queue_monitor_state.py"
PYTHON="${PYTHON:-python3}"

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
  local body
  body=$(jq -n -c \
    --arg target "channel:$NOTIFY_CHANNEL" \
    --arg content "$msg" \
    '{target:$target, content:$content, source:"auto-queue-monitor", bot:"notify"}')
  api_post_json "/api/discord/send" "$body"
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
  if [ -z "$dispatch_id" ] || [ "$dispatch_id" = "null" ]; then
    echo ""
    return
  fi
  api_get "/api/dispatches/$dispatch_id" \
    | jq -r '.dispatch.status // .status // ""' 2>/dev/null || true
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
      local entry_id review_round key alert recovery session_statuses
      issue=$(printf '%s' "$entry" | jq -r '.github_issue_number // "unknown"')
      entry_id=$(printf '%s' "$entry" | jq -r '.id // .entry_id // ("issue-" + ((.github_issue_number // "unknown") | tostring))')
      card_status=$(printf '%s' "$entry" | jq -r '.card_status // ""')
      q_status=$(printf '%s' "$entry" | jq -r '.status // ""')
      ref_ms=$(printf '%s' "$entry" | jq -r '.dispatched_at // .created_at // 0')
      age_min=$(entry_age_min "$ref_ms" "$now_ms")
      dispatch_id=$(printf '%s' "$entry" | jq -r '.dispatch_history[-1] // ""')
      dispatch_status=$(dispatch_status_for_entry "$dispatch_id")

      if [ "$sessions_available" = "true" ] \
        && [ "$q_status" = "dispatched" ] \
        && [ "$dispatch_status" = "dispatched" ] \
        && [ "${age_min:-0}" -gt "$STUCK_THRESHOLD_MIN" ]; then
        session_statuses=$(session_statuses_for_dispatch "$dispatch_id" "$sessions_json")
        case ",$session_statuses," in
          *,working,*|*,running,*|*,active,*) ;;
          *)
            key="STUCK|${run_id}|${entry_id}|${dispatch_id}"
            alert="[auto-queue monitor] STUCK: #${issue} dispatched ${age_min}min, no active session"
            recovery="[auto-queue monitor] RECOVERED: STUCK #${issue} (${entry_id})"
            append_condition "STUCK" "$key" "$alert" "$recovery"
            ;;
        esac
      fi

      if [ "$dispatch_status" = "completed" ] && [ "$q_status" = "dispatched" ]; then
        key="ANOMALY|${run_id}|${entry_id}|${dispatch_id}"
        alert="[auto-queue monitor] ANOMALY: #${issue} dispatch completed but entry not updated (card=${card_status})"
        recovery="[auto-queue monitor] RECOVERED: ANOMALY #${issue} (${entry_id})"
        append_condition "ANOMALY" "$key" "$alert" "$recovery"
      fi

      if [ "$card_status" = "review" ] && [ "${age_min:-0}" -gt "$REVIEW_THRESHOLD_MIN" ]; then
        review_round=$(printf '%s' "$entry" | jq -r '.review_round // 0')
        key="REVIEW_LONG|${run_id}|${entry_id}|round-${review_round}"
        alert="[auto-queue monitor] REVIEW_LONG: #${issue} review ${age_min}min elapsed (round=${review_round})"
        recovery="[auto-queue monitor] RECOVERED: REVIEW_LONG #${issue} round ${review_round} (${entry_id})"
        append_condition "REVIEW_LONG" "$key" "$alert" "$recovery"
      fi
    done
}

monitor_once() {
  local status_json run_status run_id sessions_json sessions_available now_epoch now_ms
  local temp_dir active_file actions_file action_file action kind message

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
  elif ! printf '%s' "$sessions_json" | jq -e 'type == "object" and ((.sessions // []) | type == "array")' >/dev/null 2>&1; then
    sessions_json='{"sessions":[]}'
    sessions_available=false
  fi

  now_epoch="${AQ_MONITOR_NOW_EPOCH:-$(date +%s)}"
  now_ms=$((now_epoch * 1000))
  temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-auto-queue-monitor.XXXXXX")
  active_file="$temp_dir/active.json"
  actions_file="$temp_dir/actions.jsonl"
  action_file="$temp_dir/action.json"
  CONDITIONS_JSONL="$temp_dir/conditions.jsonl"
  export CONDITIONS_JSONL
  : > "$CONDITIONS_JSONL"

  collect_active_conditions \
    "$status_json" "$run_status" "$run_id" "$sessions_json" \
    "$sessions_available" "$now_ms"
  jq -s '.' "$CONDITIONS_JSONL" > "$active_file"

  if ! "$PYTHON" "$STATE_HELPER" plan \
    --state-file "$STATE_FILE" \
    --active-file "$active_file" \
    --now "$now_epoch" \
    --cooldown-secs "$COOLDOWN_SECS" > "$actions_file"; then
    echo "auto-queue monitor: state reconciliation failed; preserving state" >&2
    rm -rf "$temp_dir"
    return 0
  fi

  while IFS= read -r action; do
    [ -n "$action" ] || continue
    printf '%s\n' "$action" > "$action_file"
    kind=$(printf '%s' "$action" | jq -r '.action')
    if [ "$kind" = "recovery" ]; then
      message=$(printf '%s' "$action" | jq -r '.condition.recovery')
    else
      message=$(printf '%s' "$action" | jq -r '.condition.alert')
    fi
    echo "$message"
    if notify_anomaly "$message"; then
      if ! "$PYTHON" "$STATE_HELPER" commit \
        --state-file "$STATE_FILE" --action-file "$action_file"; then
        echo "auto-queue monitor: notification sent but state commit lost CAS; will reconcile" >&2
      fi
    else
      echo "auto-queue monitor: notification failed; state not advanced" >&2
    fi
  done < "$actions_file"

  rm -rf "$temp_dir"
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

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
