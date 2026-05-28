#!/bin/bash
set -euo pipefail

NOW_KST="$(TZ=Asia/Seoul date '+%Y-%m-%d %H:%M:%S %Z')"
PROMPT_FILE="$(mktemp)"
trap 'rm -f "$PROMPT_FILE"' EXIT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/launchd-migrated/_portable-resolver.sh
source "$SCRIPT_DIR/_portable-resolver.sh"
agentdesk_source_portable_resolver

DEFAULT_MANAGED_SKILL="$AGENTDESK_ROOT_DIR/skills/memory-merge/SKILL.md"
SKILL_PATH="${AGENTDESK_MEMORY_MERGE_SKILL:-$DEFAULT_MANAGED_SKILL}"

if [[ -n "${AGENTDESK_MEMORY_MERGE_SKILL:-}" && ! -f "$SKILL_PATH" ]]; then
  echo "memory-merge skill not found: $SKILL_PATH" >&2
  exit 1
fi

if [[ ! -f "$SKILL_PATH" ]]; then
  echo "memory-merge skill not found: $SKILL_PATH" >&2
  exit 1
fi

cat >"$PROMPT_FILE" <<EOF
Read and follow $SKILL_PATH exactly.

current time: $NOW_KST

Rules:
- Scan all agent workspace memory files as specified in the skill.
- Merge shared knowledge and clean up individual memories.
- Return only the final Korean Discord summary message, or NO_REPLY if nothing changed.
- Do not wrap the final answer in code fences.
EOF

exec "$SCRIPT_DIR/run-claude-message-job.sh" \
  --source "memory-merge" \
  --target "channel:1480015244062490774" \
  --workdir "$AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR" \
  --prompt-file "$PROMPT_FILE"
