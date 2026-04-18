#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=_defaults.sh
. "$SCRIPT_DIR/_defaults.sh"

ADK_DEV="$HOME/.adk/dev"
ADK_REL="$HOME/.adk/release"
PLIST_DEV="com.agentdesk.dev"
PLIST_REL="com.agentdesk.release"
REPO="${AGENTDESK_REPO_DIR:-}"
if [ -z "$REPO" ]; then
    REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
fi
if [ ! -d "$REPO" ]; then
    echo "✗ Repo not found: $REPO"
    exit 1
fi
REPO="$(cd "$REPO" && pwd)"
REPORT_CHANNEL_ID="${AGENTDESK_REPORT_CHANNEL_ID:-}"
REPORT_PROVIDER="${AGENTDESK_REPORT_PROVIDER:-}"
PROMOTE_DETACHED_CHILD="${AGENTDESK_PROMOTE_DETACHED_CHILD:-0}"
PROMOTE_LOG_PATH="${AGENTDESK_PROMOTE_LOG_PATH:-}"
PROMOTE_TEST_MODE="${AGENTDESK_PROMOTE_TEST_MODE:-0}"
PROMOTE_DELAY_SECS="${AGENTDESK_PROMOTE_DELAY_SECS:-2}"
PROMOTE_HEALTH_RETRIES="${AGENTDESK_PROMOTE_HEALTH_RETRIES:-20}"
PROMOTE_HEALTH_DELAY_SECS="${AGENTDESK_PROMOTE_HEALTH_DELAY_SECS:-2}"
CODESIGN_IDENTITY="${AGENTDESK_CODESIGN_IDENTITY:-Developer ID Application: Wonchang Oh (A7LJY7HNGA)}"
ALLOW_ADHOC_RELEASE_SIGN="${AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN:-0}"
DASHBOARD_SOURCE=""

SKIP_HEALTH=false
for arg in "$@"; do
    case "$arg" in
        --skip-review) ;; # accepted-and-ignored for backward compatibility
        --skip-health) SKIP_HEALTH=true ;;
    esac
done

echo "═══ ADK Promote Dev → Release ═══"

sign_binary_with_fallback() {
    local target="$1"
    local identity="${CODESIGN_IDENTITY:--}"
    local signature_details=""
    local current_authority=""

    if [ -z "$identity" ]; then
        if [ "$ALLOW_ADHOC_RELEASE_SIGN" = "1" ]; then
            echo "⚠ No signing identity configured; using explicit ad-hoc release signature override"
            identity="-"
        else
            echo "✗ No release signing identity configured"
            echo "  Set AGENTDESK_CODESIGN_IDENTITY to a valid Developer ID Application certificate"
            echo "  or set AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN=1 for an explicit local override"
            exit 1
        fi
    fi

    if [ "$identity" = "-" ] && [ "$ALLOW_ADHOC_RELEASE_SIGN" != "1" ]; then
        echo "✗ Refusing ad-hoc release signing without AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN=1"
        exit 1
    fi

    if [ -n "$identity" ] && [ "$identity" != "-" ] && command -v security >/dev/null 2>&1; then
        if ! security find-identity -v -p codesigning 2>/dev/null | grep -Fq "$identity"; then
            if [ "$ALLOW_ADHOC_RELEASE_SIGN" = "1" ]; then
                echo "⚠ Signing identity not found locally; using explicit ad-hoc release signature override"
                identity="-"
            else
                echo "✗ Signing identity not found locally: $identity"
                echo "  Refusing release promotion without a valid Developer ID Application certificate"
                echo "  Set AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN=1 only for an explicit local override"
                exit 1
            fi
        fi
    fi

    # Only preserve TCC when the staged binary already carries the exact Developer ID
    # signature. Ad-hoc signatures must always be replaced before release.
    if [ "$identity" != "-" ] && codesign -v "$target" 2>/dev/null; then
        signature_details=$(codesign -dvv "$target" 2>&1 || true)
        if printf '%s\n' "$signature_details" | grep -Eq '(^Signature=adhoc$|flags=.*\badhoc\b)'; then
            echo "▸ Existing ad-hoc signature detected — re-signing with Developer ID"
        else
            current_authority=$(printf '%s\n' "$signature_details" | grep "^Authority=" | head -1 || true)
            if printf '%s\n' "$current_authority" | grep -qF "$identity" 2>/dev/null; then
                echo "✓ Already signed with matching identity — skipping re-sign (TCC preserved)"
                return 0
            fi
        fi
    fi

    if [ "$identity" = "-" ]; then
        codesign -f -s "$identity" --identifier "com.itismyfield.agentdesk" "$target"
    else
        codesign -f -s "$identity" --options runtime --identifier "com.itismyfield.agentdesk" "$target"
    fi

    if ! codesign -v "$target" 2>/dev/null; then
        echo "✗ Codesign verification failed — aborting"
        exit 1
    fi

    if [ "$identity" != "-" ]; then
        signature_details=$(codesign -dvv "$target" 2>&1 || true)
        current_authority=$(printf '%s\n' "$signature_details" | grep "^Authority=" | head -1 || true)
        if ! printf '%s\n' "$current_authority" | grep -qF "$identity" 2>/dev/null; then
            echo "✗ Developer ID signature missing after codesign"
            printf '%s\n' "$signature_details" | grep -E '^(Authority=|Signature=|flags=)' || true
            exit 1
        fi
    fi
}

_staged_promote_binary_path() {
    mktemp "$ADK_REL/bin/agentdesk.promote.XXXXXX"
}

_notify_channel() {
    local content="$1"
    [ -n "$REPORT_CHANNEL_ID" ] || return 0

    local payload
    payload=$(printf '%s' "$content" | jq -Rs --arg source "project-agentdesk" --arg target "channel:$REPORT_CHANNEL_ID" '{target:$target, content: ., source:$source, bot:"notify"}')

    local rel_port="${AGENTDESK_REL_PORT:-$ADK_DEFAULT_PORT}"
    local dev_port="${AGENTDESK_DEV_PORT:-8799}"
    curl -sf -X POST "http://${ADK_DEFAULT_LOOPBACK}:${rel_port}/api/send" \
        -H 'Content-Type: application/json' \
        --data-binary "$payload" >/dev/null 2>&1 \
        || curl -sf -X POST "http://${ADK_DEFAULT_LOOPBACK}:${dev_port}/api/send" \
            -H 'Content-Type: application/json' \
            --data-binary "$payload" >/dev/null 2>&1 \
        || true
}

_tail_for_summary() {
    local log_path="$1"
    [ -f "$log_path" ] || return 0
    tail -n 12 "$log_path" 2>/dev/null || true
}

_resolve_dashboard_source() {
    # Dev dashboard may be a symlink (deploy-dashboard.sh dev uses ln -sfn).
    # Resolve to the real path so cp -r copies actual files, not dangling links.
    local candidate
    for candidate in "$ADK_DEV/dashboard/dist" "$REPO/dashboard/dist"; do
        if [ -d "$candidate" ]; then
            local resolved
            resolved="$(cd "$candidate" && pwd -P)"
            if [ -f "$resolved/index.html" ]; then
                printf '%s\n' "$resolved"
                return 0
            fi
        fi
    done
    return 1
}

_finalize_detached_helper() {
    local status="${1:-0}"
    [ "$PROMOTE_DETACHED_CHILD" = "1" ] || return 0
    [ -n "$REPORT_CHANNEL_ID" ] || return 0

    local content
    if [ "$status" -eq 0 ]; then
        content="✅ release promote complete"
    else
        content="❌ release promote failed (exit ${status})
log: ${PROMOTE_LOG_PATH:-n/a}"
        local summary
        summary=$(_tail_for_summary "$PROMOTE_LOG_PATH")
        if [ -n "$summary" ]; then
            content="${content}
${summary}"
        fi
    fi

    _notify_channel "$content"
}

_cleanup_on_exit() {
    local status=$?
    _finalize_detached_helper "$status"
}

trap _cleanup_on_exit EXIT

_self_hosted_release_session() {
    [ "$PROMOTE_DETACHED_CHILD" != "1" ] || return 1
    [ -n "${TMUX:-}" ] || return 1
    [ -n "$REPORT_CHANNEL_ID" ] || return 1
    [ -n "$REPORT_PROVIDER" ] || return 1
    return 0
}

_spawn_detached_helper() {
    local tasks_dir="$ADK_REL/runtime/self_hosted_promote"
    mkdir -p "$tasks_dir"

    local stamp
    stamp=$(date '+%Y%m%d-%H%M%S')
    local helper_session="ADK-promote-${REPORT_CHANNEL_ID}-${stamp}"
    local log_path="$tasks_dir/promote-release-${REPORT_PROVIDER}-${REPORT_CHANNEL_ID}-${stamp}.log"
    local helper_script="$tasks_dir/promote-release-${REPORT_PROVIDER}-${REPORT_CHANNEL_ID}-${stamp}.sh"
    local quoted_args=""
    if [ "$#" -gt 0 ]; then
        quoted_args=$(printf ' %q' "$@")
    fi

    cat > "$helper_script" <<EOF
#!/usr/bin/env bash
set -euo pipefail
exec >>$(printf '%q' "$log_path") 2>&1
sleep $(printf '%q' "$PROMOTE_DELAY_SECS")
export AGENTDESK_REPORT_CHANNEL_ID=$(printf '%q' "$REPORT_CHANNEL_ID")
export AGENTDESK_REPORT_PROVIDER=$(printf '%q' "$REPORT_PROVIDER")
export AGENTDESK_REPO_DIR=$(printf '%q' "$REPO")
export AGENTDESK_PROMOTE_DETACHED_CHILD=1
export AGENTDESK_PROMOTE_LOG_PATH=$(printf '%q' "$log_path")
export AGENTDESK_PROMOTE_TEST_MODE=$(printf '%q' "$PROMOTE_TEST_MODE")
cd $(printf '%q' "$REPO")
exec $(printf '%q' "$SCRIPT_DIR/promote-release.sh")${quoted_args}
EOF
    chmod +x "$helper_script"
    tmux new-session -d -s "$helper_session" "$helper_script"

    echo "▸ Self-hosted release promotion detected — using detached helper"
    echo "  helper tmux: $helper_session"
    echo "  helper log: $log_path"
    echo "  current turn will finish before dcserver restart; final result will be reported automatically"
}

# Safety check: dev must be healthy
DEV_PORT="${AGENTDESK_DEV_PORT:-8799}"
if [ "$SKIP_HEALTH" = true ]; then
    echo "▸ Skipping dev health check (--skip-health)"
else
    echo "▸ Waiting for dev health on :${DEV_PORT}..."
    DEV_READY=false
    if wait_for_http_service_health "$PLIST_DEV" "$DEV_PORT" "$PROMOTE_HEALTH_RETRIES" "$PROMOTE_HEALTH_DELAY_SECS" 0 1; then
        DEV_READY=true
    fi

    if [ "$DEV_READY" != true ]; then
        echo "✗ Dev is not healthy after $PROMOTE_HEALTH_RETRIES attempts — aborting promotion"
        exit 1
    fi

    if _health_json_reconcile_only "${WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON:-}"; then
        echo "▸ Dev is serving (provider reconcile in progress) — proceeding"
    else
        echo "▸ Dev is healthy — proceeding"
    fi
fi

# #743: Zero-inflight gate for create-pr dispatches on the release runtime.
# A restart during an in-flight create-pr dispatch leaves its completion
# unstamped after the new code rolls out. If the release API is unreachable
# the gate skips itself (recovery deploys must not be false-blocked).
REL_PORT="${AGENTDESK_REL_PORT:-8791}"
if ! curl -sf --max-time 3 "http://127.0.0.1:${REL_PORT}/api/health" > /dev/null 2>&1; then
    echo "▸ [gate] Release API not reachable on :${REL_PORT} — skipping zero-inflight check"
else
    gate_pending=$(curl -s --max-time 3 "http://127.0.0.1:${REL_PORT}/api/dispatches?status=pending" \
        | jq '[.dispatches[] | select(.dispatch_type=="create-pr")] | length' 2>/dev/null || echo 0)
    gate_dispatched=$(curl -s --max-time 3 "http://127.0.0.1:${REL_PORT}/api/dispatches?status=dispatched" \
        | jq '[.dispatches[] | select(.dispatch_type=="create-pr")] | length' 2>/dev/null || echo 0)
    if [ "${gate_pending:-0}" -gt 0 ] || [ "${gate_dispatched:-0}" -gt 0 ]; then
        echo "✗ [gate] ${gate_pending} pending + ${gate_dispatched} dispatched create-pr dispatches inflight on release."
        echo "  Wait for completion or cancel via API, then retry promotion."
        exit 1
    fi
    echo "▸ [gate] Zero create-pr dispatches inflight on release — proceeding."
fi

if ! DASHBOARD_SOURCE=$(_resolve_dashboard_source); then
    echo "✗ Dashboard dist not found in dev or workspace — aborting promotion"
    echo "  looked for:"
    echo "    - $ADK_DEV/dashboard/dist/index.html"
    echo "    - $REPO/dashboard/dist/index.html"
    echo "  Run 'cd $REPO/dashboard && npm run build' to generate it"
    exit 1
fi
if [ "$DASHBOARD_SOURCE" = "$REPO/dashboard/dist" ] && [ ! -f "$ADK_DEV/dashboard/dist/index.html" ]; then
    echo "▸ Dashboard source: workspace fallback ($DASHBOARD_SOURCE)"
else
    echo "▸ Dashboard source: $DASHBOARD_SOURCE"
fi
if [ ! -d "$REPO/skills" ]; then
    echo "✗ Managed skills not found in workspace — aborting promotion"
    echo "  expected: $REPO/skills"
    exit 1
fi

if _self_hosted_release_session; then
    _spawn_detached_helper "$@"
    exit 0
fi

if [ "$PROMOTE_TEST_MODE" = "1" ]; then
    echo "▸ TEST MODE: skipping release bootout/copy/bootstrap"
    echo "✓ Detached helper dry run complete"
    exit 0
fi

# Ensure release dir exists
mkdir -p "$ADK_REL"/{bin,config,data,logs}

# Rebuild dashboard so promote never ships a stale dist.
echo "▸ Building dashboard..."
(cd "$REPO/dashboard" && npm run build --silent)

# Re-resolve after fresh build (source path may have changed).
if ! DASHBOARD_SOURCE=$(_resolve_dashboard_source); then
    echo "✗ Dashboard build succeeded but dist not found — aborting"
    exit 1
fi

# Stage dashboard before stopping release so missing dist never causes downtime.
echo "▸ Staging dashboard..."
mkdir -p "$ADK_REL/dashboard"
DIST_STAGED="$ADK_REL/dashboard/dist.new"
rm -rf "$DIST_STAGED"
cp -r "$DASHBOARD_SOURCE" "$DIST_STAGED"

# Stage agent prompt files atomically (source-of-truth: workspace config/agents/)
if [ -d "$REPO/config/agents" ]; then
    echo "▸ Staging agent prompts..."
    PROMPTS_STAGED="$ADK_REL/config/agents.new"
    rm -rf "$PROMPTS_STAGED"
    mkdir -p "$PROMPTS_STAGED"
    rsync -a "$REPO/config/agents/" "$PROMPTS_STAGED/"
fi

# Stage managed skills before stopping release so skill sync never sees partial content.
echo "▸ Staging managed skills..."
SKILLS_STAGED="$ADK_REL/skills.new"
rm -rf "$SKILLS_STAGED"
mkdir -p "$SKILLS_STAGED"
rsync -a --delete "$REPO/skills/" "$SKILLS_STAGED/"

# Wait for active turns to finish before stopping the server.
# Without this, the SIGTERM interrupts mid-response, cutting off output.
# REL_PORT already assigned earlier for the zero-inflight gate.
TURN_WAIT_MAX=120
TURN_WAIT=0
_health_busy_count() {
    local health_json
    health_json=$(curl -sf "http://127.0.0.1:${REL_PORT}/api/health" 2>/dev/null) || { echo "0"; return; }
    local active finalizing queue_depth
    active=$(echo "$health_json" | grep -o '"global_active":[0-9]*' | head -1 | cut -d: -f2)
    finalizing=$(echo "$health_json" | grep -o '"global_finalizing":[0-9]*' | head -1 | cut -d: -f2)
    queue_depth=$(echo "$health_json" | grep -o '"queue_depth":[0-9]*' | head -1 | cut -d: -f2)
    echo $(( ${active:-0} + ${finalizing:-0} + ${queue_depth:-0} ))
}
BUSY=$(_health_busy_count)
if [ "${BUSY}" -gt 0 ]; then
    echo "▸ Waiting for active/finalizing turns and queued interventions to drain (${BUSY} pending)..."
    while [ "${BUSY}" -gt 0 ] && [ "$TURN_WAIT" -lt "$TURN_WAIT_MAX" ]; do
        sleep 2
        TURN_WAIT=$((TURN_WAIT + 2))
        BUSY=$(_health_busy_count)
    done
    if [ "${BUSY}" -gt 0 ]; then
        echo "  ⚠ ${BUSY} pending item(s) remain after ${TURN_WAIT_MAX}s — proceeding anyway"
    else
        echo "  ✓ Active turns and queued interventions drained (${TURN_WAIT}s)"
    fi
fi

# Stop release — wait for process to actually die (flock release)
echo "▸ Stopping release..."
LOCK_FILE="$ADK_REL/runtime/dcserver.lock"
OLD_PID=""
if [ -f "$LOCK_FILE" ]; then
    OLD_PID=$(cat "$LOCK_FILE" 2>/dev/null || true)
fi
launchctl bootout "gui/$(id -u)/$PLIST_REL" 2>/dev/null || true
if [ -n "$OLD_PID" ] && kill -0 "$OLD_PID" 2>/dev/null; then
    echo "  waiting for PID $OLD_PID to exit..."
    WAIT_SECS=0
    while kill -0 "$OLD_PID" 2>/dev/null && [ "$WAIT_SECS" -lt 15 ]; do
        sleep 1
        WAIT_SECS=$((WAIT_SECS + 1))
    done
    if kill -0 "$OLD_PID" 2>/dev/null; then
        echo "  ⚠ PID $OLD_PID did not exit after 15s — sending SIGKILL"
        kill -9 "$OLD_PID" 2>/dev/null || true
        sleep 1
    fi
    echo "  ✓ old process terminated (${WAIT_SECS}s)"
else
    sleep 2
fi

# Copy binary from dev — atomic: sign in tmp, then mv to replace inode.
# In-place codesign can corrupt the OS signing cache if it fails mid-write,
# causing SIGKILL on subsequent launches even though the binary is valid.
echo "▸ Copying binary from dev..."
STAGED_BINARY="$(_staged_promote_binary_path)"
chflags nouchg "$ADK_REL/bin/agentdesk" 2>/dev/null || true
cp "$ADK_DEV/bin/agentdesk" "$STAGED_BINARY"
chmod +x "$STAGED_BINARY"
xattr -d com.apple.provenance "$STAGED_BINARY" 2>/dev/null || true
sign_binary_with_fallback "$STAGED_BINARY"
mv -f "$STAGED_BINARY" "$ADK_REL/bin/agentdesk"
# Lock binary to prevent unsigned overwrites
chflags uchg "$ADK_REL/bin/agentdesk"

# Atomic swap: old → .old, staged → dist, cleanup
if [ ! -d "$DIST_STAGED" ]; then
    echo "⚠ Dashboard staging dir missing ($DIST_STAGED) — re-staging from source"
    cp -r "$DASHBOARD_SOURCE" "$DIST_STAGED"
fi
rm -rf "$ADK_REL/dashboard/dist.old"
if [ -d "$ADK_REL/dashboard/dist" ]; then
    mv "$ADK_REL/dashboard/dist" "$ADK_REL/dashboard/dist.old"
fi
if ! mv "$DIST_STAGED" "$ADK_REL/dashboard/dist"; then
    echo "✗ Dashboard swap failed — restoring from backup"
    [ -d "$ADK_REL/dashboard/dist.old" ] && mv "$ADK_REL/dashboard/dist.old" "$ADK_REL/dashboard/dist"
fi
rm -rf "$ADK_REL/dashboard/dist.old"

rm -rf "$ADK_REL/skills.old"
[ -d "$ADK_REL/skills" ] && mv "$ADK_REL/skills" "$ADK_REL/skills.old"
mv "$SKILLS_STAGED" "$ADK_REL/skills"
rm -rf "$ADK_REL/skills.old"

if [ -d "$PROMPTS_STAGED" ]; then
    rm -rf "$ADK_REL/config/agents.old"
    [ -d "$ADK_REL/config/agents" ] && mv "$ADK_REL/config/agents" "$ADK_REL/config/agents.old"
    mv "$PROMPTS_STAGED" "$ADK_REL/config/agents"
    rm -rf "$ADK_REL/config/agents.old"
    [ ! -e "$ADK_REL/config/agents/_shared.md" ] && ln -s _shared.prompt.md "$ADK_REL/config/agents/_shared.md" 2>/dev/null || true

    # Audit copy: one-way sync to ObsidianVault (non-blocking, failure is not fatal)
    OBSIDIAN_AGENTS="$HOME/ObsidianVault/RemoteVault/adk-config/agents"
    if [ -d "$OBSIDIAN_AGENTS" ]; then
        rsync -a --delete "$ADK_REL/config/agents/" "$OBSIDIAN_AGENTS/" 2>/dev/null \
            && echo "▸ Audit copy → ObsidianVault" \
            || echo "▸ Audit copy to ObsidianVault failed (non-fatal)"
    fi
fi

# Keep the user-facing CLI wrapper discoverable via PATH.
echo "▸ Ensuring global agentdesk CLI..."
"$SCRIPT_DIR/ensure-agentdesk-cli.sh"

# Initialize release database if it doesn't exist (never overwrite release data)
if [ ! -f "$ADK_REL/data/agentdesk.sqlite" ]; then
    echo "▸ Initializing release database from dev..."
    cp "$ADK_DEV/data/agentdesk.sqlite" "$ADK_REL/data/agentdesk.sqlite"
else
    echo "▸ Release database exists — preserving release data (skip copy)"
fi

REL_LAUNCHD_ENV_FILE="$ADK_REL/config/launchd.env"
if [ -f "$REL_LAUNCHD_ENV_FILE" ]; then
    echo "▸ Syncing release launchd env..."
    sync_launchd_plist_environment_from_file "$HOME/Library/LaunchAgents/$PLIST_REL.plist" "$REL_LAUNCHD_ENV_FILE"
fi

# Start release
echo "▸ Starting release..."
xattr -d com.apple.quarantine "$HOME/Library/LaunchAgents/$PLIST_REL.plist" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/$PLIST_REL.plist"

# Health check (server health + dashboard availability)
REL_PORT="${AGENTDESK_REL_PORT:-$ADK_DEFAULT_PORT}"
echo "▸ Waiting for release health on :${REL_PORT}..."
REL_HEALTHY=false
if wait_for_http_service_health "$PLIST_REL" "$REL_PORT" "$PROMOTE_HEALTH_RETRIES" "$PROMOTE_HEALTH_DELAY_SECS" 1 1; then
    REL_HEALTHY=true
fi

if [ "$REL_HEALTHY" != true ]; then
    echo "✗ Release health check failed after $PROMOTE_HEALTH_RETRIES attempts — check logs: $ADK_REL/logs/"
    exit 1
fi

if _health_json_reconcile_only "${WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON:-}"; then
    echo "✓ Release is serving on :${REL_PORT} (provider reconcile in progress)"
else
    echo "✓ Release is healthy on :${REL_PORT}"
fi

echo "═══ Promotion Complete ═══"
