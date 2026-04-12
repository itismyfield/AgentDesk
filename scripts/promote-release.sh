#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=_defaults.sh
. "$SCRIPT_DIR/_defaults.sh"

ADK_REL="$HOME/.adk/release"
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
SOURCE_BINARY="${AGENTDESK_PROMOTE_SOURCE_BIN:-$REPO/target/release/agentdesk}"

SKIP_REVIEW=false
SKIP_HEALTH=false
for arg in "$@"; do
    case "$arg" in
        --skip-review) SKIP_REVIEW=true ;;
        --skip-health) SKIP_HEALTH=true ;;
    esac
done

echo "═══ ADK Promote Workspace → Release ═══"

sign_binary_with_fallback() {
    local target="$1"
    local identity="${CODESIGN_IDENTITY:--}"

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

    # Skip re-sign if already signed with the same identity.
    if [ "$identity" != "-" ] && codesign -v "$target" 2>/dev/null; then
        local current_authority
        current_authority=$(codesign -dvv "$target" 2>&1 | grep "^Authority=" | head -1 || true)
        if echo "$current_authority" | grep -qF "$identity" 2>/dev/null; then
            echo "✓ Already signed with matching identity — skipping re-sign (TCC preserved)"
            return 0
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
}

_notify_channel() {
    local content="$1"
    [ -n "$REPORT_CHANNEL_ID" ] || return 0

    local payload
    payload=$(printf '%s' "$content" | jq -Rs --arg source "project-agentdesk" --arg target "channel:$REPORT_CHANNEL_ID" '{target:$target, content: ., source:$source, bot:"notify"}')

    local rel_port="${AGENTDESK_REL_PORT:-$ADK_DEFAULT_PORT}"
    if curl -sf -X POST "http://${ADK_DEFAULT_LOOPBACK}:${rel_port}/api/send" \
        -H 'Content-Type: application/json' \
        --data-binary "$payload" >/dev/null 2>&1; then
        return 0
    fi

    case "$REPORT_CHANNEL_ID" in
        ''|*[!0-9]*) return 0 ;;
    esac

    local agentdesk_cli="$ADK_REL/bin/agentdesk"
    if [ ! -x "$agentdesk_cli" ] && command -v agentdesk >/dev/null 2>&1; then
        agentdesk_cli="$(command -v agentdesk)"
    fi
    [ -x "$agentdesk_cli" ] || return 0

    "$agentdesk_cli" discord-sendmessage \
        --channel "$REPORT_CHANNEL_ID" \
        --message "$content" >/dev/null 2>&1 \
        || true
}

_tail_for_summary() {
    local log_path="$1"
    [ -f "$log_path" ] || return 0
    tail -n 12 "$log_path" 2>/dev/null || true
}

_resolve_dashboard_source() {
    # Prefer the workspace build output. Fall back to the currently installed
    # release dashboard only when the workspace did not build static assets.
    local candidate
    for candidate in "$REPO/dashboard/dist" "$ADK_REL/dashboard/dist"; do
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

_read_kv_flag() {
    local db_path="$1"
    local key="$2"
    [ -f "$db_path" ] || return 1
    /usr/bin/sqlite3 -readonly "$db_path" \
        "SELECT value FROM kv_meta WHERE key = '$key' LIMIT 1;" 2>/dev/null || true
}

_normalize_bool() {
    printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]' | tr -d '"'
}

_review_gate_override_source() {
    local db_path="$ADK_REL/data/agentdesk.sqlite"
    local raw normalized

    raw=$(_read_kv_flag "$db_path" "review_enabled")
    normalized=$(_normalize_bool "$raw")
    if [ "$normalized" = "false" ]; then
        printf '%s\t%s\t%s\n' "release" "review_enabled" "$db_path"
        return 0
    fi

    raw=$(_read_kv_flag "$db_path" "counter_model_review_enabled")
    normalized=$(_normalize_bool "$raw")
    if [ "$normalized" = "false" ]; then
        printf '%s\t%s\t%s\n' "release" "counter_model_review_enabled" "$db_path"
        return 0
    fi

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
export AGENTDESK_CODESIGN_IDENTITY=$(printf '%q' "$CODESIGN_IDENTITY")
export AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN=$(printf '%q' "$ALLOW_ADHOC_RELEASE_SIGN")
export AGENTDESK_PROMOTE_SOURCE_BIN=$(printf '%q' "$SOURCE_BINARY")
export AGENTDESK_REL_PORT=$(printf '%q' "${AGENTDESK_REL_PORT:-}")
export AGENTDESK_PROMOTE_HEALTH_RETRIES=$(printf '%q' "$PROMOTE_HEALTH_RETRIES")
export AGENTDESK_PROMOTE_HEALTH_DELAY_SECS=$(printf '%q' "$PROMOTE_HEALTH_DELAY_SECS")
cd $(printf '%q' "$REPO")
exec $(printf '%q' "$SCRIPT_DIR/promote-release.sh")${quoted_args}
EOF
    chmod +x "$helper_script"
    tmux new-session -d -s "$helper_session" "$helper_script"

    echo "▸ Self-hosted release promotion detected — using detached helper"
    echo "  helper tmux: $helper_session"
    echo "  helper log: $log_path"
    echo "  current turn will finish before dcserver restart; final result will be reported when available"
    echo "  if release stays down, inspect the helper log above"
}

# Safety check: review must be passed unless review automation is disabled
# in runtime config, or unless --skip-review is passed explicitly.
if [ "$SKIP_REVIEW" != true ]; then
    REVIEW_OVERRIDE=$(_review_gate_override_source || true)
    if [ -n "$REVIEW_OVERRIDE" ]; then
        IFS=$'\t' read -r REVIEW_OVERRIDE_RUNTIME REVIEW_OVERRIDE_KEY REVIEW_OVERRIDE_DB <<<"$REVIEW_OVERRIDE"
        echo "▸ Review automation disabled in ${REVIEW_OVERRIDE_RUNTIME} runtime (${REVIEW_OVERRIDE_KEY}=false)"
        echo "  bypassing review gate using $REVIEW_OVERRIDE_DB"
    else
        # Check if the latest commit has a review-passed marker in the release runtime.
        LAST_COMMIT=$(cd "$REPO" && git rev-parse HEAD 2>/dev/null)
        REVIEW_MARKER_REL="$ADK_REL/runtime/review_passed/$LAST_COMMIT"
        if [ ! -f "$REVIEW_MARKER_REL" ]; then
            echo "✗ Review not passed for commit $LAST_COMMIT — aborting promotion"
            echo "  Run review first, or use --skip-review to override"
            exit 1
        fi
        echo "▸ Review passed for $LAST_COMMIT"
    fi
fi

# Safety check: the workspace release build must exist.
if [ ! -x "$SOURCE_BINARY" ]; then
    echo "✗ Release source binary not found: $SOURCE_BINARY"
    echo "  Build the workspace first, for example:"
    echo "    cd $REPO && ./scripts/build-release.sh"
    exit 1
fi
echo "▸ Release source binary: $SOURCE_BINARY"

if [ "$SKIP_HEALTH" = true ]; then
    echo "▸ --skip-health is ignored in release-only promotion mode"
fi

if ! DASHBOARD_SOURCE=$(_resolve_dashboard_source); then
    echo "✗ Dashboard dist not found in workspace or current release — aborting promotion"
    echo "  looked for:"
    echo "    - $REPO/dashboard/dist/index.html"
    echo "    - $ADK_REL/dashboard/dist/index.html"
    echo "  Run 'cd $REPO/dashboard && npm run build' to generate it"
    exit 1
fi
if [ "$DASHBOARD_SOURCE" = "$REPO/dashboard/dist" ]; then
    echo "▸ Dashboard source: workspace build ($DASHBOARD_SOURCE)"
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

# Stage dashboard before stopping release so missing dist never causes downtime.
echo "▸ Staging dashboard..."
mkdir -p "$ADK_REL/dashboard"
DIST_STAGED="$ADK_REL/dashboard/dist.new"
rm -rf "$DIST_STAGED"
cp -r "$DASHBOARD_SOURCE" "$DIST_STAGED"

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

REL_PORT="${AGENTDESK_REL_PORT:-8791}"
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

# Copy binary from workspace build — atomic: sign in tmp, then mv to replace inode.
# In-place codesign can corrupt the OS signing cache if it fails mid-write,
# causing SIGKILL on subsequent launches even though the binary is valid.
echo "▸ Copying binary from workspace build..."
chflags nouchg "$ADK_REL/bin/agentdesk" 2>/dev/null || true
cp "$SOURCE_BINARY" "$ADK_REL/bin/agentdesk.new"
chmod +x "$ADK_REL/bin/agentdesk.new"
xattr -d com.apple.provenance "$ADK_REL/bin/agentdesk.new" 2>/dev/null || true
xattr -d com.apple.quarantine "$ADK_REL/bin/agentdesk.new" 2>/dev/null || true
sign_binary_with_fallback "$ADK_REL/bin/agentdesk.new"
mv -f "$ADK_REL/bin/agentdesk.new" "$ADK_REL/bin/agentdesk"
# Lock binary to prevent unsigned overwrites
chflags uchg "$ADK_REL/bin/agentdesk"

# Atomic swap: old → .old, staged → dist, cleanup
rm -rf "$ADK_REL/dashboard/dist.old"
[ -d "$ADK_REL/dashboard/dist" ] && mv "$ADK_REL/dashboard/dist" "$ADK_REL/dashboard/dist.old"
mv "$DIST_STAGED" "$ADK_REL/dashboard/dist"
rm -rf "$ADK_REL/dashboard/dist.old"

rm -rf "$ADK_REL/skills.old"
[ -d "$ADK_REL/skills" ] && mv "$ADK_REL/skills" "$ADK_REL/skills.old"
mv "$SKILLS_STAGED" "$ADK_REL/skills"
rm -rf "$ADK_REL/skills.old"

if [ -d "${PROMPTS_STAGED:-}" ]; then
    rm -rf "$ADK_REL/config/agents.old"
    [ -d "$ADK_REL/config/agents" ] && mv "$ADK_REL/config/agents" "$ADK_REL/config/agents.old"
    mv "$PROMPTS_STAGED" "$ADK_REL/config/agents"
    rm -rf "$ADK_REL/config/agents.old"
    [ ! -e "$ADK_REL/config/agents/_shared.md" ] && ln -s _shared.prompt.md "$ADK_REL/config/agents/_shared.md" 2>/dev/null || true
fi

# Keep the user-facing CLI wrapper discoverable via PATH.
echo "▸ Ensuring global agentdesk CLI..."
"$SCRIPT_DIR/ensure-agentdesk-cli.sh"

# Initialize release database lazily on first boot.
if [ ! -f "$ADK_REL/data/agentdesk.sqlite" ]; then
    echo "▸ Release database not found — runtime will create a fresh database on startup"
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
launchctl enable "gui/$(id -u)/$PLIST_REL" 2>/dev/null || true
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
