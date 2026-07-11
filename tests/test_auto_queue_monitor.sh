#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-auto-queue-monitor-test.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

mkdir -p "$TMP_ROOT/bin"
FAKE_MODE_FILE="$TMP_ROOT/mode"
FAKE_NOTIFY_LOG="$TMP_ROOT/notify.jsonl"
STATE_FILE="$TMP_ROOT/state/monitor.json"
export FAKE_MODE_FILE FAKE_NOTIFY_LOG

cat > "$TMP_ROOT/bin/curl" <<'FAKE_CURL'
#!/usr/bin/env bash
set -euo pipefail

url=""
body=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -d)
      body="$2"
      shift 2
      ;;
    http://*|https://*)
      url="$1"
      shift
      ;;
    *) shift ;;
  esac
done

case "$url" in
  */api/queue/status)
    if [ "$(cat "$FAKE_MODE_FILE")" = "active" ]; then
      printf '%s\n' '{"run":{"id":"run-1","status":"active"},"entries":[{"id":"entry-anomaly","github_issue_number":4448,"status":"dispatched","card_status":"implementation","dispatch_history":["dispatch-1"],"created_at":1},{"id":"entry-stuck","github_issue_number":4449,"status":"dispatched","card_status":"implementation","dispatch_history":["dispatch-2"],"created_at":1},{"id":"entry-review","github_issue_number":4450,"status":"pending","card_status":"review","review_round":2,"dispatch_history":[],"created_at":1}]}'
    else
      printf '%s\n' '{"run":{"id":"run-1","status":"completed"},"entries":[]}'
    fi
    ;;
  */api/sessions)
    printf '%s\n' '{"sessions":[]}'
    ;;
  */api/dispatches/dispatch-1)
    printf '%s\n' '{"dispatch":{"status":"completed"}}'
    ;;
  */api/dispatches/dispatch-2)
    printf '%s\n' '{"dispatch":{"status":"dispatched"}}'
    ;;
  */api/discord/send)
    if [ "${FAKE_FAIL_POST:-0}" = "1" ]; then
      exit 22
    fi
    printf '%s\n' "$body" >> "$FAKE_NOTIFY_LOG"
    ;;
  *)
    echo "unexpected fake curl URL: $url" >&2
    exit 64
    ;;
esac
FAKE_CURL
chmod +x "$TMP_ROOT/bin/curl"

run_once() {
  PATH="$TMP_ROOT/bin:$PATH" \
  AQ_MONITOR_ONCE=1 \
  AQ_MONITOR_NOW_EPOCH=1000 \
  AQ_MONITOR_COOLDOWN_SECS=1 \
  AQ_STUCK_THRESHOLD_MIN=1 \
  AQ_REVIEW_THRESHOLD_MIN=1 \
  AQ_MONITOR_STATE_FILE="$STATE_FILE" \
  bash "$ROOT/scripts/auto-queue-monitor.sh" >/dev/null
}

line_count() {
  if [ -f "$FAKE_NOTIFY_LOG" ]; then
    wc -l < "$FAKE_NOTIFY_LOG" | tr -d ' '
  else
    echo 0
  fi
}

echo active > "$FAKE_MODE_FILE"
FAKE_FAIL_POST=1 run_once
[ "$(line_count)" -eq 0 ]
[ ! -f "$STATE_FILE" ]

run_once
[ "$(line_count)" -eq 3 ]
jq -e '
  (.conditions | keys | sort) == [
    "ANOMALY|run-1|entry-anomaly|dispatch-1",
    "REVIEW_LONG|run-1|entry-review|round-2",
    "STUCK|run-1|entry-stuck|dispatch-2"
  ]
' "$STATE_FILE" >/dev/null
run_once
[ "$(line_count)" -eq 3 ]

echo inactive > "$FAKE_MODE_FILE"
run_once
[ "$(line_count)" -eq 6 ]
run_once
[ "$(line_count)" -eq 6 ]

jq -s -e '
  all(.[]; .source == "auto-queue-monitor") and
  any(.[]; .content | contains("ANOMALY")) and
  any(.[]; .content | contains("STUCK")) and
  any(.[]; .content | contains("REVIEW_LONG")) and
  (map(select(.content | contains("RECOVERED"))) | length == 3)
' \
  "$FAKE_NOTIFY_LOG" >/dev/null
jq -e '.version == 1 and (.conditions | length == 0)' "$STATE_FILE" >/dev/null

echo "auto-queue monitor restart/cooldown/recovery behavior passed"
