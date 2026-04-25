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
DEPLOY_DETACHED_CHILD="${AGENTDESK_DEPLOY_DETACHED_CHILD:-0}"
DEPLOY_LOG_PATH="${AGENTDESK_DEPLOY_LOG_PATH:-}"
DEPLOY_TEST_MODE="${AGENTDESK_DEPLOY_TEST_MODE:-0}"
DEPLOY_DELAY_SECS="${AGENTDESK_DEPLOY_DELAY_SECS:-2}"
DEPLOY_HEALTH_RETRIES="${AGENTDESK_DEPLOY_HEALTH_RETRIES:-60}"
DEPLOY_HEALTH_DELAY_SECS="${AGENTDESK_DEPLOY_HEALTH_DELAY_SECS:-2}"
CODESIGN_IDENTITY="${AGENTDESK_CODESIGN_IDENTITY:-Developer ID Application: Wonchang Oh (A7LJY7HNGA)}"
ALLOW_ADHOC_RELEASE_SIGN="${AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN:-0}"
DASHBOARD_SOURCE=""
STAGED_BINARY=""

for arg in "$@"; do
    case "$arg" in
        --skip-review) ;; # accepted-and-ignored for backward compatibility
        --skip-health) ;; # accepted-and-ignored for backward compatibility
    esac
done

echo "═══ ADK Deploy → Release ═══"

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

_staged_deploy_binary_path() {
    mktemp "$ADK_REL/bin/agentdesk.deploy.XXXXXX"
}

_notify_channel() {
    local content="$1"
    [ -n "$REPORT_CHANNEL_ID" ] || return 0

    local payload
    payload=$(printf '%s' "$content" | jq -Rs --arg source "project-agentdesk" --arg target "channel:$REPORT_CHANNEL_ID" '{target:$target, content: ., source:$source, bot:"notify"}')

    local rel_port="${AGENTDESK_REL_PORT:-$ADK_DEFAULT_PORT}"
    curl -sf -X POST "http://${ADK_DEFAULT_LOOPBACK}:${rel_port}/api/send" \
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
    # Resolve to the real path so cp -r copies actual files, not dangling links.
    local candidate="$REPO/dashboard/dist"
    if [ -d "$candidate" ]; then
        local resolved
        resolved="$(cd "$candidate" && pwd -P)"
        if [ -f "$resolved/index.html" ]; then
            printf '%s\n' "$resolved"
            return 0
        fi
    fi
    return 1
}

_finalize_detached_helper() {
    local status="${1:-0}"
    [ "$DEPLOY_DETACHED_CHILD" = "1" ] || return 0
    [ -n "$REPORT_CHANNEL_ID" ] || return 0

    local content
    if [ "$status" -eq 0 ]; then
        content="✅ release deploy complete"
    else
        content="❌ release deploy failed (exit ${status})
log: ${DEPLOY_LOG_PATH:-n/a}"
        local summary
        summary=$(_tail_for_summary "$DEPLOY_LOG_PATH")
        if [ -n "$summary" ]; then
            content="${content}
${summary}"
        fi
    fi

    _notify_channel "$content"
}

_cleanup_on_exit() {
    local status=$?
    if [ -n "${STAGED_BINARY:-}" ] && [ -e "$STAGED_BINARY" ]; then
        rm -f "$STAGED_BINARY" 2>/dev/null || true
    fi
    _finalize_detached_helper "$status"
}

trap _cleanup_on_exit EXIT

_self_hosted_release_session() {
    [ "$DEPLOY_DETACHED_CHILD" != "1" ] || return 1
    [ -n "${TMUX:-}" ] || return 1
    [ -n "$REPORT_CHANNEL_ID" ] || return 1
    [ -n "$REPORT_PROVIDER" ] || return 1
    return 0
}

_spawn_detached_helper() {
    local tasks_dir="$ADK_REL/runtime/self_hosted_deploy"
    mkdir -p "$tasks_dir"

    local stamp
    stamp=$(date '+%Y%m%d-%H%M%S')
    local helper_session="ADK-deploy-${REPORT_CHANNEL_ID}-${stamp}"
    local log_path="$tasks_dir/deploy-release-${REPORT_PROVIDER}-${REPORT_CHANNEL_ID}-${stamp}.log"
    local helper_script="$tasks_dir/deploy-release-${REPORT_PROVIDER}-${REPORT_CHANNEL_ID}-${stamp}.sh"
    local quoted_args=""
    if [ "$#" -gt 0 ]; then
        quoted_args=$(printf ' %q' "$@")
    fi

    cat > "$helper_script" <<EOF
#!/usr/bin/env bash
set -euo pipefail
exec >>$(printf '%q' "$log_path") 2>&1
sleep $(printf '%q' "$DEPLOY_DELAY_SECS")
export AGENTDESK_REPORT_CHANNEL_ID=$(printf '%q' "$REPORT_CHANNEL_ID")
export AGENTDESK_REPORT_PROVIDER=$(printf '%q' "$REPORT_PROVIDER")
export AGENTDESK_REPO_DIR=$(printf '%q' "$REPO")
export AGENTDESK_DEPLOY_DETACHED_CHILD=1
export AGENTDESK_DEPLOY_LOG_PATH=$(printf '%q' "$log_path")
export AGENTDESK_DEPLOY_TEST_MODE=$(printf '%q' "$DEPLOY_TEST_MODE")
export AGENTDESK_SKIP_TURN_DRAIN=$(printf '%q' "${AGENTDESK_SKIP_TURN_DRAIN:-1}")
export AGENTDESK_CODESIGN_IDENTITY=$(printf '%q' "${AGENTDESK_CODESIGN_IDENTITY:-}")
export AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN=$(printf '%q' "${AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN:-}")
export AGENTDESK_DEPLOY_BINARY=$(printf '%q' "${AGENTDESK_DEPLOY_BINARY:-}")
cd $(printf '%q' "$REPO")
exec $(printf '%q' "$SCRIPT_DIR/deploy-release.sh")${quoted_args}
EOF
    chmod +x "$helper_script"
    tmux new-session -d -s "$helper_session" "$helper_script"

    echo "▸ Self-hosted release deploy detected — using detached helper"
    echo "  helper tmux: $helper_session"
    echo "  helper log: $log_path"
    echo "  current turn will finish before dcserver restart; final result will be reported automatically"
}

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
        echo "  Wait for completion or cancel via API, then retry deploy."
        exit 1
    fi
    echo "▸ [gate] Zero create-pr dispatches inflight on release — proceeding."
fi

if ! DASHBOARD_SOURCE=$(_resolve_dashboard_source); then
    echo "✗ Dashboard dist not found in workspace — aborting deploy"
    echo "  looked for:"
    echo "    - $REPO/dashboard/dist/index.html"
    echo "  Run 'cd $REPO/dashboard && npm run build' to generate it"
    exit 1
fi
echo "▸ Dashboard source: $DASHBOARD_SOURCE"
if [ ! -d "$REPO/skills" ]; then
    echo "✗ Managed skills not found in workspace — aborting deploy"
    echo "  expected: $REPO/skills"
    exit 1
fi

if _self_hosted_release_session; then
    _spawn_detached_helper "$@"
    exit 0
fi

if [ "$DEPLOY_TEST_MODE" = "1" ]; then
    echo "▸ TEST MODE: skipping release bootout/copy/bootstrap"
    echo "✓ Detached helper dry run complete"
    exit 0
fi

# Ensure release dir exists
mkdir -p "$ADK_REL"/{bin,config,data,logs}

export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-10G}"
if setup_sccache_env; then
    export RUSTC_WRAPPER=sccache
    echo "▸ sccache cache: $SCCACHE_DIR (size $SCCACHE_CACHE_SIZE)"
else
    echo "⚠ sccache not found in PATH; continuing without rustc wrapper"
    echo "  Install it first for faster release builds (for example: brew install sccache)"
    echo "  See docs/ci/sccache-setup.md"
    # Explicitly clear any rustc-wrapper coming from .cargo/config.toml so we
    # don't fail the build when the binary is missing.
    export RUSTC_WRAPPER=""
    export CARGO_BUILD_RUSTC_WRAPPER=""
fi

# Build the release binary from the current workspace by default so deploy
# always ships code compiled from the current HEAD. When a validated external
# artifact is provided explicitly, keep the existing override behavior.
SOURCE_BINARY="${AGENTDESK_DEPLOY_BINARY:-$REPO/target/release/agentdesk}"
if [ -z "${AGENTDESK_DEPLOY_BINARY:-}" ]; then
    echo "▸ Building release binary..."
    (cd "$REPO" && cargo build --release --bin agentdesk)
fi

# Rebuild dashboard so deploy never ships a stale dist.
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

# Stage agent prompt files atomically (source-of-truth: Obsidian vault, private).
# Agent prompts contain operator-specific content and are NOT tracked in this repo.
# See docs/source-of-truth.md.
OBSIDIAN_AGENTS_SRC="$HOME/ObsidianVault/RemoteVault/adk-config/agents"
if [ -d "$OBSIDIAN_AGENTS_SRC" ]; then
    echo "▸ Staging agent prompts from Obsidian vault..."
    PROMPTS_STAGED="$ADK_REL/config/agents.new"
    rm -rf "$PROMPTS_STAGED"
    mkdir -p "$PROMPTS_STAGED"
    rsync -a "$OBSIDIAN_AGENTS_SRC/" "$PROMPTS_STAGED/"
else
    echo "⚠ Obsidian agent prompt source missing: $OBSIDIAN_AGENTS_SRC"
    echo "  Skipping prompt staging — existing $ADK_REL/config/agents/ will be retained."
fi

# Stage managed skills before stopping release so skill sync never sees partial content.
echo "▸ Staging managed skills..."
SKILLS_STAGED="$ADK_REL/skills.new"
rm -rf "$SKILLS_STAGED"
mkdir -p "$SKILLS_STAGED"
rsync -a --delete "$REPO/skills/" "$SKILLS_STAGED/"

# Wait for active turns to finish before stopping the server.
# dcserver SIGTERM preserves turn state (#43e3cacc): tmux sessions stay alive
# and the watcher silent-reattaches after restart. What the drain gate guards
# against is mid-stream output truncation to Discord during the SIGTERM window.
# #899: the default is now AGENTDESK_SKIP_TURN_DRAIN=1 (bypass) — in practice
# every self-hosted promotion carries a live turn (the operator agent's own
# turn), so blocking on drain is a near-permanent false-negative; the brief
# stream hiccup is acceptable and #826/#896 already guarantee recovery via
# watcher silent-reattach + inflight rebind. Set AGENTDESK_SKIP_TURN_DRAIN=0
# to force the classic drain-wait when a clean restart is genuinely required.
# REL_PORT already assigned earlier for the zero-inflight gate.
if ! wait_for_live_turns_to_drain_or_fail "release" "$PLIST_REL" "$REL_PORT" 120 2; then
    exit 1
fi

# Source binary pre-flight — validate BEFORE bootout so a stale or missing
# build aborts without leaving release down.
if [ ! -x "$SOURCE_BINARY" ]; then
    echo "✗ Source binary missing or not executable: $SOURCE_BINARY"
    echo "  Run 'cargo build --release' or './scripts/build-release.sh' first."
    exit 1
fi

# Binary freshness check — reject deploying a binary built before the current HEAD.
# An older binary may miss embedded migrations (sqlx::migrate! is a compile-time
# macro) or code changes, leading to runtime migration-mismatch errors. Opt out
# with AGENTDESK_DEPLOY_SKIP_FRESHNESS=1 when intentional (e.g. bisecting, or
# when AGENTDESK_DEPLOY_BINARY points at a validated artifact from elsewhere).
if [ "${AGENTDESK_DEPLOY_SKIP_FRESHNESS:-0}" != "1" ] && [ -z "${AGENTDESK_DEPLOY_BINARY:-}" ]; then
    HEAD_EPOCH=$(git -C "$REPO" log -1 --format=%ct 2>/dev/null || echo 0)
    BIN_EPOCH=$(stat -f %m "$SOURCE_BINARY" 2>/dev/null || stat -c %Y "$SOURCE_BINARY" 2>/dev/null || echo 0)
    if [ "$BIN_EPOCH" -lt "$HEAD_EPOCH" ]; then
        HEAD_SHORT=$(git -C "$REPO" log -1 --format=%h 2>/dev/null || echo "?")
        BIN_MTIME_HUMAN=$(stat -f '%Sm' "$SOURCE_BINARY" 2>/dev/null || stat -c '%y' "$SOURCE_BINARY" 2>/dev/null || echo "?")
        HEAD_HUMAN=$(git -C "$REPO" log -1 --format='%ai' 2>/dev/null || echo "?")
        echo "✗ Binary is older than current HEAD (${HEAD_SHORT}):"
        echo "    binary mtime: ${BIN_MTIME_HUMAN}"
        echo "    HEAD commit:  ${HEAD_HUMAN}"
        echo "  Rebuild with 'cargo build --release' before deploying, or override with"
        echo "  AGENTDESK_DEPLOY_SKIP_FRESHNESS=1 when intentional."
        exit 1
    fi
fi

# Copy and sign the binary before stopping release. This keeps a missing
# certificate or failed codesign from taking down a healthy dcserver.
echo "▸ Staging signed binary from $SOURCE_BINARY..."
STAGED_BINARY="$(_staged_deploy_binary_path)"
cp "$SOURCE_BINARY" "$STAGED_BINARY"
chmod +x "$STAGED_BINARY"
xattr -d com.apple.provenance "$STAGED_BINARY" 2>/dev/null || true
sign_binary_with_fallback "$STAGED_BINARY"

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

# Promote the already signed staged binary atomically. In-place codesign can
# corrupt the OS signing cache if it fails mid-write.
echo "▸ Promoting staged binary..."
chflags nouchg "$ADK_REL/bin/agentdesk" 2>/dev/null || true
mv -f "$STAGED_BINARY" "$ADK_REL/bin/agentdesk"
STAGED_BINARY=""
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

if [ -n "${PROMPTS_STAGED:-}" ] && [ -d "$PROMPTS_STAGED" ]; then
    rm -rf "$ADK_REL/config/agents.old"
    [ -d "$ADK_REL/config/agents" ] && mv "$ADK_REL/config/agents" "$ADK_REL/config/agents.old"
    mv "$PROMPTS_STAGED" "$ADK_REL/config/agents"
    rm -rf "$ADK_REL/config/agents.old"
    [ ! -e "$ADK_REL/config/agents/_shared.md" ] && ln -s _shared.prompt.md "$ADK_REL/config/agents/_shared.md" 2>/dev/null || true
fi

# Keep the user-facing CLI wrapper discoverable via PATH.
echo "▸ Ensuring global agentdesk CLI..."
"$SCRIPT_DIR/ensure-agentdesk-cli.sh"

# Postgres database is operator-managed; SQLite copy removed after #461 cutover.

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
if wait_for_http_service_health "$PLIST_REL" "$REL_PORT" "$DEPLOY_HEALTH_RETRIES" "$DEPLOY_HEALTH_DELAY_SECS" 1 1; then
    REL_HEALTHY=true
fi

if [ "$REL_HEALTHY" != true ]; then
    echo "✗ Release health check failed after $DEPLOY_HEALTH_RETRIES attempts — check logs: $ADK_REL/logs/"
    exit 1
fi

if _health_json_field_exists "${WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON:-}" "fully_recovered" \
  && ! _health_json_field_is_true "${WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON:-}" "fully_recovered"; then
    echo "✓ Release is serving on :${REL_PORT} (startup recovery still in progress)"
elif _health_json_reconcile_only "${WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON:-}"; then
    echo "✓ Release is serving on :${REL_PORT} (provider reconcile in progress)"
else
    echo "✓ Release is healthy on :${REL_PORT}"
fi

echo "═══ Deploy Complete ═══"
