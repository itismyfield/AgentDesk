#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────────
# setup-merge-drivers.sh — register AgentDesk custom git merge drivers (#4724).
#
# .gitattributes assigns `merge=regen-inventory` to the committed generated
# inventory docs, but a merge-driver definition in .gitattributes is INERT
# until the driver command is registered in this repo's local git config.
# Run this once per clone (also invoked by scripts/setup-hooks.sh).
# ──────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_ROOT"

# %O ancestor, %A ours (result written here), %B theirs, %P repo-relative path.
git config --local merge.regen-inventory.name \
  "regenerate committed inventory docs from the merged tree (#4724)"
git config --local merge.regen-inventory.driver \
  "bash scripts/git-merge-regen-inventory.sh %O %A %B %P"

chmod +x scripts/git-merge-regen-inventory.sh

echo "Registered git merge driver: regen-inventory"
echo "  name   = $(git config --local merge.regen-inventory.name)"
echo "  driver = $(git config --local merge.regen-inventory.driver)"
