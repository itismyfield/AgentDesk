#!/usr/bin/env bash
set -euo pipefail

# Legacy tmux-wrapper smoke suite.
#
# This exercises the shipped AgentDesk wrapper subprocesses inside real tmux
# sessions with --input-mode fifo. It does not cover ProcessBackend/pipe stdin
# and does not touch live Discord channels.

AGENTDESK_BIN="${AGENTDESK_BIN:-$HOME/.adk/release/bin/agentdesk}"
TIMEOUT_SECS="${TIMEOUT_SECS:-30}"

run_id="$(date +%Y%m%d-%H%M%S)"

if [ "${1:-}" = "--help" ]; then
  echo "Usage: $0"
  echo
  echo "Environment:"
  echo "  AGENTDESK_BIN   AgentDesk binary to test (default: ~/.adk/release/bin/agentdesk)"
  echo "  TIMEOUT_SECS    Per-probe timeout (default: 30)"
  exit 0
fi

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required command: $1" >&2
    exit 2
  }
}

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-tmux-wrapper-smoke.XXXXXX")"
tmux_sessions=""

cleanup() {
  set +e
  for session in $tmux_sessions; do
    tmux kill-session -t "$session" >/dev/null 2>&1 || true
  done
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

require_cmd python3
require_cmd tmux
[ -x "$AGENTDESK_BIN" ] || {
  echo "agentdesk binary not executable: $AGENTDESK_BIN" >&2
  exit 2
}

fake_claude="$tmp_dir/fake-claude.py"
fake_codex="$tmp_dir/fake-codex.py"

cat >"$fake_claude" <<'PY'
#!/usr/bin/env python3
import json
import os
import re
import sys

previous_marker = ""

def content_text(value):
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        parts = []
        for item in value:
            if isinstance(item, dict):
                text = item.get("text") or item.get("content")
                if isinstance(text, str):
                    parts.append(text)
            elif isinstance(item, str):
                parts.append(item)
        return "\n".join(parts)
    return ""

def marker_from(prompt):
    match = re.search(r"TMUX-WRAPPER-SMOKE-[A-Za-z0-9_.:-]+", prompt)
    return match.group(0) if match else "TMUX-WRAPPER-SMOKE-missing-marker"

def response_for(prompt):
    global previous_marker
    marker = marker_from(prompt)
    cwd_name = os.path.basename(os.getcwd()) or os.getcwd()
    if "long" in marker:
        text = marker + "\n" + "\n".join(f"{marker} item {i}" for i in range(1, 121))
    elif "unicode" in marker:
        text = f"{marker} 유니코드-정상"
    elif "memory" in marker and previous_marker:
        text = f"{previous_marker} {marker}"
    else:
        text = f"{marker} {cwd_name}"
    previous_marker = marker
    return text

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        event = json.loads(line)
    except json.JSONDecodeError:
        prompt = line
    else:
        prompt = content_text(event.get("message", {}).get("content", ""))
    marker = marker_from(prompt)
    if "tool" in marker:
        print(json.dumps({
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use",
                "id": "tmux-wrapper-smoke-tool",
                "name": "Bash",
                "input": {"command": "printf claude-tool-ok"},
            }]},
        }, ensure_ascii=False), flush=True)
        print(json.dumps({
            "type": "user",
            "message": {"content": [{
                "type": "tool_result",
                "tool_use_id": "tmux-wrapper-smoke-tool",
                "content": f"{marker} tool-result-ok",
            }]},
        }, ensure_ascii=False), flush=True)
        text = f"{marker} tool-ok"
    elif "error" in marker:
        previous_marker = marker
        print(json.dumps({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": True,
            "errors": [f"{marker} simulated-error"],
            "result": f"{marker} simulated-error",
            "session_id": "tmux-wrapper-smoke-claude",
            "duration_ms": 1,
            "total_cost_usd": 0.01,
        }, ensure_ascii=False), flush=True)
        continue
    else:
        text = response_for(prompt)
    print(json.dumps({
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": text}]},
    }, ensure_ascii=False), flush=True)
    print(json.dumps({
        "type": "result",
        "subtype": "success",
        "result": text,
        "session_id": "tmux-wrapper-smoke-claude",
        "duration_ms": 1,
        "total_cost_usd": 0.0,
    }, ensure_ascii=False), flush=True)
PY

cat >"$fake_codex" <<'PY'
#!/usr/bin/env python3
import json
import os
import re
import sys

state_file = os.environ.get("TMUX_WRAPPER_SMOKE_CODEX_STATE")

def read_previous():
    if not state_file:
        return ""
    try:
        with open(state_file, "r", encoding="utf-8") as handle:
            return handle.read().strip()
    except FileNotFoundError:
        return ""

def write_previous(marker):
    if not state_file:
        return
    with open(state_file, "w", encoding="utf-8") as handle:
        handle.write(marker)

def prompt_from_args(argv):
    if "--" in argv:
        idx = len(argv) - 1 - argv[::-1].index("--")
        if idx + 1 < len(argv):
            return argv[idx + 1]
    return argv[-1] if argv else ""

def marker_from(prompt):
    match = re.search(r"TMUX-WRAPPER-SMOKE-[A-Za-z0-9_.:-]+", prompt)
    return match.group(0) if match else "TMUX-WRAPPER-SMOKE-missing-marker"

prompt = prompt_from_args(sys.argv[1:])
marker = marker_from(prompt)
previous_marker = read_previous()
cwd_name = os.path.basename(os.getcwd()) or os.getcwd()

if "long" in marker:
    text = marker + "\n" + "\n".join(f"{marker} item {i}" for i in range(1, 121))
elif "unicode" in marker:
    text = f"{marker} 유니코드-정상"
elif "memory" in marker and previous_marker:
    text = f"{previous_marker} {marker}"
elif "multiline" in marker:
    text = f"{marker} multiline-ok"
elif "background" in marker:
    text = f"{marker} background-ok"
elif "tool" in marker:
    text = f"{marker} tool-ok"
else:
    text = f"{marker} {cwd_name}"

write_previous(marker)

print(json.dumps({"type": "thread.started", "thread_id": "tmux-wrapper-smoke-codex"}, ensure_ascii=False), flush=True)
if "tool" in marker:
    print(json.dumps({
        "type": "item.started",
        "item": {
            "type": "command_execution",
            "command": "printf codex-tool-ok",
        },
    }, ensure_ascii=False), flush=True)
    print(json.dumps({
        "type": "item.completed",
        "item": {
            "type": "command_execution",
            "aggregated_output": f"{marker} tool-result-ok",
            "exit_code": 0,
        },
    }, ensure_ascii=False), flush=True)
elif "background" in marker:
    print(json.dumps({
        "type": "background_event",
        "message": f"{marker} background-ok",
    }, ensure_ascii=False), flush=True)
elif "error" in marker:
    print(json.dumps({
        "type": "error",
        "message": f"{marker} simulated-error",
    }, ensure_ascii=False), flush=True)
    sys.exit(0)
print(json.dumps({
    "type": "item.completed",
    "item": {"type": "agent_message", "text": text},
}, ensure_ascii=False), flush=True)
print(json.dumps({
    "type": "turn.completed",
    "usage": {"input_tokens": 1, "output_tokens": 1},
}, ensure_ascii=False), flush=True)
PY

chmod +x "$fake_claude" "$fake_codex"

json_user_line() {
  PROMPT="$1" python3 - <<'PY'
import json
import os
print(json.dumps({
    "type": "user",
    "message": {"role": "user", "content": os.environ["PROMPT"]},
}, ensure_ascii=False))
PY
}

b64_prompt_line() {
  PROMPT="$1" python3 - <<'PY'
import base64
import os
print("__AGENTDESK_B64__:" + base64.b64encode(os.environ["PROMPT"].encode()).decode())
PY
}

ready_count() {
  local output_file="$1"
  local provider="$2"
  OUTPUT_FILE="$output_file" PROVIDER="$provider" python3 - <<'PY'
import json
import os

path = os.environ["OUTPUT_FILE"]
provider = os.environ["PROVIDER"]
count = 0
try:
    with open(path, "r", encoding="utf-8") as handle:
        for line in handle:
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if event.get("type") == "ready_for_input" and event.get("provider") == provider:
                count += 1
except FileNotFoundError:
    pass
print(count)
PY
}

wait_file_contains() {
  local output_file="$1"
  local marker="$2"
  local expected_extra="${3:-}"
  local deadline=$((SECONDS + TIMEOUT_SECS))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if [ -f "$output_file" ] && grep -Fq "$marker" "$output_file"; then
      if [ -z "$expected_extra" ] || grep -Fq "$expected_extra" "$output_file"; then
        return 0
      fi
    fi
    sleep 1
  done
  echo "timeout waiting for output marker: $marker" >&2
  [ -f "$output_file" ] && tail -40 "$output_file" >&2 || true
  return 1
}

wait_file_text() {
  local output_file="$1"
  local expected_text="$2"
  local description="${3:-$expected_text}"
  local deadline=$((SECONDS + TIMEOUT_SECS))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if [ -f "$output_file" ] && grep -Fq "$expected_text" "$output_file"; then
      return 0
    fi
    sleep 1
  done
  echo "timeout waiting for output text: $description" >&2
  [ -f "$output_file" ] && tail -40 "$output_file" >&2 || true
  return 1
}

wait_ready_increment() {
  local output_file="$1"
  local provider="$2"
  local before_count="$3"
  local deadline=$((SECONDS + TIMEOUT_SECS))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if [ "$(ready_count "$output_file" "$provider")" -gt "$before_count" ]; then
      return 0
    fi
    sleep 1
  done
  echo "timeout waiting for $provider ready_for_input sentinel" >&2
  [ -f "$output_file" ] && tail -40 "$output_file" >&2 || true
  return 1
}

wait_tmux_contains() {
  local session="$1"
  local marker="$2"
  local deadline=$((SECONDS + TIMEOUT_SECS))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if tmux capture-pane -p -J -S -4000 -t "$session" 2>/dev/null | grep -Fq "$marker"; then
      return 0
    fi
    sleep 1
  done
  echo "timeout waiting for tmux pane marker: $marker" >&2
  tmux capture-pane -p -J -S -80 -t "$session" >&2 2>/dev/null || true
  return 1
}

write_fifo_line() {
  local fifo="$1"
  local line="$2"
  printf '%s\n' "$line" >"$fifo"
}

start_tmux_wrapper() {
  local provider="$1"
  local session="$2"
  local output_file="$3"
  local input_fifo="$4"
  local prompt_file="$5"
  local runner="$tmp_dir/$provider-runner.sh"

  rm -f "$input_fifo"
  mkfifo "$input_fifo"
  case "$provider" in
    claude)
      cat >"$runner" <<EOF
#!/usr/bin/env bash
set -euo pipefail
exec "$AGENTDESK_BIN" tmux-wrapper \\
  --output-file "$output_file" \\
  --input-fifo "$input_fifo" \\
  --prompt-file "$prompt_file" \\
  --cwd "$PWD" \\
  --input-mode fifo \\
  -- "$fake_claude" --input-format stream-json
EOF
      ;;
    codex)
      cat >"$runner" <<EOF
#!/usr/bin/env bash
set -euo pipefail
export TMUX_WRAPPER_SMOKE_CODEX_STATE="$tmp_dir/codex-state.txt"
exec "$AGENTDESK_BIN" codex-tmux-wrapper \\
  --output-file "$output_file" \\
  --input-fifo "$input_fifo" \\
  --prompt-file "$prompt_file" \\
  --codex-bin "$fake_codex" \\
  --cwd "$PWD" \\
  --input-mode fifo
EOF
      ;;
    *)
      echo "unknown provider: $provider" >&2
      return 2
      ;;
  esac
  chmod +x "$runner"
  tmux new-session -d -s "$session" -c "$PWD" "$runner"
  tmux_sessions="$tmux_sessions $session"
}

run_claude_scenarios() {
  local session="ADKSmoke-claude-wrapper-${run_id}"
  local output_file="$tmp_dir/claude.jsonl"
  local input_fifo="$tmp_dir/claude.input.fifo"
  local prompt_file="$tmp_dir/claude.prompt"
  local basic_marker="TMUX-WRAPPER-SMOKE-${run_id}-claude-basic"
  local memory_marker="TMUX-WRAPPER-SMOKE-${run_id}-claude-memory"
  local unicode_marker="TMUX-WRAPPER-SMOKE-${run_id}-claude-unicode"
  local tool_marker="TMUX-WRAPPER-SMOKE-${run_id}-claude-tool"
  local error_marker="TMUX-WRAPPER-SMOKE-${run_id}-claude-error"
  local long_marker="TMUX-WRAPPER-SMOKE-${run_id}-claude-long"
  local before_ready

  printf '%s\n' "$basic_marker 한 줄로 marker와 cwd basename을 응답해줘." >"$prompt_file"
  start_tmux_wrapper claude "$session" "$output_file" "$input_fifo" "$prompt_file"
  wait_file_contains "$output_file" "$basic_marker"
  wait_tmux_contains "$session" "$basic_marker"
  wait_ready_increment "$output_file" claude 0

  before_ready="$(ready_count "$output_file" claude)"
  write_fifo_line "$input_fifo" "$(json_user_line "$memory_marker 직전 marker도 함께 한 줄로 응답해줘.")"
  wait_file_contains "$output_file" "$memory_marker" "$basic_marker"
  wait_ready_increment "$output_file" claude "$before_ready"

  before_ready="$(ready_count "$output_file" claude)"
  write_fifo_line "$input_fifo" "$(json_user_line "$unicode_marker 한글/유니코드가 깨지지 않는지 응답해줘.")"
  wait_file_contains "$output_file" "$unicode_marker" "유니코드-정상"
  wait_ready_increment "$output_file" claude "$before_ready"

  before_ready="$(ready_count "$output_file" claude)"
  write_fifo_line "$input_fifo" "$(json_user_line "$tool_marker tool_use와 tool_result 이벤트를 흘려줘.")"
  wait_file_contains "$output_file" "$tool_marker" "tool-result-ok"
  wait_file_text "$output_file" "\"tool_use\"" "claude tool_use event"
  wait_file_text "$output_file" "\"tool_result\"" "claude tool_result event"
  wait_ready_increment "$output_file" claude "$before_ready"

  before_ready="$(ready_count "$output_file" claude)"
  write_fifo_line "$input_fifo" "$(json_user_line "$error_marker 오류 result를 흘려줘.")"
  wait_file_contains "$output_file" "$error_marker" "simulated-error"
  wait_file_text "$output_file" "error_during_execution" "claude error result"
  wait_ready_increment "$output_file" claude "$before_ready"

  before_ready="$(ready_count "$output_file" claude)"
  write_fifo_line "$input_fifo" "$(json_user_line "$long_marker marker를 첫 줄에 포함하고 120줄을 출력해줘.")"
  wait_file_contains "$output_file" "$long_marker item 120"
  wait_tmux_contains "$session" "$long_marker"
  wait_ready_increment "$output_file" claude "$before_ready"

  echo "  claude: $basic_marker"
}

run_codex_scenarios() {
  local session="ADKSmoke-codex-wrapper-${run_id}"
  local output_file="$tmp_dir/codex.jsonl"
  local input_fifo="$tmp_dir/codex.input.fifo"
  local prompt_file="$tmp_dir/codex.prompt"
  local basic_marker="TMUX-WRAPPER-SMOKE-${run_id}-codex-basic"
  local memory_marker="TMUX-WRAPPER-SMOKE-${run_id}-codex-memory"
  local multiline_marker="TMUX-WRAPPER-SMOKE-${run_id}-codex-multiline"
  local unicode_marker="TMUX-WRAPPER-SMOKE-${run_id}-codex-unicode"
  local tool_marker="TMUX-WRAPPER-SMOKE-${run_id}-codex-tool"
  local background_marker="TMUX-WRAPPER-SMOKE-${run_id}-codex-background"
  local error_marker="TMUX-WRAPPER-SMOKE-${run_id}-codex-error"
  local long_marker="TMUX-WRAPPER-SMOKE-${run_id}-codex-long"
  local before_ready

  printf '%s\n' "$basic_marker 한 줄로 marker와 cwd basename을 응답해줘." >"$prompt_file"
  start_tmux_wrapper codex "$session" "$output_file" "$input_fifo" "$prompt_file"
  wait_file_contains "$output_file" "$basic_marker"
  wait_tmux_contains "$session" "$basic_marker"
  wait_ready_increment "$output_file" codex 0

  before_ready="$(ready_count "$output_file" codex)"
  write_fifo_line "$input_fifo" "$(b64_prompt_line "$memory_marker 직전 marker도 함께 한 줄로 응답해줘.")"
  wait_file_contains "$output_file" "$memory_marker" "$basic_marker"
  wait_ready_increment "$output_file" codex "$before_ready"

  before_ready="$(ready_count "$output_file" codex)"
  write_fifo_line "$input_fifo" "$(b64_prompt_line "$multiline_marker 첫 줄 marker를 포함해줘.
두 번째 줄도 FIFO base64 decode 검증용이야.")"
  wait_file_contains "$output_file" "$multiline_marker" "multiline-ok"
  wait_ready_increment "$output_file" codex "$before_ready"

  before_ready="$(ready_count "$output_file" codex)"
  write_fifo_line "$input_fifo" "$(b64_prompt_line "$unicode_marker 한글/유니코드가 깨지지 않는지 응답해줘.")"
  wait_file_contains "$output_file" "$unicode_marker" "유니코드-정상"
  wait_ready_increment "$output_file" codex "$before_ready"

  before_ready="$(ready_count "$output_file" codex)"
  write_fifo_line "$input_fifo" "$(b64_prompt_line "$tool_marker command_execution 이벤트를 흘려줘.")"
  wait_file_contains "$output_file" "$tool_marker" "tool-result-ok"
  wait_file_text "$output_file" "tool_use" "codex command_execution tool_use translation"
  wait_file_text "$output_file" "tool_result" "codex command_execution tool_result translation"
  wait_ready_increment "$output_file" codex "$before_ready"

  before_ready="$(ready_count "$output_file" codex)"
  write_fifo_line "$input_fifo" "$(b64_prompt_line "$background_marker background_event를 task notification으로 변환해줘.")"
  wait_file_contains "$output_file" "$background_marker" "background-ok"
  wait_file_text "$output_file" "task_notification" "codex background task notification"
  wait_ready_increment "$output_file" codex "$before_ready"

  before_ready="$(ready_count "$output_file" codex)"
  write_fifo_line "$input_fifo" "$(b64_prompt_line "$error_marker 오류 이벤트를 흘려줘.")"
  wait_file_contains "$output_file" "$error_marker" "simulated-error"
  wait_file_text "$output_file" "error_during_execution" "codex error result"
  wait_ready_increment "$output_file" codex "$before_ready"

  before_ready="$(ready_count "$output_file" codex)"
  write_fifo_line "$input_fifo" "$(b64_prompt_line "$long_marker marker를 첫 줄에 포함하고 120줄을 출력해줘.")"
  wait_file_contains "$output_file" "$long_marker item 120"
  wait_tmux_contains "$session" "$long_marker"
  wait_ready_increment "$output_file" codex "$before_ready"

  echo "  codex:  $basic_marker"
}

run_claude_scenarios
run_codex_scenarios

echo "tmux-wrapper smoke completed:"
echo "  run: $run_id"
