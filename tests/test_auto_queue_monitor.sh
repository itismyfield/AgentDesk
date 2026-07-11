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
    case "$(cat "$FAKE_MODE_FILE")" in
      active)
        printf '%s\n' '{"run":{"id":"run-1","status":"active"},"entries":[{"id":"entry-anomaly","github_issue_number":4448,"status":"dispatched","card_status":"implementation","dispatch_history":["dispatch-1"],"created_at":1},{"id":"entry-stuck","github_issue_number":4449,"status":"dispatched","card_status":"implementation","dispatch_history":["dispatch-2"],"created_at":1},{"id":"entry-review","github_issue_number":4450,"status":"pending","card_status":"review","review_round":2,"review_entered_at":1,"dispatch_history":[],"created_at":1}]}'
        ;;
      active-review-clock-missing)
        printf '%s\n' '{"run":{"id":"run-1","status":"active"},"entries":[{"id":"entry-anomaly","github_issue_number":4448,"status":"dispatched","card_status":"implementation","dispatch_history":["dispatch-1"],"created_at":1},{"id":"entry-stuck","github_issue_number":4449,"status":"dispatched","card_status":"implementation","dispatch_history":["dispatch-2"],"created_at":1},{"id":"entry-review","github_issue_number":4450,"status":"pending","card_status":"review","review_round":2,"dispatch_history":[],"created_at":1}]}'
        ;;
      stuck-only)
        printf '%s\n' '{"run":{"id":"run-1","status":"active"},"entries":[{"id":"entry-stuck","github_issue_number":4449,"status":"dispatched","card_status":"implementation","dispatch_history":["dispatch-2"],"created_at":1}]}'
        ;;
      review-fresh-only)
        printf '%s\n' '{"run":{"id":"run-1","status":"active"},"entries":[{"id":"entry-review-fresh","github_issue_number":4450,"status":"pending","card_status":"review","review_round":3,"review_entered_at":999000,"dispatch_history":[],"created_at":1}]}'
        ;;
      review-missing-only)
        printf '%s\n' '{"run":{"id":"run-1","status":"active"},"entries":[{"id":"entry-review-missing","github_issue_number":4450,"status":"pending","card_status":"review","review_round":4,"dispatch_history":[],"created_at":1}]}'
        ;;
      *)
        printf '%s\n' '{"run":{"id":"run-1","status":"completed"},"entries":[]}'
        ;;
    esac
    ;;
  */api/sessions)
    if [ "${FAKE_SESSIONS_FAIL:-0}" = "1" ]; then
      exit 22
    fi
    if [ -n "${FAKE_SESSION_STATUS:-}" ]; then
      jq -n -c --arg status "$FAKE_SESSION_STATUS" \
        '{sessions:[{active_dispatch_id:"dispatch-2",status:$status}]}'
    else
      printf '%s\n' '{"sessions":[]}'
    fi
    ;;
  */api/dispatches/dispatch-1)
    if [ "${FAKE_DISPATCH_FAIL:-0}" = "1" ]; then
      exit 22
    fi
    printf '%s\n' '{"dispatch":{"status":"completed"}}'
    ;;
  */api/dispatches/dispatch-2)
    if [ "${FAKE_DISPATCH_FAIL:-0}" = "1" ]; then
      exit 22
    fi
    printf '%s\n' '{"dispatch":{"status":"dispatched"}}'
    ;;
  */api/discord/send)
    if [ "${FAKE_FAIL_POST:-0}" = "1" ]; then
      exit 22
    fi
    if [ -n "${FAKE_NOTIFY_DELAY:-}" ]; then
      sleep "$FAKE_NOTIFY_DELAY"
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

assert_notify_count() {
  local expected="$1"
  local actual
  actual=$(line_count)
  if [ "$actual" -ne "$expected" ]; then
    echo "expected $expected notification(s), got $actual" >&2
    exit 1
  fi
}

echo active > "$FAKE_MODE_FILE"
FAKE_FAIL_POST=1 run_once
assert_notify_count 0
if [ -f "$STATE_FILE" ]; then
  echo "failed notification must not advance persistent state" >&2
  exit 1
fi

run_once
assert_notify_count 3
jq -e '
  (.conditions | keys | sort) == [
    "ANOMALY|run-1|entry-anomaly|dispatch-1",
    "REVIEW_LONG|run-1|entry-review|round-2",
    "STUCK|run-1|entry-stuck|dispatch-2"
  ]
' "$STATE_FILE" >/dev/null || {
  echo "condition identity must include kind, run, entry, and retry stage" >&2
  exit 1
}
run_once
assert_notify_count 3

# Detector outages are UNKNOWN, not RECOVERED. Existing incidents remain
# durable until their owning API becomes observable again.
FAKE_SESSIONS_FAIL=1 run_once
assert_notify_count 3
FAKE_DISPATCH_FAIL=1 run_once
assert_notify_count 3
echo active-review-clock-missing > "$FAKE_MODE_FILE"
run_once
assert_notify_count 3
echo active > "$FAKE_MODE_FILE"

echo inactive > "$FAKE_MODE_FILE"
run_once
assert_notify_count 6
run_once
assert_notify_count 6

jq -s -e '
  all(.[]; .source == "auto-queue-monitor") and
  any(.[]; .content | contains("ANOMALY")) and
  any(.[]; .content | contains("STUCK")) and
  any(.[]; .content | contains("REVIEW_LONG")) and
  (map(select(.content | contains("RECOVERED"))) | length == 3)
' \
  "$FAKE_NOTIFY_LOG" >/dev/null
jq -e '.version == 1 and (.conditions | length == 0)' "$STATE_FILE" >/dev/null

# All production live-session states suppress STUCK classification.
echo stuck-only > "$FAKE_MODE_FILE"
for status in working turn_active awaiting_bg awaiting_user; do
  FAKE_SESSION_STATUS="$status" run_once
  assert_notify_count 6
done

# An old queue entry that entered review one second ago is not REVIEW_LONG.
# The monitor must never fall back to created_at/dispatched_at for this rule.
echo review-fresh-only > "$FAKE_MODE_FILE"
run_once
assert_notify_count 6
echo review-missing-only > "$FAKE_MODE_FILE"
run_once
assert_notify_count 6

# The state lock spans detection, delivery, and commit. Two processes racing
# from an empty state still deliver one alert per condition, not two.
rm -f "$STATE_FILE" "$STATE_FILE.lock" "$FAKE_NOTIFY_LOG"
echo active > "$FAKE_MODE_FILE"
FAKE_NOTIFY_DELAY=0.2 run_once &
first_pid=$!
FAKE_NOTIFY_DELAY=0.2 run_once &
second_pid=$!
wait "$first_pid"
wait "$second_pid"
assert_notify_count 3

echo "auto-queue monitor restart/cooldown/recovery behavior passed"
