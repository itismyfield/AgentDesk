#!/usr/bin/env bash
# Inline python heredocs use single-quoted apostrophes that confuse the
# quote tracker. The script is deprecated; PR 5 removes it after the
# adk-dashboard-e2e archive.
# shellcheck disable=SC1078,SC1079
set -euo pipefail

# Live Discord/TUI relay smoke suite for the dedicated E2E channels.
# It intentionally uses the shipped AgentDesk CLI and runtime tmux sessions
# instead of DB internals so the check follows the operator path.

AGENTDESK_BIN="${AGENTDESK_BIN:-$HOME/.adk/release/bin/agentdesk}"
SEND_SOURCE="${SEND_SOURCE:-project-agentdesk}"
SEND_BOT="${SEND_BOT:-announce}"
CLAUDE_CHANNEL="${CLAUDE_CHANNEL:-1506295332949196840}"
CODEX_CHANNEL="${CODEX_CHANNEL:-1506295335096549406}"
CLAUDE_TMUX="${CLAUDE_TMUX:-AgentDesk-claude-adk-dash-cc-e2e}"
CODEX_TMUX="${CODEX_TMUX:-AgentDesk-codex-adk-dash-cdx-e2e}"
TIMEOUT_SECS="${TIMEOUT_SECS:-180}"

run_id="$(date +%Y%m%d-%H%M%S)"

if [ "${1:-}" = "--help" ]; then
  echo "Usage: $0"
  exit 0
fi

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required command: $1" >&2
    exit 2
  }
}

send_turn() {
  local channel="$1"
  local label="$2"
  local instruction="${3:-진행 확인. 한 줄로 marker를 그대로 응답하고, 현재 작업 디렉토리 basename도 함께 알려줘.}"
  local marker="TUI-E2E-${run_id}-${label}"
  "$AGENTDESK_BIN" send \
    --target "channel:$channel" \
    --source "$SEND_SOURCE" \
    --bot "$SEND_BOT" \
    --content "$marker $instruction" >&2
  echo "$marker"
}

send_control() {
  local channel="$1"
  local content="$2"
  "$AGENTDESK_BIN" send \
    --target "channel:$channel" \
    --bot "$SEND_BOT" \
    --content "$content" >&2
}

latest_message_id() {
  local channel="$1"
  "$AGENTDESK_BIN" discord read "$channel" --limit 1 \
    | python3 -c 'import json,sys; msgs=json.load(sys.stdin).get("messages",[]); print(msgs[0]["id"] if msgs else "0")'
}

messages_after() {
  local channel="$1"
  local _after_id="$2"
  "$AGENTDESK_BIN" discord read "$channel" --limit 30
}

diag_status() {
  local channel="$1"
  "$AGENTDESK_BIN" diag "$channel" --json \
    | python3 -c 'import json,sys; print(json.load(sys.stdin).get("status","unknown"))'
}

assert_tui_prompt_ready() {
  local channel="$1"
  "$AGENTDESK_BIN" diag "$channel" --json | python3 -c '
import json, sys
doc = json.load(sys.stdin)
readiness = doc.get("tui_prompt_readiness")
if not isinstance(readiness, dict):
    raise SystemExit(
        f"channel {doc.get('target')} diag has no tui_prompt_readiness; "
        "this TUI relay smoke is not exercising a tmux-hosted TUI path"
    )
if not readiness.get("ready_for_input"):
    tail = (readiness.get("pane_tail") or "").replace("\n", " | ")
    if len(tail) > 700:
        tail = tail[:700] + "..."
    raise SystemExit(
        "TUI prompt is not ready for follow-up input after idle: "
        f"kind={readiness.get('kind')} "
        f"prompt_marker_detected={readiness.get('prompt_marker_detected')} "
        f"prompt_draft_detected={readiness.get('prompt_draft_detected')} "
        f"tmux_pane_alive={readiness.get('tmux_pane_alive')} "
        f"capture_available={readiness.get('capture_available')} "
        f"pane_tail={tail}"
    )
'
}

wait_channel_idle() {
  local channel="$1"
  local deadline=$((SECONDS + 90))
  while [ "$SECONDS" -lt "$deadline" ]; do
    [ "$(diag_status "$channel")" = "idle" ] && return 0
    sleep 2
  done
  echo "timeout waiting for channel $channel to become idle" >&2
  "$AGENTDESK_BIN" diag "$channel" --json >&2 || true
  return 1
}

assert_channel_stable_idle() {
  local channel="$1"
  wait_channel_idle "$channel"
  assert_tui_prompt_ready "$channel"
  sleep 10
  if [ "$(diag_status "$channel")" != "idle" ]; then
    echo "channel $channel re-entered non-idle state after relay settle window" >&2
    "$AGENTDESK_BIN" diag "$channel" --json >&2 || true
    return 1
  fi
  assert_tui_prompt_ready "$channel"
}

message_probe() {
  local channel="$1"
  local after_id="$2"
  local marker="$3"
  local mode="$4"
  local sent_prompt="$5"
  local expected_extra="${6:-}"
  messages_after "$channel" "$after_id" | AFTER_ID="$after_id" MARKER="$marker" MODE="$mode" SENT_PROMPT="$sent_prompt" EXPECTED_EXTRA="$expected_extra" python3 -c '
import json, os, re, sys
marker, mode = os.environ["MARKER"], os.environ["MODE"]
sent_prompt = os.environ["SENT_PROMPT"]
expected_extra = os.environ.get("EXPECTED_EXTRA", "")
after_id = int(os.environ.get("AFTER_ID") or "0")
doc = json.load(sys.stdin)
messages = [m for m in doc.get("messages", []) if int(m.get("id") or "0") > after_id]

def bot(m):
    return bool(m.get("author", {}).get("bot"))

def content(m):
    return m.get("content") or ""

def processing_message(text):
    return bool(
        re.search(r"^[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]\s+Processing\.\.\.$", text)
        or re.search(r"진행 중\s+—\s+(claude|codex|qwen)", text, re.IGNORECASE)
        or re.search(r"도구 실행 중\s*\(", text)
    )

def relay_response_candidate(m):
    text = content(m)
    if not bot(m):
        return False
    if text == sent_prompt:
        return False
    if text.startswith("터미널에 직접 주입된 입력"):
        return False
    if text in {"Session cleared.", "🧹 세션 클리어 (!clear)"}:
        return False
    if text.startswith("✅ **응답 완료**"):
        return False
    return True

if mode == "processing":
    ok = any(bot(m) and processing_message(content(m)) for m in messages)
elif mode == "response":
    ok = any(
        relay_response_candidate(m)
        and marker in content(m)
        and (not expected_extra or expected_extra in content(m))
        for m in messages
    )
elif mode == "stale_processing":
    ok = any(bot(m) and processing_message(content(m)) for m in messages)
else:
    raise SystemExit(f"unknown probe mode: {mode}")
raise SystemExit(0 if ok else 1)
'
}

wait_clear_evidence() {
  local channel="$1"
  local before_id="$2"
  local deadline=$((SECONDS + 60))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if messages_after "$channel" "$before_id" | AFTER_ID="$before_id" python3 -c '
import json, os, sys
after_id = int(os.environ.get("AFTER_ID") or "0")
messages = [m for m in json.load(sys.stdin).get("messages", []) if int(m.get("id") or "0") > after_id]
ok = any((m.get("content") or "") in {"Session cleared.", "🧹 세션 클리어 (!clear)"} for m in messages)
raise SystemExit(0 if ok else 1)
'; then
      [ "$(diag_status "$channel")" = "idle" ] && return 0
    fi
    sleep 2
  done
  echo "timeout waiting for !clear evidence on channel $channel" >&2
  return 1
}

wait_relay_evidence() {
  local channel="$1"
  local marker="$2"
  local before_id="$3"
  local sent_prompt="$4"
  local expected_extra="${5:-}"
  local require_persistent_processing="${6:-0}"
  local saw_busy=0
  local saw_processing=0
  local saw_response=0
  local busy_processing_misses=0

  local deadline=$((SECONDS + TIMEOUT_SECS))
  while [ "$SECONDS" -lt "$deadline" ]; do
    local status
    status="$(diag_status "$channel")"
    local processing_visible=0
    if message_probe "$channel" "$before_id" "$marker" processing "$sent_prompt" "$expected_extra"; then
      processing_visible=1
      saw_processing=1
    fi

    if [ "$status" != "idle" ]; then
      saw_busy=1
      if [ "$require_persistent_processing" = "1" ] && [ "$processing_visible" -eq 0 ]; then
        busy_processing_misses=$((busy_processing_misses + 1))
        if [ "$busy_processing_misses" -ge 2 ]; then
          echo "processing placeholder disappeared while channel $channel was busy" >&2
          return 1
        fi
      else
        busy_processing_misses=0
      fi
    fi
    if message_probe "$channel" "$before_id" "$marker" response "$sent_prompt" "$expected_extra"; then
      saw_response=1
    fi
    if [ "$saw_response" -eq 1 ] && [ "$status" = "idle" ]; then
      if [ "$require_persistent_processing" = "1" ] && [ "$saw_busy" -eq 1 ] && [ "$saw_processing" -ne 1 ]; then
        sleep 3
        continue
      fi
      if message_probe "$channel" "$before_id" "$marker" stale_processing "$sent_prompt" "$expected_extra"; then
        # Discord REST can briefly return the pre-edit status panel right after
        # the watcher commits idle. Treat it as stale only if it survives settle.
        sleep 3
        if message_probe "$channel" "$before_id" "$marker" stale_processing "$sent_prompt" "$expected_extra"; then
          echo "stale processing placeholder remained after response in channel $channel" >&2
          return 1
        fi
      fi
      return 0
    fi
    sleep 3
  done
  echo "timeout waiting for relay evidence on channel $channel (busy=$saw_busy processing=$saw_processing response=$saw_response)" >&2
  return 1
}

wait_tmux_contains() {
  local session="$1"
  local marker="$2"
  local deadline=$((SECONDS + TIMEOUT_SECS))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if tmux capture-pane -p -J -S -1000 -t "$session" 2>/dev/null | grep -q "$marker"; then
      return 0
    fi
    sleep 2
  done
  echo "timeout waiting for marker in tmux session $session: $marker" >&2
  return 1
}

assert_tmux_alive() {
  local session="$1"
  tmux has-session -t "$session" 2>/dev/null || {
    echo "tmux session not alive: $session" >&2
    return 1
  }
}

prepare_tmux_input() {
  local session="$1"
  if tmux has-session -t "$session" 2>/dev/null; then
    tmux send-keys -t "$session" C-u >/dev/null 2>&1 || true
    sleep 1
  fi
}

clear_channel_session() {
  local channel="$1"
  local session="$2"
  local before
  before="$(latest_message_id "$channel")"
  send_control "$channel" "!clear"
  wait_clear_evidence "$channel" "$before"
  if tmux has-session -t "$session" 2>/dev/null; then
    prepare_tmux_input "$session"
  fi
  assert_tui_prompt_ready "$channel"
}

assert_no_processing_tail() {
  local session="$1"
  local tail
  tail="$(tmux capture-pane -p -J -S -40 -t "$session" 2>/dev/null || true)"
  if printf '%s\n' "$tail" | grep -Eiq 'processing placeholder|No response requested'; then
    echo "unexpected relay noise in tmux tail for $session" >&2
    return 1
  fi
}

require_cmd tmux
require_cmd python3
[ -x "$AGENTDESK_BIN" ] || {
  echo "agentdesk binary not executable: $AGENTDESK_BIN" >&2
  exit 2
}

run_turn_scenario() {
  local channel="$1"
  local session="$2"
  local label="$3"
  local instruction="$4"
  local expected_extra="${5:-}"
  local require_persistent_processing="${6:-0}"
  local before marker sent_prompt
  prepare_tmux_input "$session"
  before="$(latest_message_id "$channel")"
  marker="TUI-E2E-${run_id}-${label}"
  sent_prompt="$marker $instruction"
  marker="$(send_turn "$channel" "$label" "$instruction")"
  wait_tmux_contains "$session" "$marker"
  wait_relay_evidence "$channel" "$marker" "$before" "$sent_prompt" "$expected_extra" "$require_persistent_processing"
  assert_tui_prompt_ready "$channel"
  printf '%s\n' "$marker"
}

# E-4 regression guard: inject a prompt directly into the TUI pane via
# tmux send-keys (bypassing the Discord message path) and verify the response
# still relays to Discord. This exercises the SSH-direct anchor path that
# `should_suppress_post_terminal_output_without_inflight` must respect.
#
# Mirrors the production single-line input path used by `tui_send_text` in
# both claude_tui/input.rs and codex_tui/input.rs: `send-keys -l -- <text>`
# followed by a settle delay and Enter. We intentionally avoid paste-buffer
# here — production only switches to paste-buffer for multi-line input and
# uses different flags (`paste-buffer -p -r`), so paste-buffer in tests
# would mask single-line regressions and add bracketed-paste timing risk.
run_direct_turn_scenario() {
  local channel="$1"
  local session="$2"
  local label="$3"
  local instruction="$4"
  local before marker sent_prompt
  if ! tmux has-session -t "$session" 2>/dev/null; then
    echo "tmux session missing for direct scenario: $session" >&2
    return 1
  fi
  prepare_tmux_input "$session"
  before="$(latest_message_id "$channel")"
  marker="TUI-E2E-${run_id}-${label}"
  sent_prompt="$marker $instruction"
  tmux send-keys -t "$session" -l -- "$sent_prompt"
  # Match the 200ms-after-paste settle pattern from PR #2730/#2731 — gives
  # the TUI line editor a tick to flush the input buffer before Enter.
  sleep 0.3
  tmux send-keys -t "$session" Enter
  wait_tmux_contains "$session" "$marker"
  wait_relay_evidence "$channel" "$marker" "$before" "$sent_prompt" ""
  assert_tui_prompt_ready "$channel"
  printf '%s\n' "$marker"
}

clear_channel_session "$CLAUDE_CHANNEL" "$CLAUDE_TMUX"
clear_channel_session "$CODEX_CHANNEL" "$CODEX_TMUX"

claude_marker="$(run_turn_scenario "$CLAUDE_CHANNEL" "$CLAUDE_TMUX" claude-basic "한 줄로 marker를 그대로 응답하고, 현재 작업 디렉토리 basename도 함께 알려줘.")"
codex_marker="$(run_turn_scenario "$CODEX_CHANNEL" "$CODEX_TMUX" codex-basic "한 줄로 marker를 그대로 응답하고, 현재 작업 디렉토리 basename도 함께 알려줘.")"

run_turn_scenario "$CLAUDE_CHANNEL" "$CLAUDE_TMUX" claude-memory "직전 응답에 나온 marker를 기억해서 그대로 포함하고, 이 새 marker도 함께 포함해 한 줄로 답해줘." "$claude_marker" >/dev/null
run_turn_scenario "$CODEX_CHANNEL" "$CODEX_TMUX" codex-memory "직전 응답에 나온 marker를 기억해서 그대로 포함하고, 이 새 marker도 함께 포함해 한 줄로 답해줘." "$codex_marker" >/dev/null

run_turn_scenario "$CLAUDE_CHANNEL" "$CLAUDE_TMUX" claude-long "도구를 사용하지 말고 marker를 첫 줄에 그대로 포함한 뒤, 1부터 120까지 각 줄을 'marker 항목 N' 형식으로 출력해줘." "" 1 >/dev/null
run_turn_scenario "$CODEX_CHANNEL" "$CODEX_TMUX" codex-long "도구를 사용하지 말고 marker를 첫 줄에 그대로 포함한 뒤, 1부터 120까지 각 줄을 'marker 항목 N' 형식으로 출력해줘." "" 1 >/dev/null

run_turn_scenario "$CLAUDE_CHANNEL" "$CLAUDE_TMUX" claude-rollover "marker를 그대로 포함하고, 1부터 180까지 번호 목록을 출력해 롤오버를 유도해줘." >/dev/null
run_turn_scenario "$CODEX_CHANNEL" "$CODEX_TMUX" codex-rollover "marker를 그대로 포함하고, 1부터 180까지 번호 목록을 출력해 롤오버를 유도해줘." >/dev/null

# Must run AFTER the Discord-side turns above so the watcher already has
# turn_result_relayed = true — i.e., this exercises the exact shape that the
# post-terminal suppress guard wrongly silenced before the anchor exemption.
run_direct_turn_scenario "$CLAUDE_CHANNEL" "$CLAUDE_TMUX" claude-ssh-direct "한 줄로 marker를 그대로 응답하고, 'ssh-direct' 단어도 포함해줘." >/dev/null
run_direct_turn_scenario "$CODEX_CHANNEL" "$CODEX_TMUX" codex-ssh-direct "한 줄로 marker를 그대로 응답하고, 'ssh-direct' 단어도 포함해줘." >/dev/null

assert_channel_stable_idle "$CLAUDE_CHANNEL"
assert_channel_stable_idle "$CODEX_CHANNEL"
assert_tmux_alive "$CLAUDE_TMUX"
assert_tmux_alive "$CODEX_TMUX"
assert_no_processing_tail "$CLAUDE_TMUX"
assert_no_processing_tail "$CODEX_TMUX"

echo "TUI relay E2E smoke completed:"
echo "  claude: $claude_marker"
echo "  codex:  $codex_marker"
