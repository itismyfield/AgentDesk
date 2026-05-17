#!/bin/bash
set -euo pipefail

NOW_KST="$(TZ=Asia/Seoul date '+%Y-%m-%d %H:%M:%S %Z')"
PROMPT_FILE="$(mktemp)"
trap 'rm -f "$PROMPT_FILE"' EXIT

cat >"$PROMPT_FILE" <<EOF
Read and follow /Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/cookingheart-daily-briefing/SKILL.md exactly.

current time: $NOW_KST

Rules:
- Use local git, gh, nc, and ssh commands as needed by the skill.
- Never start a long build.
- If there is little change, say so and keep the output concise.
- Return only the final Korean Discord message ready to send, or NO_REPLY if that is the correct result.
- Do not send the message yourself.
- Do not wrap the final answer in code fences.
EOF

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "$SCRIPT_DIR/run-claude-message-job.sh" \
  --source "cookingheart-daily-briefing" \
  --target "channel:1479644764294086877" \
  --prompt-file "$PROMPT_FILE"
