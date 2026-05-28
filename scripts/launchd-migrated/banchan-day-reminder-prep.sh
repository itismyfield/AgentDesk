#!/bin/bash
set -euo pipefail

NOW_KST="$(TZ=Asia/Seoul date '+%Y-%m-%d %H:%M:%S %Z')"
PROMPT_FILE="$(mktemp)"
trap 'rm -f "$PROMPT_FILE"' EXIT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/launchd-migrated/_portable-resolver.sh
source "$SCRIPT_DIR/_portable-resolver.sh"
agentdesk_source_portable_resolver
SKILL_PATH="$(agentdesk_obsidian_skill_path "banchan-day-reminder")"
MESSAGES_PATH="$AGENTDESK_OBSIDIAN_SKILL_ROOT/banchan-day-reminder/references/messages.md"
agentdesk_optional_file_or_skip "banchan-day-reminder skill" "$SKILL_PATH"
agentdesk_optional_file_or_skip "banchan-day-reminder messages" "$MESSAGES_PATH"

cat >"$PROMPT_FILE" <<EOF
Read and follow these files exactly:
- $SKILL_PATH
- $MESSAGES_PATH

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

exec "$SCRIPT_DIR/run-claude-message-job.sh" \
  --source "banchan-day-reminder:prep" \
  --target "channel:1473922824350601297" \
  --prompt-file "$PROMPT_FILE"
