#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
# shellcheck disable=SC2034 # Used by the extracted staging block.
SCRIPT_DIR="$(dirname "$DEPLOY_SH")"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-deploy-routines.XXXXXX")
trap 'rm -r "$WORK"' EXIT

# Run the release entry point's blocks against fixtures, without running deploy or SSH.
awk '/^# Stage routine scripts/ { copy = 1 }
     copy && /^# Stage launchd-migrated shell entrypoints/ { exit }
     copy { print }' "$DEPLOY_SH" >"$WORK/stage"
awk '/^_deploy_to_one_peer\(\)/ { peer = 1 }
     peer && /^    if \[ -d "\$ADK_REL\/routines" \]; then$/ { copy = 1 }
     copy { print }
     copy && /^    fi$/ { exit }' "$DEPLOY_SH" >"$WORK/sync"
[ -s "$WORK/stage" ] && [ -s "$WORK/sync" ] || { echo 'FAIL: missing deploy routines blocks' >&2; exit 1; }

REPO="$WORK/checkout"
ADK_REL="$WORK/local"
EXPECTED="$WORK/expected"
mkdir -p "$REPO/routines/nested" "$ADK_REL/routines/nested"
git -C "$REPO" init -q
printf 'routines/*\n' >"$REPO/.gitignore"
printf 'live\n' >"$ADK_REL/routines/briefing.js"
printf 'old checkout\n' >"$REPO/routines/briefing.js"
printf 'checkout only\n' >"$REPO/routines/nested/ignored.js"
git -C "$REPO" check-ignore -q routines/briefing.js
git -C "$REPO" check-ignore -q routines/nested/ignored.js
cp -R "$ADK_REL/routines" "$EXPECTED"

compare_staging() (
    unset ROUTINES_STAGED
    # shellcheck disable=SC1091 # Extracted from deploy-release.sh above.
    . "$WORK/stage" || exit "$?"
    diff -r "$EXPECTED" "${ROUTINES_STAGED:-$ADK_REL/routines}"
)

failures=0
check() {
    local name="$1"
    shift
    if "$@"; then
        printf 'PASS: %s\n' "$name"
    else
        printf 'FAIL: %s\n' "$name" >&2
        failures=$((failures + 1))
    fi
}

check ignored_checkout_preserves_live compare_staging

tracked='nested/bundled routine.js'
printf 'old\n' >"$ADK_REL/routines/$tracked"
printf 'new\n' >"$REPO/routines/$tracked"
touch -r "$ADK_REL/routines/$tracked" "$REPO/routines/$tracked"
git -C "$REPO" add -f -- "routines/$tracked"
cp "$REPO/routines/$tracked" "$EXPECTED/$tracked"
check only_tracked_files_overlay compare_staging

sync_peer() (
    local peer=fixture peer_adk_rel="$WORK/peer"
    # shellcheck disable=SC2034 # Used by the extracted peer block.
    local DEPLOY_SSH_CONNECT_TIMEOUT=10
    rsync() {
        local opts=()
        while [ "$#" -gt 2 ]; do
            case "$1" in
                -e) shift 2 ;;
                *) opts+=("$1"); shift ;;
            esac
        done
        [ "$2" = "$peer:$peer_adk_rel/routines/" ] || return 1
        command rsync "${opts[@]}" "$1" "$peer_adk_rel/routines/"
    }

    mkdir -p "$peer_adk_rel/routines" || exit
    printf 'peer live\n' >"$peer_adk_rel/routines/briefing.js" || exit
    # shellcheck disable=SC1091 # Extracted from the peer deploy function above.
    . "$WORK/sync" || exit "$?"
    [ "$(cat "$peer_adk_rel/routines/briefing.js")" = 'peer live' ] &&
        cmp "$ADK_REL/routines/$tracked" "$peer_adk_rel/routines/$tracked"
)
check peer_preserves_existing_and_copies_missing sync_peer

[ "$failures" -eq 0 ]
