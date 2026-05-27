#!/usr/bin/env bash
set -euo pipefail

# Provision the dedicated multi-provider E2E channels under one Discord
# category. Idempotent — re-runs return the existing ids without creating
# duplicates. Output is JSONL on stdout; one line per resource with the
# id you need to paste into ~/.adk/release/config/agentdesk.yaml.
#
# Requires:
#  - PR 2 deployed (provides `agentdesk discord category-create / channel-create`).
#  - announce bot has admin perms in the target guild.
#  - dcserver running (the CLI builds AppState).
#
# Usage:
#   scripts/setup-multi-provider-e2e.sh [--category-name "ADK E2E"]
#                                       [--guild-id <id>]
#                                       [--dry-run]

AGENTDESK_BIN="${AGENTDESK_BIN:-$HOME/.adk/release/bin/agentdesk}"
CATEGORY_NAME="${CATEGORY_NAME:-ADK E2E}"
GUILD_ID=""
DRY_RUN=0

usage() {
  cat <<USAGE
Usage: scripts/setup-multi-provider-e2e.sh [options]

Options:
  --category-name NAME   Discord category name (default: ADK E2E)
  --guild-id ID          Override agentdesk.yaml discord.guild_id
  --dry-run              Print intended CLI commands without running them
  -h, --help             Show this help
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --category-name)
      CATEGORY_NAME="${2:?missing value for --category-name}"
      shift 2
      ;;
    --guild-id)
      GUILD_ID="${2:?missing value for --guild-id}"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=1
      shift
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

if [ ! -x "$AGENTDESK_BIN" ]; then
  echo "agentdesk binary not found at $AGENTDESK_BIN (set AGENTDESK_BIN to override)" >&2
  exit 2
fi

# Preflight: PR 2 ships `discord category-create` / `channel-create`. Refuse to
# run live against an older binary so we never partially provision and then
# error out mid-way.
if [ "$DRY_RUN" -ne 1 ]; then
  if ! "$AGENTDESK_BIN" discord --help 2>/dev/null | grep -q "category-create"; then
    cat >&2 <<MISSING
$AGENTDESK_BIN is missing the \`discord category-create\` subcommand.
This script requires PR #2804 (CLI: discord category/channel/thread create)
to be merged and deployed to ~/.adk/release/bin/agentdesk.

  - Check release deploy:  $AGENTDESK_BIN --version
  - Or re-deploy:           scripts/deploy-release.sh

Re-run with --dry-run to preview the CLI invocations without needing PR 2.
MISSING
    exit 2
  fi
fi

run_cli() {
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "[dry-run] $AGENTDESK_BIN $*" >&2
    return 0
  fi
  "$AGENTDESK_BIN" "$@"
}

extract_id() {
  python3 -c '
import json, sys
doc = json.load(sys.stdin)
print(doc["id"])
'
}

guild_args=()
if [ -n "$GUILD_ID" ]; then
  guild_args=(--guild-id "$GUILD_ID")
fi

# 1. Category
category_json="$(run_cli discord category-create --name "$CATEGORY_NAME" "${guild_args[@]+"${guild_args[@]}"}")"
[ "$DRY_RUN" -eq 1 ] && category_json='{"id":"PLACEHOLDER_CATEGORY","name":"'"$CATEGORY_NAME"'","kind":"category","created":false}'
echo "$category_json"
category_id="$(printf '%s' "$category_json" | extract_id)"

# 2. Channels — 5 worker cells + 1 orchestrator
channels=(
  "adk-claude-pipe-e2e|Claude pipe runtime E2E worker"
  "adk-claude-tui-e2e|Claude tui runtime E2E worker"
  "adk-claude-e-e2e|claude-e runtime E2E worker"
  "adk-codex-pipe-e2e|Codex pipe runtime E2E worker"
  "adk-codex-tui-e2e|Codex tui runtime E2E worker"
  "adk-e2e-orchestrator|Multi-provider E2E orchestrator (전체 e2e 시작 등)"
)

for entry in "${channels[@]}"; do
  name="${entry%%|*}"
  topic="${entry#*|}"
  channel_json="$(
    run_cli discord channel-create \
      --name "$name" \
      --category-id "$category_id" \
      --topic "$topic" \
      "${guild_args[@]+"${guild_args[@]}"}"
  )"
  [ "$DRY_RUN" -eq 1 ] && channel_json='{"id":"PLACEHOLDER_'"$name"'","name":"'"$name"'","kind":"text","category_id":"'"$category_id"'","created":false}'
  echo "$channel_json"
done

cat >&2 <<NEXT

Next steps:
  1. Open ~/.adk/release/config/agentdesk.yaml.
  2. Under \`agents:\`, replace each \`PLACEHOLDER_ADK_*\` id with the matching
     channel id printed above. (See agentdesk.example.yaml for the entry
     shape.)
  3. \`agentdesk restart-dcserver\` so the workers come online.
  4. Smoke a single cell:
       scripts/e2e/run_tui_relay.py --cell claude-pipe \\
         --channel-id <id of adk-claude-pipe-e2e>
NEXT
