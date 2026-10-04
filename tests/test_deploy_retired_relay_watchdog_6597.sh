#!/usr/bin/env bash
# Execute only the retirement block with launchctl stubbed and a disposable HOME.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-retired-relay.XXXXXX")
trap 'rm -r "$TMP_ROOT"' EXIT
export HOME="$TMP_ROOT/home"
mkdir -p "$HOME/Library/LaunchAgents"
BLOCK=$(sed -n '/^# >>> BEGIN retired-relay-watchdog cleanup$/,/^# <<< END retired-relay-watchdog cleanup$/p' "$DEPLOY_SH")
[ -n "$BLOCK" ] || { echo 'FAIL: missing cleanup block' >&2; exit 1; }
LABEL="com.agentdesk.relay-watchdog"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOADED=0
BOOTOUTS=0
launchctl() {
    case "$1" in
        list) [ "$2" = "$LABEL" ] && [ "$LOADED" = 1 ] ;;
        bootout)
            [ "$2" = "gui/$(id -u)/$LABEL" ] || return 1
            BOOTOUTS=$((BOOTOUTS + 1))
            LOADED=0
            ;;
        *) echo "FAIL: unexpected launchctl action $1" >&2; return 1 ;;
    esac
}
check() {
    [ "$BOOTOUTS" = "$1" ] && [ ! -e "$PLIST" ] || {
        echo "FAIL: $2 (bootouts=$BOOTOUTS)" >&2; exit 1;
    }
    echo "PASS: $2"
}
eval "$BLOCK"
check 0 'absent config, job and plist are a no-op'
touch "$PLIST"
eval "$BLOCK"
check 0 'an unloaded old plist is removed without bootout'
LOADED=1
touch "$PLIST"
eval "$BLOCK"
check 1 'a loaded old job is booted out and its plist removed'
LOADED=1
eval "$BLOCK"
check 2 'a loaded old job without a plist is booted out'
eval "$BLOCK"
check 2 'repeated cleanup is a no-op'
if grep -Eq 'relay_watchdog\.py|WATCHDOG_CONFIG|WATCHDOG_BIN|_install_relay_watchdog_plist' "$DEPLOY_SH"; then
    echo 'FAIL: watchdog installation or arm wiring remains' >&2
    exit 1
fi
if ! grep -Fq 'bash tests/test_deploy_retired_relay_watchdog_6597.sh' "$REPO_ROOT/scripts/ci-script-checks.sh"; then
    echo 'FAIL: retirement regression is not wired into CI' >&2
    exit 1
fi
echo 'PASS: installation removed and cleanup regression wired into CI'
echo '6 checks passed; 0 failures; 0 skips'
