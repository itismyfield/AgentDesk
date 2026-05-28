#!/usr/bin/env bash
set -euo pipefail

# DEPRECATED (2026-05-28): drives the python harness with
# --channel-id-cc/--channel-id-cdx, which the cell-based driver no longer
# accepts. Use `scripts/e2e/run_tui_relay.py --cell <cell> --channel-id <id>`
# per cell once the adk-{cell}-e2e channels are live (PR 3 of the
# multi-provider-e2e migration).
echo "DEPRECATED: see header for cell-based replacement." >&2
exit 64

# Live Discord relay E2E for the legacy tmux-wrapper path.
#
# This does not use dedicated wrapper channels. It temporarily flips the
# existing adk-dashboard-e2e channel overrides to tui_hosting: false, restarts
# release dcserver through the safe restart wrapper, runs the relay scenarios,
# then restores the original config and restarts release again.

AGENTDESK_BIN="${AGENTDESK_BIN:-$HOME/.adk/release/bin/agentdesk}"
CONFIG_PATH="${CONFIG_PATH:-$HOME/.adk/release/config/agentdesk.yaml}"
RESTART_SCRIPT="${RESTART_SCRIPT:-$HOME/.adk/release/skills/agentdesk-restart/scripts/restart_agentdesk.sh}"
BASE_URL="${BASE_URL:-http://127.0.0.1:8791}"
CLAUDE_CHANNEL="${CLAUDE_CHANNEL:-1506295332949196840}"
CODEX_CHANNEL="${CODEX_CHANNEL:-1506295335096549406}"
SCENARIOS_DIR="${SCENARIOS_DIR:-tests/e2e/tui_relay/scenarios}"
RUNNER="${RUNNER:-scripts/e2e/run_tui_relay.py}"
PROVIDERS="${PROVIDERS:-claude,codex}"

DEFAULT_FILTER="E-1,E-2,E-3,E-5,E-6,E-7,E-11"
DESTRUCTIVE_FILTER="E-8,E-9,E-12"
FILTER="${FILTER:-$DEFAULT_FILTER}"
OUTPUT_DIR="${OUTPUT_DIR:-}"

dry_run=0
include_destructive=0
filter_explicit=0

usage() {
  cat <<'USAGE'
Usage: scripts/e2e-wrapper-relay-toggle.sh [options]

Temporarily sets adk-dashboard-e2e channel tui_hosting=false, restarts release,
runs live Discord relay E2E scenarios, then restores the original config.

Options:
  --dry-run                  Print the planned config toggle and runner command.
  --include-destructive      Also run restart/pane-kill scenarios E-8,E-9,E-12.
  --filter IDS              Comma-separated exact scenario IDs to run.
  --output DIR              E2E report directory.
  --config PATH             agentdesk.yaml path.
  --base-url URL            AgentDesk API URL (default: http://127.0.0.1:8791).
  --providers LIST          Comma list of channel providers to toggle
                             (default: claude,codex).
  -h, --help                Show this help.

Default filter excludes TUI-direct composer scenarios E-4 and E-10 because the
wrapper path is validated through Discord -> tmux-wrapper/FIFO -> Discord.
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --dry-run)
      dry_run=1
      shift
      ;;
    --include-destructive)
      include_destructive=1
      shift
      ;;
    --filter)
      FILTER="${2:?missing value for --filter}"
      filter_explicit=1
      shift 2
      ;;
    --output)
      OUTPUT_DIR="${2:?missing value for --output}"
      shift 2
      ;;
    --config)
      CONFIG_PATH="${2:?missing value for --config}"
      shift 2
      ;;
    --base-url)
      BASE_URL="${2:?missing value for --base-url}"
      shift 2
      ;;
    --providers)
      PROVIDERS="${2:?missing value for --providers}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [ "$include_destructive" -eq 1 ] && [ "$filter_explicit" -eq 0 ]; then
  FILTER="${DEFAULT_FILTER},${DESTRUCTIVE_FILTER}"
fi

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required command: $1" >&2
    exit 2
  }
}

wait_for_health() {
  local deadline=$((SECONDS + 90))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if curl -fsS --max-time 5 "$BASE_URL/api/health" >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
  done
  echo "dcserver did not become healthy at $BASE_URL/api/health" >&2
  return 1
}

guard_no_foreign_active_turns() {
  # Refuse restart if any non-E2E channel has a live turn (typical case:
  # the bot itself was driven from a non-E2E channel to invoke this test).
  # See 2026-05-26 adk-cdx incident.
  local sessions_json busy_lines
  sessions_json="$(curl -fsS --max-time 5 "$BASE_URL/api/sessions" 2>/dev/null || true)"
  [ -z "$sessions_json" ] && return 0
  busy_lines="$(
    printf '%s' "$sessions_json" \
      | python3 -c '
import json, sys
data = json.loads(sys.stdin.read() or "{}")
items = data.get("sessions") if isinstance(data, dict) else data
busy = []
allowed = {"'"$CLAUDE_CHANNEL"'", "'"$CODEX_CHANNEL"'"}
for s in items or []:
    status = str(s.get("status", "")).lower()
    if status not in {"turn_active", "turn_busy", "active"}:
        continue
    key = str(s.get("session_key") or "")
    chan = str(s.get("channel_id") or s.get("channelId") or "")
    if chan in allowed:
        continue
    if any(cid and cid in key for cid in allowed if cid):
        continue
    busy.append(key or chan or "<unknown>")
for line in busy:
    print(line)
' 2>/dev/null || true
  )"
  if [ -n "$busy_lines" ]; then
    echo "REFUSE: live turn(s) outside E2E channels — refusing restart_release" >&2
    echo "  E2E channels: cc=$CLAUDE_CHANNEL cdx=$CODEX_CHANNEL" >&2
    echo "  Active: $busy_lines" >&2
    return 1
  fi
  return 0
}

restart_release() {
  if [ "$dry_run" -eq 1 ]; then
    echo "[dry-run] $RESTART_SCRIPT release"
    return 0
  fi
  if ! guard_no_foreign_active_turns; then
    return 1
  fi
  "$RESTART_SCRIPT" release
  wait_for_health
}

run_config_audit() {
  if [ "$dry_run" -eq 1 ]; then
    echo "[dry-run] $AGENTDESK_BIN config audit --dry-run"
    return 0
  fi
  "$AGENTDESK_BIN" config audit --dry-run
}

set_e2e_tui_hosting() {
  local desired="$1"
  if [ "$dry_run" -eq 1 ]; then
    echo "[dry-run] set adk-dashboard-e2e providers ($PROVIDERS) tui_hosting=$desired in $CONFIG_PATH"
    return 0
  fi
  DESIRED_TUI_HOSTING="$desired" CONFIG_PATH="$CONFIG_PATH" PROVIDERS="$PROVIDERS" python3 - <<'PY'
from __future__ import annotations

import os
import re
import sys
from pathlib import Path

import yaml

path = Path(os.environ["CONFIG_PATH"]).expanduser()
desired_text = os.environ["DESIRED_TUI_HOSTING"].strip().lower()
if desired_text not in {"true", "false"}:
    raise SystemExit(f"bad DESIRED_TUI_HOSTING={desired_text!r}")
desired = desired_text == "true"
providers = [item.strip() for item in os.environ["PROVIDERS"].split(",") if item.strip()]
if not providers:
    raise SystemExit("no providers requested")

lines = path.read_text(encoding="utf-8").splitlines(keepends=True)

def indent_of(line: str) -> int:
    return len(line) - len(line.lstrip(" "))

agent_start = None
agent_indent = 0
for idx, line in enumerate(lines):
    if re.match(r"^\s*-\s+id:\s*['\"]?adk-dashboard-e2e['\"]?\s*$", line):
        agent_start = idx
        agent_indent = indent_of(line)
        break
if agent_start is None:
    raise SystemExit("agent adk-dashboard-e2e not found")

agent_end = len(lines)
for idx in range(agent_start + 1, len(lines)):
    if indent_of(lines[idx]) == agent_indent and re.match(r"^\s*-\s+id:\s*", lines[idx]):
        agent_end = idx
        break

changed = False
for provider in providers:
    provider_start = None
    provider_indent = 0
    pattern = re.compile(rf"^\s*{re.escape(provider)}:\s*$")
    for idx in range(agent_start + 1, agent_end):
        if pattern.match(lines[idx]):
            provider_start = idx
            provider_indent = indent_of(lines[idx])
            break
    if provider_start is None:
        raise SystemExit(f"provider channel {provider!r} not found in adk-dashboard-e2e")

    provider_end = agent_end
    for idx in range(provider_start + 1, agent_end):
        stripped = lines[idx].strip()
        if stripped and indent_of(lines[idx]) <= provider_indent:
            provider_end = idx
            break

    tui_line = None
    for idx in range(provider_start + 1, provider_end):
        if re.match(r"^\s*tui_hosting\s*:", lines[idx]):
            tui_line = idx
            break

    replacement = " " * (provider_indent + 2) + f"tui_hosting: {desired_text}\n"
    if tui_line is None:
        lines.insert(provider_end, replacement)
        agent_end += 1
        changed = True
    elif lines[tui_line] != replacement:
        lines[tui_line] = replacement
        changed = True

if changed:
    path.write_text("".join(lines), encoding="utf-8")

doc = yaml.safe_load(path.read_text(encoding="utf-8"))
agent = next((item for item in doc.get("agents", []) if item.get("id") == "adk-dashboard-e2e"), None)
if agent is None:
    raise SystemExit("updated YAML lost adk-dashboard-e2e")
channels = agent.get("channels") or {}
for provider in providers:
    actual = (channels.get(provider) or {}).get("tui_hosting")
    if actual is not desired:
        raise SystemExit(f"{provider}.tui_hosting expected {desired}, found {actual!r}")
print(f"set adk-dashboard-e2e tui_hosting={desired_text} for {','.join(providers)}")
PY
}

require_cmd curl
require_cmd python3
require_cmd tmux

python3 - <<'PY'
import yaml  # noqa: F401
PY

[ -x "$AGENTDESK_BIN" ] || {
  echo "agentdesk binary not executable: $AGENTDESK_BIN" >&2
  exit 2
}
[ -f "$CONFIG_PATH" ] || {
  echo "config not found: $CONFIG_PATH" >&2
  exit 2
}
[ -x "$RESTART_SCRIPT" ] || {
  echo "restart script not executable: $RESTART_SCRIPT" >&2
  exit 2
}
[ -x "$RUNNER" ] || {
  echo "E2E runner not executable: $RUNNER" >&2
  exit 2
}
[ -d "$SCENARIOS_DIR" ] || {
  echo "scenarios dir not found: $SCENARIOS_DIR" >&2
  exit 2
}

run_id="$(date +%Y%m%d-%H%M%S)"
if [ -z "$OUTPUT_DIR" ]; then
  OUTPUT_DIR="out/e2e/wrapper_relay/$run_id"
fi

backup_path="$(mktemp "${TMPDIR:-/tmp}/agentdesk-e2e-wrapper-config.XXXXXX")"
restore_needed=0

cleanup() {
  local status=$?
  if [ "$restore_needed" -eq 1 ]; then
    if [ "$dry_run" -eq 1 ]; then
      echo "[dry-run] restore original config from backup and restart release"
    elif ! cmp -s "$backup_path" "$CONFIG_PATH"; then
      echo "[wrapper-e2e] restoring original config"
      cp "$backup_path" "$CONFIG_PATH"
      run_config_audit || status=1
      restart_release || status=1
    fi
  fi
  rm -f "$backup_path"
  exit "$status"
}
trap cleanup EXIT

cp "$CONFIG_PATH" "$backup_path"
restore_needed=1

echo "[wrapper-e2e] toggling existing E2E channels to legacy tmux-wrapper mode"
set_e2e_tui_hosting false
run_config_audit
restart_release

runner_args=(
  "$RUNNER"
  --base-url "$BASE_URL"
  --channel-id-cc "$CLAUDE_CHANNEL"
  --channel-id-cdx "$CODEX_CHANNEL"
  --scenarios "$SCENARIOS_DIR"
  --filter "$FILTER"
  --output "$OUTPUT_DIR"
  --no-reset-before-each
  --require-cdx
  --restart-script "$RESTART_SCRIPT"
  --restart-target-override release
)

if [ "$dry_run" -eq 1 ]; then
  runner_args+=(--dry-run)
fi

if [ "$include_destructive" -eq 1 ] || [[ ",$FILTER," =~ ,E-(8|9|12), ]]; then
  runner_args+=(--allow-destructive)
  export AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1
fi

AGENTDESK_BIN_DIR="$(dirname "$AGENTDESK_BIN")"
export PATH="$AGENTDESK_BIN_DIR:$PATH"

echo "[wrapper-e2e] running scenarios: $FILTER"
"${runner_args[@]}"

echo "[wrapper-e2e] passed. Report: $OUTPUT_DIR/report.json"
