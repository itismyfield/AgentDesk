#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-auto-queue-monitor-test.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

mkdir -p "$TMP_ROOT/bin"
FAKE_MODE_FILE="$TMP_ROOT/mode"
FAKE_NOTIFY_LOG="$TMP_ROOT/notify.jsonl"
FAKE_DIRECT_LOG="$TMP_ROOT/direct.log"
STATE_FILE="$TMP_ROOT/state/monitor.json"
export FAKE_MODE_FILE FAKE_NOTIFY_LOG FAKE_DIRECT_LOG

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
      review-old-only)
        printf '%s\n' '{"run":{"id":"run-1","status":"active"},"entries":[{"id":"entry-review-old","github_issue_number":4450,"status":"pending","card_status":"review","review_round":3,"review_entered_at":1,"dispatch_history":[],"created_at":1}]}'
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
  */api/message-outbox/monitor-alerts)
    if [ "${FAKE_FAIL_POST:-0}" = "1" ]; then
      exit 22
    fi
    if [ -n "${FAKE_NOTIFY_DELAY:-}" ]; then
      sleep "$FAKE_NOTIFY_DELAY"
    fi
    action_id=$(printf '%s' "$body" | jq -r '.action_id')
    if [ -f "$FAKE_NOTIFY_LOG" ] \
      && jq -e --arg action_id "$action_id" \
        'select(.action_id == $action_id)' "$FAKE_NOTIFY_LOG" >/dev/null; then
      exit 0
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

cat > "$TMP_ROOT/bin/agentdesk" <<'FAKE_AGENTDESK'
#!/usr/bin/env bash
set -euo pipefail
if [ "${FAKE_DIRECT_FAIL:-0}" = "1" ]; then
  exit 9
fi
[ "${1:-}" = "discord-sendmessage" ] || exit 8
printf '%s\n' "$*" >> "$FAKE_DIRECT_LOG"
FAKE_AGENTDESK
chmod +x "$TMP_ROOT/bin/agentdesk"

run_once() {
  PATH="$TMP_ROOT/bin:$PATH" \
  AQ_MONITOR_ONCE=1 \
  AQ_MONITOR_NOW_EPOCH=1000 \
  AQ_MONITOR_COOLDOWN_SECS=1 \
  AQ_STUCK_THRESHOLD_MIN=1 \
  AQ_REVIEW_THRESHOLD_MIN=1 \
  AQ_MONITOR_STATE_FILE="$STATE_FILE" \
  AGENTDESK_BIN="$TMP_ROOT/bin/agentdesk" \
  AQ_MONITOR_NOTIFY_KEY="discord_test_notify" \
  PYTHON="${PYTHON:-python3}" \
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
FAKE_FAIL_POST=1 FAKE_DIRECT_FAIL=1 run_once
assert_notify_count 0
jq -e '.pending_action.action_id | test("^[0-9a-f]{32}$")' "$STATE_FILE" >/dev/null || {
  echo "failed enqueue must preserve a durable pending action ID" >&2
  exit 1
}

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
  all(.[];
    (.action_id | test("^[0-9a-f]{32}$"))
    and (.action == "alert" or .action == "recovery")) and
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

# A crash/failure after the durable endpoint accepts an action but before the
# state commit retries the same action ID. The endpoint's action-ID dedupe
# leaves one notification obligation, then the retry commits local state.
cat > "$TMP_ROOT/bin/python-commit-fail-once" <<'FAKE_PYTHON'
#!/usr/bin/env bash
set -euo pipefail
if [ "${2:-}" = "commit" ] && [ ! -f "$FAKE_COMMIT_MARKER" ]; then
  : > "$FAKE_COMMIT_MARKER"
  exit 3
fi
exec python3 "$@"
FAKE_PYTHON
chmod +x "$TMP_ROOT/bin/python-commit-fail-once"
rm -f "$STATE_FILE" "$STATE_FILE.lock" "$FAKE_NOTIFY_LOG"
export FAKE_COMMIT_MARKER="$TMP_ROOT/commit-failed-once"
rm -f "$FAKE_COMMIT_MARKER"
echo stuck-only > "$FAKE_MODE_FILE"
PYTHON="$TMP_ROOT/bin/python-commit-fail-once" run_once
assert_notify_count 1
first_action_id=$(jq -r '.action_id' "$FAKE_NOTIFY_LOG")
jq -e --arg action_id "$first_action_id" \
  '.pending_action.action_id == $action_id' "$STATE_FILE" >/dev/null
run_once
assert_notify_count 1
jq -e --arg action_id "$first_action_id" '
  .pending_action == null
  and .conditions["STUCK|run-1|entry-stuck|dispatch-2"].last_alert_at == 1000
' "$STATE_FILE" >/dev/null

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

# Actionable STUCK/ANOMALY alerts use the durable announce route first. When
# the API/PG path is down, a direct Discord post preserves human visibility and
# commits the same durable action ID. REVIEW_LONG remains informational and
# must not enter the direct fallback.
rm -f "$STATE_FILE" "$STATE_FILE.lock" "$FAKE_NOTIFY_LOG" "$FAKE_DIRECT_LOG"
echo stuck-only > "$FAKE_MODE_FILE"
FAKE_FAIL_POST=1 run_once
[ "$(wc -l < "$FAKE_DIRECT_LOG" | tr -d ' ')" -eq 1 ] || {
  echo "STUCK API failure must use exactly one direct fallback" >&2
  exit 1
}
grep -q 'discord-sendmessage.*STUCK' "$FAKE_DIRECT_LOG" || {
  echo "direct fallback must retain the actionable STUCK body" >&2
  exit 1
}
grep -q -- '--key discord_test_notify' "$FAKE_DIRECT_LOG" || {
  echo "direct fallback must pin the notify bot credential" >&2
  exit 1
}
jq -e '.pending_action == null' "$STATE_FILE" >/dev/null || {
  echo "successful direct fallback must commit the durable action" >&2
  exit 1
}

rm -f "$STATE_FILE" "$STATE_FILE.lock" "$FAKE_NOTIFY_LOG" "$FAKE_DIRECT_LOG"
echo review-old-only > "$FAKE_MODE_FILE"
FAKE_FAIL_POST=1 run_once
[ ! -e "$FAKE_DIRECT_LOG" ] || {
  echo "REVIEW_LONG must remain notify-only when durable enqueue fails" >&2
  exit 1
}
jq -e '.pending_action.condition.kind == "REVIEW_LONG"' "$STATE_FILE" >/dev/null || {
  echo "failed REVIEW_LONG enqueue must retain its pending action" >&2
  exit 1
}

echo "auto-queue monitor restart/cooldown/recovery behavior passed"
