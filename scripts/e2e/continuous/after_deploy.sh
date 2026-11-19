#!/bin/bash
# After a healthy deploy: hand the deployed SHA to the continuous E2E runner and keep its launchd tick loaded.
# Dormant without the machine-local config. Every failure is a printed note, never a deploy failure.
# Usage: after_deploy.sh <repo> <deployed sha> <deploy stamp> [time limit in seconds]
set -u
repo="${1:-}" sha="${2:-}" stamp="${3:-}" limit="${4:-20}"
config="${AGENTDESK_E2E_CONTINUOUS_CONFIG:-$HOME/.adk/e2e-continuous/config.json}"
[ -f "$config" ] || exit 0

companion() {
    local result label plist rendered
    result=$(cd "$repo" && python3 -m scripts.e2e.continuous.release_host --config "$config" \
        after-deploy --request-id "deploy-$stamp" --deploy-sha "$sha" 2>/dev/null) || result='{"status": "REJECTED"}'
    echo "  continuous E2E companion: $result"
    case "$result" in
        *'"status": "SUBMITTED"'*) ;;
        *) return 0 ;;
    esac

    # The tick job only wakes the runner; the runner itself decides what is due and within budget.
    command -v launchctl >/dev/null 2>&1 || return 0
    label="com.agentdesk.e2e-continuous"
    plist="$HOME/Library/LaunchAgents/$label.plist"
    rendered=$(sed -e "s|__REPO__|$repo|g" -e "s|__HOME__|$HOME|g" -e "s|__CONFIG__|$config|g" \
        "$repo/scripts/launchd/$label.plist") || return 0
    if [ "$rendered" != "$(cat "$plist" 2>/dev/null)" ]; then
        mkdir -p "$(dirname "$plist")" || return 0
        printf '%s\n' "$rendered" > "$plist.new" && mv "$plist.new" "$plist" || return 0
        launchctl bootout "gui/$(id -u)/$label" >/dev/null 2>&1 || true
    fi
    launchctl print "gui/$(id -u)/$label" >/dev/null 2>&1 \
        || launchctl bootstrap "gui/$(id -u)" "$plist" >/dev/null 2>&1 \
        || echo "  continuous E2E tick job could not be loaded: $plist"
}

# The companion runs in its own process group under a time limit, so a stalled step can neither hold
# the deploy nor outlive it with the deploy lock inherited.
set -m
companion &
work=$!
( sleep "$limit"; kill -KILL -- "-$work" ) >/dev/null 2>&1 &
watch=$!
if ! wait "$work" 2>/dev/null; then
    echo "  continuous E2E companion did not finish within ${limit}s; skipped"
fi
kill -KILL -- "-$watch" >/dev/null 2>&1
wait "$watch" 2>/dev/null
exit 0
