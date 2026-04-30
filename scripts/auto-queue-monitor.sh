#!/usr/bin/env bash
# Auto-queue monitor script for Claude Code Monitor tool.
# Checks active queue entries every 30s, reports stuck/complete/anomalies.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/_defaults.sh
. "$SCRIPT_DIR/_defaults.sh"

REL_PORT="${AGENTDESK_REL_PORT:-$ADK_DEFAULT_PORT}"
API="http://${ADK_DEFAULT_LOOPBACK}:${REL_PORT}"
INTERVAL="${AQ_MONITOR_INTERVAL:-30}"
STUCK_THRESHOLD_MIN="${AQ_STUCK_THRESHOLD_MIN:-30}"
REVIEW_THRESHOLD_MIN="${AQ_REVIEW_THRESHOLD_MIN:-60}"
NOTIFY_CHANNEL="${AQ_MONITOR_CHANNEL:-1479671298497183835}"

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
  body=$(jq -n \
    --arg target "channel:$NOTIFY_CHANNEL" \
    --arg content "$msg" \
    '{target:$target, content:$content, source:"auto-queue-monitor", bot:"notify"}')
  api_post_json "/api/discord/send" "$body" || true
}

entry_age_min() {
  local ref_ms="$1"
  local now_ms
  now_ms=$(($(date +%s) * 1000))
  if [ -z "$ref_ms" ] || [ "$ref_ms" = "null" ] || [ "$ref_ms" -le 0 ] 2>/dev/null; then
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

while true; do
  STATUS_JSON=$(api_get "/api/auto-queue/status" 2>/dev/null || echo '{}')
  RUN_STATUS=$(printf '%s' "$STATUS_JSON" | jq -r '.run.status // ""' 2>/dev/null || true)
  if [ "$RUN_STATUS" != "active" ] && [ "$RUN_STATUS" != "pending" ] && [ "$RUN_STATUS" != "paused" ]; then
    sleep "$INTERVAL"
    continue
  fi

  SESSIONS_JSON=$(api_get "/api/sessions" 2>/dev/null || echo '{"sessions":[]}')
  printf '%s' "$STATUS_JSON" \
    | jq -c '.entries[]? | select(.status != "done" and .status != "skipped")' \
    | while IFS= read -r entry; do
      issue=$(printf '%s' "$entry" | jq -r '.github_issue_number // "unknown"')
      card_status=$(printf '%s' "$entry" | jq -r '.card_status // ""')
      q_status=$(printf '%s' "$entry" | jq -r '.status // ""')
      ref_ms=$(printf '%s' "$entry" | jq -r '.dispatched_at // .created_at // 0')
      age_min=$(entry_age_min "$ref_ms")
      dispatch_id=$(printf '%s' "$entry" | jq -r '.dispatch_history[-1] // ""')
      dispatch_status=$(dispatch_status_for_entry "$dispatch_id")

      if [ "$q_status" = "dispatched" ] \
        && [ "$dispatch_status" = "dispatched" ] \
        && [ "${age_min:-0}" -gt "$STUCK_THRESHOLD_MIN" ]; then
        session_statuses=$(session_statuses_for_dispatch "$dispatch_id" "$SESSIONS_JSON")
        case ",$session_statuses," in
          *,working,*|*,running,*|*,active,*) ;;
          *)
            echo "STUCK: #${issue} dispatched ${age_min}min, no active session (session=${session_statuses:-none})"
            notify_anomaly "[auto-queue monitor] STUCK: #${issue} dispatched ${age_min}min, no active session"
            ;;
        esac
      fi

      if [ "$dispatch_status" = "completed" ] && [ "$q_status" = "dispatched" ]; then
        echo "ANOMALY: #${issue} dispatch completed but queue entry still 'dispatched' (card=${card_status})"
        notify_anomaly "[auto-queue monitor] ANOMALY: #${issue} dispatch completed but entry not updated (card=${card_status})"
      fi

      if [ "$card_status" = "review" ] && [ "${age_min:-0}" -gt "$REVIEW_THRESHOLD_MIN" ]; then
        review_round=$(printf '%s' "$entry" | jq -r '.review_round // 0')
        echo "REVIEW_LONG: #${issue} review ${age_min}min elapsed (round=${review_round})"
        notify_anomaly "[auto-queue monitor] review long-running: #${issue} ${age_min}min elapsed"
      fi
    done

  sleep "$INTERVAL"
done
