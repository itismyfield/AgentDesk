#!/usr/bin/env bash
# Regression coverage for #4902 helper-asset deployment boundaries.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=../scripts/routine-asset-surface.sh
. "$REPO_ROOT/scripts/routine-asset-surface.sh"

TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-routine-assets.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

SOURCE_ROOT="$TMP_ROOT/repo"
RUNTIME_ROOT="$TMP_ROOT/release"
HELPER_DIR="monitoring"
mkdir -p "$SOURCE_ROOT/routine-helpers/$HELPER_DIR" \
    "$RUNTIME_ROOT/routine-helpers/$HELPER_DIR" \
    "$RUNTIME_ROOT/routines/$HELPER_DIR"

for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
    printf 'repo-%s\n' "$helper_ref" > "$SOURCE_ROOT/routine-helpers/$helper_ref"
    printf 'legacy-%s\n' "$helper_ref" > "$RUNTIME_ROOT/routines/$helper_ref"
done
printf 'operator-private helper\n' > "$RUNTIME_ROOT/routine-helpers/$HELPER_DIR/operator-private.py"
printf 'stale bundled helper\n' > "$RUNTIME_ROOT/routine-helpers/$HELPER_DIR/daily_log_digest.py"
printf 'operator-private routine\n' > "$RUNTIME_ROOT/routines/$HELPER_DIR/operator-private.js"
printf 'bundled QuickJS entrypoint\n' > "$RUNTIME_ROOT/routines/$HELPER_DIR/daily-log-digest.js"

# Existing helper assets survive; tracked source wins only at matching paths.
HELPERS_STAGED="$(adk_stage_routine_helpers "$SOURCE_ROOT" "$RUNTIME_ROOT")"
if [ "$(<"$HELPERS_STAGED/$HELPER_DIR/operator-private.py")" != 'operator-private helper' ]; then
    echo 'FAIL: helper staging erased an operator-private asset' >&2
    exit 1
fi
if [ "$(<"$HELPERS_STAGED/$HELPER_DIR/daily_log_digest.py")" != 'repo-monitoring/daily_log_digest.py' ]; then
    echo 'FAIL: repository helper did not overlay its exact path' >&2
    exit 1
fi

# Simulate an old binary layout whose helpers were under the QuickJS routine
# root. Exactly the four migrated paths disappear; unrelated routine assets do
# not, proving this is a surgical compatibility tombstone rather than --delete.
ROUTINES_STAGED="$TMP_ROOT/routines.new"
rsync -a "$RUNTIME_ROOT/routines/" "$ROUTINES_STAGED/"
adk_remove_legacy_routine_helpers "$ROUTINES_STAGED"
for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
    if [ -e "$ROUTINES_STAGED/$helper_ref" ]; then
        echo "FAIL: legacy helper survived routine tombstone: $helper_ref" >&2
        exit 1
    fi
done
if [ ! -f "$ROUTINES_STAGED/$HELPER_DIR/operator-private.js" ] \
  || [ ! -f "$ROUTINES_STAGED/$HELPER_DIR/daily-log-digest.js" ]; then
    echo 'FAIL: routine tombstone removed a non-legacy entrypoint or operator asset' >&2
    exit 1
fi

# Helper swap follows the routines.new -> live -> .old pattern. This ensures an
# old release root gains the new sibling surface without a mixed live tree.
adk_swap_staged_routine_helpers "$RUNTIME_ROOT" "$HELPERS_STAGED"
if [ -d "$RUNTIME_ROOT/routine-helpers.old" ] \
  || [ ! -f "$RUNTIME_ROOT/routine-helpers/$HELPER_DIR/operator-private.py" ] \
  || [ ! -f "$RUNTIME_ROOT/routine-helpers/$HELPER_DIR/weekly_churn_audit.py" ]; then
    echo 'FAIL: routine helper swap did not produce the expected live surface' >&2
    exit 1
fi

# All release paths must wire the sibling surface; build artifacts remain a
# repo-only snapshot while both deploy paths use preservation-aware staging.
for script in scripts/build-release.sh scripts/deploy-release.sh scripts/deploy.sh; do
    if ! grep -Fq 'routine-helpers' "$REPO_ROOT/$script"; then
        echo "FAIL: $script does not package/deploy routine helpers" >&2
        exit 1
    fi
done
if ! grep -Fq 'adk_remove_legacy_routine_helpers "$ROUTINES_STAGED"' \
  "$REPO_ROOT/scripts/deploy-release.sh"; then
    echo 'FAIL: release deploy does not tombstone the exact legacy helper paths' >&2
    exit 1
fi
if grep -Fq -- '--delete "$REPO/routines/" "$ROUTINES_STAGED/"' \
  "$REPO_ROOT/scripts/deploy-release.sh"; then
    echo 'FAIL: release routine staging uses a broad delete instead of exact tombstones' >&2
    exit 1
fi

echo 'routine helper asset surface tests passed'
