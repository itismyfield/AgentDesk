#!/bin/bash
set -euo pipefail

NOW_KST="$(TZ=Asia/Seoul date '+%Y-%m-%d %H:%M:%S %Z')"
PROMPT_FILE="$(mktemp)"
trap 'rm -f "$PROMPT_FILE"' EXIT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/launchd-migrated/_portable-resolver.sh
source "$SCRIPT_DIR/_portable-resolver.sh"
agentdesk_source_portable_resolver
SKILL_PATH="$(agentdesk_obsidian_skill_path "cookingheart-daily-briefing")"
agentdesk_optional_file_or_skip "cookingheart-daily-briefing skill" "$SKILL_PATH"

cat >"$PROMPT_FILE" <<EOF
Read and follow $SKILL_PATH exactly.

current time: $NOW_KST

Rules:
- Use local git, gh, nc, and ssh commands as needed by the skill.
- Never start a long build.
- If there is little change, say so and keep the output concise.
- Return only the final Korean Discord message ready to send, or NO_REPLY if that is the correct result.
- Do not send the message yourself.
- Do not wrap the final answer in code fences.
EOF

exec "$SCRIPT_DIR/run-claude-message-job.sh" \
  --source "cookingheart-daily-briefing" \
  --target "channel:1479644764294086877" \
  --prompt-file "$PROMPT_FILE"
