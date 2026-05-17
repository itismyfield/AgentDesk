#!/bin/bash
set -euo pipefail

NOW_KST="$(TZ=Asia/Seoul date '+%Y-%m-%d %H:%M:%S %Z')"
PROMPT_FILE="$(mktemp)"
trap 'rm -f "$PROMPT_FILE"' EXIT

cat >"$PROMPT_FILE" <<EOF
Read and follow these files exactly:
- /Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/banchan-day-reminder/SKILL.md
- /Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/banchan-day-reminder/references/messages.md

mode: prep
target channel: 1473922824350601297
current time: $NOW_KST

Rules:
- Use family calendar via gog CLI exactly as the skill requires.
- If there is no relevant event, return exactly NO_REPLY.
- If there is a relevant event, return the final Korean reminder message only.
- Keep the reminder practical and aligned with the template constraints.
- Do not send the message yourself.
- Do not wrap the final answer in code fences.
EOF

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "$SCRIPT_DIR/run-claude-message-job.sh" \
  --source "banchan-day-reminder:prep" \
  --target "channel:1473922824350601297" \
  --prompt-file "$PROMPT_FILE"
