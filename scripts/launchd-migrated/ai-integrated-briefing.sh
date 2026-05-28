#!/bin/bash
set -euo pipefail

NOW_KST="$(TZ=Asia/Seoul date '+%Y-%m-%d %H:%M:%S %Z')"
PROMPT_FILE="$(mktemp)"
trap 'rm -f "$PROMPT_FILE"' EXIT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/launchd-migrated/_portable-resolver.sh
source "$SCRIPT_DIR/_portable-resolver.sh"
agentdesk_source_portable_resolver
SKILL_PATH="$(agentdesk_obsidian_skill_path "ai-integrated-briefing")"
agentdesk_optional_file_or_skip "ai-integrated-briefing skill" "$SKILL_PATH"

cat >"$PROMPT_FILE" <<EOF
Read and follow $SKILL_PATH exactly.

current time: $NOW_KST

Rules:
- Use web search for the last 24-72 hours as required by the skill.
- Prefer official sources and GitHub Releases only.
- Exclude Copilot and OpenClaw updates.
- If there is low novelty or no meaningful updates, return exactly NO_REPLY.
- Otherwise return only the final Discord-ready markdown briefing.
- Use markdown hyperlinks with angle brackets to suppress embeds: [라벨](<URL>)
- Do not send the message yourself.
- Do not wrap the final answer in code fences.
EOF

exec "$SCRIPT_DIR/run-claude-message-job.sh" \
  --search \
  --source "ai-integrated-briefing" \
  --target "channel:1470762182344966311" \
  --prompt-file "$PROMPT_FILE"
