#!/usr/bin/env bash
# Regression test for #4511: post-deploy WARN sampling starts at the dcserver
# restart watermark, excluding stale lines while retaining the spike threshold.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPLOY_SH="$REPO_ROOT/scripts/deploy-release.sh"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-smoke-warn-test.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

extract_function() {
    local function_name="$1"
    awk -v start="^${function_name}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$DEPLOY_SH"
}

# Exercise the production functions without executing the deploy script.
eval "$(extract_function _post_deploy_smoke_note)"
eval "$(extract_function _post_deploy_smoke_fail)"
eval "$(extract_function _post_deploy_smoke_check_fail_closed_warn_rate)"

ADK_REL="$TMP_ROOT/release"
POST_DEPLOY_SMOKE_TMP_DIR="$TMP_ROOT/smoke"
POST_DEPLOY_SMOKE_EVIDENCE="$TMP_ROOT/evidence.log"
POST_DEPLOY_SMOKE_LOG_PATH="$ADK_REL/logs/dcserver.stdout.log"
POST_DEPLOY_SMOKE_LOG_LINES=500
POST_DEPLOY_SMOKE_WARN_LIMIT=5
POST_DEPLOY_SMOKE_FAILURES=()
# The production functions loaded through eval consume these test globals;
# explicit exports/references make that dynamic use visible to ShellCheck.
export POST_DEPLOY_SMOKE_LOG_LINES POST_DEPLOY_SMOKE_WARN_LIMIT
: "${POST_DEPLOY_SMOKE_FAILURES[*]-}"
mkdir -p "$ADK_REL/logs" "$POST_DEPLOY_SMOKE_TMP_DIR"
: > "$POST_DEPLOY_SMOKE_EVIDENCE"

for index in 1 2 3 4 5; do
    printf '2026-07-14T08:03:4%sZ WARN fail-closed stale-before-restart\n' "$index"
done > "$POST_DEPLOY_SMOKE_LOG_PATH"
POST_DEPLOY_SMOKE_LOG_OFFSET=$(wc -c < "$POST_DEPLOY_SMOKE_LOG_PATH" | tr -d '[:space:]')
export POST_DEPLOY_SMOKE_LOG_OFFSET

printf '%s\n' \
    '2026-07-14T09:16:54Z INFO dcserver started' \
    '2026-07-14T09:16:55Z INFO startup recovery running' \
    >> "$POST_DEPLOY_SMOKE_LOG_PATH"

if ! _post_deploy_smoke_check_fail_closed_warn_rate; then
    echo "FAIL: stale pre-restart WARNs tripped the post-restart sampler" >&2
    exit 1
fi
if ! grep -q 'sample=2 warn_lines=0 fail_closed_warns=0 threshold=5' "$POST_DEPLOY_SMOKE_EVIDENCE"; then
    echo "FAIL: sampler did not exclude stale pre-restart WARNs" >&2
    exit 1
fi

for index in 1 2 3 4 5; do
    printf '2026-07-14T09:17:0%sZ WARN fail-closed new-after-restart\n' "$index"
done >> "$POST_DEPLOY_SMOKE_LOG_PATH"

if _post_deploy_smoke_check_fail_closed_warn_rate; then
    echo "FAIL: genuine post-restart WARN spike did not trip the threshold" >&2
    exit 1
fi
if ! grep -q 'sample=7 warn_lines=5 fail_closed_warns=5 threshold=5' "$POST_DEPLOY_SMOKE_EVIDENCE"; then
    echo "FAIL: post-restart WARN spike was not counted at the existing threshold" >&2
    exit 1
fi

echo "deploy smoke WARN post-restart scope tests passed"
