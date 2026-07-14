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
eval "$(extract_function _post_deploy_smoke_log_identity_and_size)"
eval "$(extract_function _post_deploy_smoke_log_head_fingerprint)"
eval "$(extract_function _post_deploy_smoke_note)"
eval "$(extract_function _post_deploy_smoke_fail)"
eval "$(extract_function _post_deploy_smoke_check_fail_closed_warn_rate)"

capture_log_watermark() {
    local fingerprint_bytes fingerprint_path
    if [ ! -e "$POST_DEPLOY_SMOKE_LOG_PATH" ]; then
        POST_DEPLOY_SMOKE_LOG_INODE=0
        POST_DEPLOY_SMOKE_LOG_OFFSET=0
        fingerprint_path=/dev/null
    else
        read -r POST_DEPLOY_SMOKE_LOG_INODE POST_DEPLOY_SMOKE_LOG_OFFSET \
            <<< "$(_post_deploy_smoke_log_identity_and_size "$POST_DEPLOY_SMOKE_LOG_PATH")"
        fingerprint_path="$POST_DEPLOY_SMOKE_LOG_PATH"
    fi
    fingerprint_bytes="$POST_DEPLOY_SMOKE_LOG_OFFSET"
    if [ "$fingerprint_bytes" -gt "$POST_DEPLOY_SMOKE_LOG_FINGERPRINT_CAP" ]; then
        fingerprint_bytes="$POST_DEPLOY_SMOKE_LOG_FINGERPRINT_CAP"
    fi
    POST_DEPLOY_SMOKE_LOG_FINGERPRINT=$(
        _post_deploy_smoke_log_head_fingerprint \
            "$fingerprint_path" "$fingerprint_bytes"
    )
    export POST_DEPLOY_SMOKE_LOG_INODE POST_DEPLOY_SMOKE_LOG_OFFSET
    export POST_DEPLOY_SMOKE_LOG_FINGERPRINT
}

ADK_REL="$TMP_ROOT/release"
POST_DEPLOY_SMOKE_TMP_DIR="$TMP_ROOT/smoke"
POST_DEPLOY_SMOKE_EVIDENCE="$TMP_ROOT/evidence.log"
POST_DEPLOY_SMOKE_LOG_PATH="$ADK_REL/logs/dcserver.stdout.log"
POST_DEPLOY_SMOKE_LOG_FINGERPRINT_CAP=4096
POST_DEPLOY_SMOKE_LOG_LINES=500
POST_DEPLOY_SMOKE_WARN_LIMIT=5
POST_DEPLOY_SMOKE_FAILURES=()
# The production functions loaded through eval consume these test globals;
# explicit exports/references make that dynamic use visible to ShellCheck.
export POST_DEPLOY_SMOKE_LOG_LINES POST_DEPLOY_SMOKE_WARN_LIMIT
: "${POST_DEPLOY_SMOKE_FAILURES[*]-}"
mkdir -p "$ADK_REL/logs" "$POST_DEPLOY_SMOKE_TMP_DIR"
: > "$POST_DEPLOY_SMOKE_EVIDENCE"

capture_log_watermark
if [ "$POST_DEPLOY_SMOKE_LOG_INODE" -ne 0 ] \
  || [ "$POST_DEPLOY_SMOKE_LOG_OFFSET" -ne 0 ] \
  || [ -z "$POST_DEPLOY_SMOKE_LOG_FINGERPRINT" ]; then
    echo "FAIL: absent log did not produce a complete offset-zero watermark" >&2
    exit 1
fi
if [ "$(_post_deploy_smoke_log_head_fingerprint "$POST_DEPLOY_SMOKE_LOG_PATH" 0)" \
  != "$POST_DEPLOY_SMOKE_LOG_FINGERPRINT" ]; then
    echo "FAIL: offset-zero fingerprint did not bypass the absent log path" >&2
    exit 1
fi
for index in 1 2 3 4 5; do
    printf '2026-07-14T09:15:0%sZ WARN fail-closed new-after-absent-log\n' "$index"
done > "$POST_DEPLOY_SMOKE_LOG_PATH"
if _post_deploy_smoke_check_fail_closed_warn_rate; then
    echo "FAIL: WARN spike after an absent-log watermark did not trip the threshold" >&2
    exit 1
fi
if ! grep -q 'sample=5 warn_lines=5 fail_closed_warns=5 threshold=5' "$POST_DEPLOY_SMOKE_EVIDENCE"; then
    echo "FAIL: offset-zero sampler did not read the full new log" >&2
    exit 1
fi
if grep -q 'restart log fingerprint unavailable\|could not fingerprint current log' "$POST_DEPLOY_SMOKE_EVIDENCE"; then
    echo "FAIL: offset-zero watermark raised a spurious fingerprint failure" >&2
    exit 1
fi
echo "absent-log-then-WARN: $(grep 'sample=5 warn_lines=5 fail_closed_warns=5 threshold=5' "$POST_DEPLOY_SMOKE_EVIDENCE")"

POST_DEPLOY_SMOKE_FAILURES=()
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
for index in 1 2 3 4 5; do
    printf '2026-07-14T08:03:4%sZ WARN fail-closed stale-before-restart\n' "$index"
done > "$POST_DEPLOY_SMOKE_LOG_PATH"
capture_log_watermark

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

POST_DEPLOY_SMOKE_FAILURES=()
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
for ((index = 1; index <= 40; index++)); do
    printf 'stale-before-truncation-%02d padding-padding-padding-padding-padding\n' "$index"
done > "$POST_DEPLOY_SMOKE_LOG_PATH"
capture_log_watermark
: > "$POST_DEPLOY_SMOKE_LOG_PATH"
for index in 1 2 3 4 5; do
    printf '2026-07-14T09:18:0%sZ WARN fail-closed new-after-truncation\n' "$index"
done >> "$POST_DEPLOY_SMOKE_LOG_PATH"
read -r current_inode current_size \
    <<< "$(_post_deploy_smoke_log_identity_and_size "$POST_DEPLOY_SMOKE_LOG_PATH")"
if [ "$current_inode" != "$POST_DEPLOY_SMOKE_LOG_INODE" ] \
  || [ "$current_size" -ge "$POST_DEPLOY_SMOKE_LOG_OFFSET" ]; then
    echo "FAIL: truncation fixture did not retain inode and shrink below watermark" >&2
    exit 1
fi
if _post_deploy_smoke_check_fail_closed_warn_rate; then
    echo "FAIL: WARN spike after in-place truncation did not trip the threshold" >&2
    exit 1
fi
if ! grep -q 'sample=5 warn_lines=5 fail_closed_warns=5 threshold=5' "$POST_DEPLOY_SMOKE_EVIDENCE"; then
    echo "FAIL: sampler did not read the full truncated post-restart log" >&2
    exit 1
fi
echo "truncate-then-WARN: $(grep 'sample=5 warn_lines=5 fail_closed_warns=5 threshold=5' "$POST_DEPLOY_SMOKE_EVIDENCE")"

POST_DEPLOY_SMOKE_FAILURES=()
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
printf '%-120s' 'stale-before-truncate-regrow' > "$POST_DEPLOY_SMOKE_LOG_PATH"
capture_log_watermark
: > "$POST_DEPLOY_SMOKE_LOG_PATH"
for index in 1 2 3 4 5; do
    printf '2026-07-14T09:18:1%sZ WARN fail-closed new-after-truncate-regrow padding-padding\n' "$index"
done >> "$POST_DEPLOY_SMOKE_LOG_PATH"
read -r current_inode current_size \
    <<< "$(_post_deploy_smoke_log_identity_and_size "$POST_DEPLOY_SMOKE_LOG_PATH")"
if [ "$POST_DEPLOY_SMOKE_LOG_OFFSET" -ne 120 ] \
  || [ "$current_inode" != "$POST_DEPLOY_SMOKE_LOG_INODE" ] \
  || [ "$current_size" -lt "$POST_DEPLOY_SMOKE_LOG_OFFSET" ]; then
    echo "FAIL: truncate-regrow fixture did not retain inode and regrow past watermark" >&2
    exit 1
fi
if _post_deploy_smoke_check_fail_closed_warn_rate; then
    echo "FAIL: WARN spike after same-inode truncate-regrow did not trip the threshold" >&2
    exit 1
fi
if ! grep -q 'sample=5 warn_lines=5 fail_closed_warns=5 threshold=5' "$POST_DEPLOY_SMOKE_EVIDENCE"; then
    echo "FAIL: fingerprint reset did not count the full truncate-regrow WARN spike" >&2
    exit 1
fi
echo "truncate-regrow-then-WARN: $(grep 'sample=5 warn_lines=5 fail_closed_warns=5 threshold=5' "$POST_DEPLOY_SMOKE_EVIDENCE")"

POST_DEPLOY_SMOKE_FAILURES=()
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
printf 'stale-before-rotation\n' > "$POST_DEPLOY_SMOKE_LOG_PATH"
capture_log_watermark
mv "$POST_DEPLOY_SMOKE_LOG_PATH" "$POST_DEPLOY_SMOKE_LOG_PATH.1"
for index in 1 2 3 4 5; do
    printf '2026-07-14T09:19:0%sZ WARN fail-closed new-after-rotation padding-padding\n' "$index"
done > "$POST_DEPLOY_SMOKE_LOG_PATH"
read -r current_inode current_size \
    <<< "$(_post_deploy_smoke_log_identity_and_size "$POST_DEPLOY_SMOKE_LOG_PATH")"
if [ "$current_inode" = "$POST_DEPLOY_SMOKE_LOG_INODE" ] \
  || [ "$current_size" -lt "$POST_DEPLOY_SMOKE_LOG_OFFSET" ]; then
    echo "FAIL: rotation fixture did not replace inode with a larger file" >&2
    exit 1
fi
if _post_deploy_smoke_check_fail_closed_warn_rate; then
    echo "FAIL: WARN spike after log rotation did not trip the threshold" >&2
    exit 1
fi
if ! grep -q 'sample=5 warn_lines=5 fail_closed_warns=5 threshold=5' "$POST_DEPLOY_SMOKE_EVIDENCE"; then
    echo "FAIL: sampler did not read the full rotated post-restart log" >&2
    exit 1
fi
echo "rotate-then-WARN: $(grep 'sample=5 warn_lines=5 fail_closed_warns=5 threshold=5' "$POST_DEPLOY_SMOKE_EVIDENCE")"

echo "deploy smoke WARN post-restart scope tests passed"
