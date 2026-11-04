#!/usr/bin/env bash
unset TMUX TMUX_PANE
set -euo pipefail

api_response=""
if [ "$#" -gt 0 ]; then
  if [ "$#" -ne 2 ] || [ "$1" != --api-response ] || [ ! -f "$2" ]; then
    echo "Usage: $0 [--api-response <JSON file>]" >&2
    exit 2
  fi
  api_response="$2"
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
. "$REPO_ROOT/scripts/_defaults.sh"
fixture=$(mktemp -d)
trap 'rm -r "$fixture"' EXIT
mkdir "$fixture/bin" "$fixture/no-tmux"
ln -s "$(command -v python3)" "$fixture/no-tmux/python3"
cat >"$fixture/bin/tmux" <<'SH'
#!/bin/sh
printf '%s\n' "$*" >>"$PROBE_LOG"
utf8=0
if [ "$1" = -u ]; then utf8=1; shift; fi
[ "$*" = 'list-sessions -F #{session_name}' ] || exit 99
[ -n "${TMUX:-}" ] && exit 0
case "$PROBE_MODE" in
  empty) exit 0 ;;
  live) printf '%s\n' AgentDesk-codex-live; exit 0 ;;
  unicode) if [ "$utf8" = 1 ]; then printf '%s\n' AgentDesk-codex-살아있음; else printf '%s\n' AgentDesk-codex-________; fi; exit 0 ;;
  server) echo 'no server running' >&2; exit 1 ;;
  denied) echo 'permission denied' >&2; exit 1 ;;
  timeout) sleep 10; exit 0 ;;
esac
exit 99
SH
chmod +x "$fixture/bin/tmux"
export PROBE_LOG="$fixture/probes" PROBE_MODE=empty
task_path="$PATH"
export PATH="$fixture/bin:$PATH"
unset AGENTDESK_RESTART_ALLOW_FOREIGN_TURNS

curl() {
  printf '%s\n' "$*" >"$fixture/curl-args"
  /bin/cat "$fixture/api.json"
}

write_row() {
  printf '{"sessions":[{"session_key":"codex/hash/old-host:AgentDesk-codex-%s","channel_id":"42","status":"%s","owner":{"is_local":%s},"restart_gate_local_tmux":%s}]}\n' \
    "$1" "${2:-turn_active}" "${3:-true}" "${4:-true}" >"$fixture/api.json"
}

checks=0
failures=0
expect_gate() {
  local label="$1" expected="$2" exempt="${3:-}" rc=0
  guard_no_foreign_active_turns_or_warn 0 "$exempt" >"$fixture/out" 2>&1 || rc=$?
  checks=$((checks + 1))
  if [ "$rc" = "$expected" ]; then
    echo "PASS: $label rc=$rc"
  else
    echo "FAIL: $label expected=$expected actual=$rc"
    /bin/cat "$fixture/out"
    failures=$((failures + 1))
  fi
}

if [ -n "$api_response" ]; then
  /bin/cat "$api_response" >"$fixture/api.json"
  PROBE_MODE=live
  expect_gate 'API direct process stays protected with only another tmux session present' 1
  printf 'checks=%s failures=%s skip=0\n' "$checks" "$failures"
  [ "$failures" -eq 0 ]
  exit 0
fi

write_row dead
expect_gate 'absent old local tmux needs no override' 0
if ! /usr/bin/grep -q '/api/sessions?restart_gate=1' "$fixture/curl-args"; then
  echo 'FAIL: collector must request host evidence'
  failures=$((failures + 1))
fi
write_row live
PROBE_MODE=live
expect_gate 'live foreign channel remains protected' 1
/usr/bin/grep -q 'AgentDesk-codex-live' "$fixture/out"
TMUX=/wrong-caller-socket TMUX_PANE=%7770 expect_gate 'caller socket cannot hide a live runtime session' 1
PROBE_MODE=unicode
write_row 살아있음
expect_gate 'non-ASCII live session stays protected' 1
PROBE_MODE=live
write_row dead
expect_gate 'successful snapshot with only another session permits dead row' 0
write_row live
expect_gate 'exact channel exemption' 0 42
PROBE_MODE=empty
for host in herdr process unknown; do
  write_row "$host" turn_active true false
  expect_gate "$host is protected without tmux" 1
done
write_row remote turn_active false true
expect_gate 'remote owner cannot be disproven locally' 1
printf '%s\n' '{"sessions":[{"session_key":"host:AgentDesk-codex-unknown","status":"turn_active"}]}' >"$fixture/api.json"
expect_gate 'old API without host evidence stays protected' 1
write_row dead
for PROBE_MODE in server denied timeout; do
  export PROBE_MODE
  expect_gate "tmux $PROBE_MODE remains unconfirmed" 1
done
export PATH="$fixture/no-tmux"
expect_gate 'tmux spawn failure remains unconfirmed' 1
PATH="$fixture/bin:$task_path"
PROBE_MODE=empty
write_row dead idle
expect_gate 'idle rows do not block' 0
write_row live
PROBE_MODE=live
export AGENTDESK_RESTART_ALLOW_FOREIGN_TURNS=1
expect_gate 'explicit override still works for live turn' 0
printf 'checks=%s failures=%s skip=0\n' "$checks" "$failures"
[ "$failures" -eq 0 ]
