#!/usr/bin/env bash
set -euo pipefail

# ENV (operator-overridable; defaults preserve current behavior):
#   AGENTDESK_BUNDLE_ID         codesign --identifier value (default: com.itismyfield.agentdesk)
#   AGENTDESK_DCSERVER_LABEL    release launchd plist Label / file basename.
#                               Read by the Rust dcserver as well — use this to keep
#                               launchd label and plist filename in sync across both sides.
#                               (default: com.agentdesk.release)
#   AGENTDESK_PLIST_REL         Deprecated alias for AGENTDESK_DCSERVER_LABEL; honored as
#                               fallback when AGENTDESK_DCSERVER_LABEL is unset.
#   OBSIDIAN_VAULT_ROOT         Obsidian vault root used for agent prompt staging
#                               (default: $HOME/ObsidianVault; full source path is
#                               $OBSIDIAN_VAULT_ROOT/RemoteVault/adk-config/agents)
#   AGENTDESK_OBSIDIAN_AGENTS_SRC
#                               Full override for the agent prompt source directory.
#                               Takes precedence over OBSIDIAN_VAULT_ROOT when set.
# Additional AGENTDESK_* env vars (codesign, lock, peers, freshness, …) are
# defined inline below — search for "${AGENTDESK_" to enumerate them.
# Source safety overrides:
#   AGENTDESK_DEPLOY_ALLOW_NON_MAIN=1  allow deploying a HEAD that is not
#                                      exactly origin/main.
#   AGENTDESK_DEPLOY_ALLOW_DIRTY=1     allow deploying with local changes.
#   AGENTDESK_DEPLOY_SKIP_FRESHNESS=1  skip both source-identity and remote
#                                      freshness gates for an intentional
#                                      offline/emergency deploy.
#   AGENTDESK_DEPLOY_FAST=1            opt into the release-fast Cargo profile
#                                      for lower-latency dev-loop deploys.
# Resource-contention pre-flight (#4255 — runs on every node before the build):
#   AGENTDESK_DEPLOY_MAX_LOADAVG           1-min load-average ceiling; over it the
#                                          deploy refuses. Default: 1.5 × logical
#                                          CPU count (e.g. 21.0 on a 14-core box).
#                                          The load probe is SKIPPED (fail-open) if
#                                          the CPU count is unreadable and no
#                                          explicit ceiling is set.
#   AGENTDESK_DEPLOY_MAX_MEM_PRESSURE_LEVEL macOS memory-pressure ceiling
#                                          (kern.memorystatus_vm_pressure_level:
#                                          1=normal 2=warn 4=critical). Refuse when
#                                          the level is >= this. Default: 4.
#   AGENTDESK_DEPLOY_HIGH_CPU_PCT           ps %CPU at/above which a non-deploy
#                                          process (own process group excluded) is
#                                          flagged by pid/name. Default: 90.
#   AGENTDESK_DEPLOY_RUNAWAY_CPU_RATIO      a flagged process refuses ON ITS OWN
#                                          (no corroboration) when it is a SUSTAINED
#                                          runaway: cumulative-CPU / elapsed >= this
#                                          ratio (the 07-07 zombie-ugrep shape, a
#                                          single core never moves loadavg on a
#                                          many-core box). Default: 0.8. Otherwise a
#                                          lone hot process is advisory unless
#                                          corroborated by load-over-ceiling or
#                                          memory pressure at/above the block level.
#   AGENTDESK_DEPLOY_RUNAWAY_MIN_ELAPSED    seconds a process must have lived before
#                                          the runaway rule applies — spares a fresh
#                                          legitimate burst (a rust-analyzer reindex
#                                          begun 90 s ago has ratio ~1 but is not a
#                                          zombie). Default: 600.
#   AGENTDESK_DEPLOY_FORCE_RESOURCE_PREFLIGHT=1
#                                          escape hatch — proceed past a failed
#                                          resource pre-flight (findings are still
#                                          printed, downgraded to warnings).
# Post-deploy functional smoke (#4262 — always fail-open after DEPLOY_OK):
#   AGENTDESK_POST_DEPLOY_SMOKE_RELAY_CELL  configured TUI E2E cell for the
#                                          single E-1 relay round-trip
#                                          (default: claude-tui).
#   AGENTDESK_POST_DEPLOY_SMOKE_LOG_LINES   recent dcserver log sample size
#                                          (default: 500 lines).
#   AGENTDESK_POST_DEPLOY_SMOKE_WARN_LIMIT  fail-closed WARN count considered
#                                          abnormal (default: 5 in the bounded
#                                          sample; one WARN never flags).
#   AGENTDESK_POST_DEPLOY_SMOKE_CREATE_ISSUE
#                                          default: off. Set to literal
#                                          "confirmed" only when an operator
#                                          accepts live issue creation for a
#                                          confirmed regression. Failures always
#                                          write a local draft regardless.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=_defaults.sh
. "$SCRIPT_DIR/_defaults.sh"

ADK_REL="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"
# The Rust dcserver reads AGENTDESK_DCSERVER_LABEL for the plist Label; honor it first
# so launchd Label and plist filename never diverge when the operator overrides one side.
PLIST_REL="${AGENTDESK_DCSERVER_LABEL:-${AGENTDESK_PLIST_REL:-com.agentdesk.release}}"
BUNDLE_ID="${AGENTDESK_BUNDLE_ID:-com.itismyfield.agentdesk}"
REL_LAUNCHD_ENV_FILE="$ADK_REL/config/launchd.env"
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
DEPLOY_LOCK_FILE="${AGENTDESK_DEPLOY_LOCK_FILE:-$ADK_REL/runtime/deploy-release.lock}"
DEPLOY_LOCK_TIMEOUT_SECS="${AGENTDESK_DEPLOY_LOCK_TIMEOUT_SECS:-1800}"
DEPLOY_MKDIR_LOCK_DIR=""
CODESIGN_IDENTITY="${AGENTDESK_CODESIGN_IDENTITY:-Developer ID Application: Wonchang Oh (A7LJY7HNGA)}"
ALLOW_ADHOC_RELEASE_SIGN="${AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN:-0}"
CODESIGN_KEYCHAIN_PW_FILE="${AGENTDESK_CODESIGN_KEYCHAIN_PW_FILE:-}"
CODESIGN_KEYCHAIN_NAME="${AGENTDESK_CODESIGN_KEYCHAIN_NAME:-agentdesk-codesign.keychain}"
CODESIGN_KEYCHAIN_UNLOCKED=0
RESOLVED_RELEASE_SIGNING_MODE=""
DASHBOARD_SOURCE=""
STAGED_BINARY=""
POLICIES_STAGED=""
LAUNCHD_MIGRATED_STAGED=""
RELEASE_ROOT_SCRIPTS_STAGED=""
DEPLOY_ALL_NODES="${AGENTDESK_DEPLOY_ALL_NODES:-0}"
DEPLOY_PEERS_OVERRIDE=()
DEPLOY_PEERS_FILE="${AGENTDESK_DEPLOY_PEERS_FILE:-$ADK_REL/config/deploy-peers.txt}"
DEPLOY_PEER_INVOCATION="${AGENTDESK_DEPLOY_PEER_INVOCATION:-0}"
DEPLOY_FAST="${AGENTDESK_DEPLOY_FAST:-0}"
# #4348 Defect 3: bound the peer SSH connection phase so an unreachable mDNS
# alias (e.g. mac-book.local not resolving) fails fast instead of hanging the
# whole cluster deploy. Only the connect is bounded; a reachable peer's long
# remote build is unaffected.
DEPLOY_SSH_CONNECT_TIMEOUT="${AGENTDESK_DEPLOY_SSH_CONNECT_TIMEOUT:-10}"

# Parse flags non-destructively into shell vars + env so that the lock-acquire
# re-exec (lockf/flock pass-through) and the detached-helper tmux script both
# see the same configuration without us having to reconstruct $@.
PARSED_ARGS=()
_idx=0
_args=("$@")
while [ "$_idx" -lt "${#_args[@]}" ]; do
    case "${_args[$_idx]}" in
        --skip-review|--skip-health)
            PARSED_ARGS+=("${_args[$_idx]}") ;;
        --fast)
            DEPLOY_FAST=1
            export AGENTDESK_DEPLOY_FAST=1
            ;;
        --all-nodes|--cluster)
            DEPLOY_ALL_NODES=1
            export AGENTDESK_DEPLOY_ALL_NODES=1
            ;;
        --peer)
            _idx=$((_idx + 1))
            [ "$_idx" -lt "${#_args[@]}" ] || { echo "✗ --peer requires a value"; exit 2; }
            DEPLOY_PEERS_OVERRIDE+=("${_args[$_idx]}")
            if [ -n "${AGENTDESK_DEPLOY_PEERS:-}" ]; then
                AGENTDESK_DEPLOY_PEERS="${AGENTDESK_DEPLOY_PEERS},${_args[$_idx]}"
            else
                AGENTDESK_DEPLOY_PEERS="${_args[$_idx]}"
            fi
            export AGENTDESK_DEPLOY_PEERS
            ;;
        *)
            PARSED_ARGS+=("${_args[$_idx]}") ;;
    esac
    _idx=$((_idx + 1))
done
unset _idx _args
if [ "${#PARSED_ARGS[@]}" -gt 0 ]; then
    set -- "${PARSED_ARGS[@]}"
else
    set --
fi

case "$DEPLOY_FAST" in
    1|true|TRUE|yes|YES) DEPLOY_FAST=1 ;;
    *) DEPLOY_FAST=0 ;;
esac
DEPLOY_BUILD_PROFILE="release"
if [ "$DEPLOY_FAST" = "1" ]; then
    DEPLOY_BUILD_PROFILE="release-fast"
    export AGENTDESK_DEPLOY_FAST=1
fi

if [ "${AGENTDESK_DEPLOY_LOCK_HELD:-0}" != "1" ]; then
    echo "═══ ADK Deploy → Release ═══"
fi

_unlock_codesign_keychain_if_configured() {
    [ "$CODESIGN_KEYCHAIN_UNLOCKED" = "1" ] && return 0
    [ -n "$CODESIGN_KEYCHAIN_PW_FILE" ] || return 0
    if [ ! -r "$CODESIGN_KEYCHAIN_PW_FILE" ]; then
        echo "⚠ Codesign keychain pw file not readable: $CODESIGN_KEYCHAIN_PW_FILE — continuing without explicit unlock"
        return 0
    fi
    command -v security >/dev/null 2>&1 || return 0
    local pw
    if ! pw=$(cat "$CODESIGN_KEYCHAIN_PW_FILE"); then
        echo "⚠ Failed to read codesign keychain pw file"
        return 0
    fi
    if security unlock-keychain -p "$pw" "$CODESIGN_KEYCHAIN_NAME" 2>/dev/null; then
        echo "▸ Unlocked codesign keychain: $CODESIGN_KEYCHAIN_NAME"
        CODESIGN_KEYCHAIN_UNLOCKED=1
    else
        echo "⚠ Failed to unlock codesign keychain $CODESIGN_KEYCHAIN_NAME — codesign may fail in non-GUI sessions"
    fi
    unset pw
}

sign_binary_with_fallback() {
    local target="$1"
    local identity="${CODESIGN_IDENTITY:--}"
    local signature_details=""
    local current_authority=""

    _unlock_codesign_keychain_if_configured

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
            current_identifier=$(printf '%s\n' "$signature_details" | grep "^Identifier=" | head -1 || true)
            identifier_matches=0
            if [ -n "$current_identifier" ] && printf '%s\n' "$current_identifier" | grep -qF "=$BUNDLE_ID" 2>/dev/null; then
                identifier_matches=1
            fi
            if printf '%s\n' "$current_authority" | grep -qF "$identity" 2>/dev/null && [ "$identifier_matches" = "1" ]; then
                RESOLVED_RELEASE_SIGNING_MODE="developer-id"
                echo "✓ Already signed with matching identity and identifier — skipping re-sign (TCC preserved)"
                return 0
            fi
        fi
    fi

    if [ "$identity" = "-" ]; then
        RESOLVED_RELEASE_SIGNING_MODE="adhoc"
        codesign -f -s "$identity" --identifier "$BUNDLE_ID" "$target"
    else
        RESOLVED_RELEASE_SIGNING_MODE="developer-id"
        codesign -f -s "$identity" --options runtime --identifier "$BUNDLE_ID" "$target"
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

start_release_tmux_fallback() {
    local session="${AGENTDESK_RELEASE_TMUX_SESSION:-AgentDesk-dcserver-release-manual}"
    echo "▸ Starting release via tmux fallback: $session"
    tmux kill-session -t "$session" 2>/dev/null || true
    tmux new-session -d -s "$session" -c "$ADK_REL" \
        "ulimit -n 4096; set -a; [ -f '$REL_LAUNCHD_ENV_FILE' ] && . '$REL_LAUNCHD_ENV_FILE'; set +a; export AGENTDESK_ROOT_DIR='$ADK_REL'; echo '[agentdesk-tmux-fallback] ulimit -n='\"\$(ulimit -n)\" >&2; exec '$ADK_REL/bin/agentdesk' dcserver"
}

_staged_deploy_binary_path() {
    mktemp "$ADK_REL/bin/agentdesk.deploy.XXXXXX"
}

_resolve_release_server_port() {
    local fallback_port="${AGENTDESK_REL_PORT:-$ADK_DEFAULT_PORT}"
    local config_path=""
    local configured_port

    if [ -n "${AGENTDESK_CONFIG:-}" ] && [ -f "$AGENTDESK_CONFIG" ]; then
        config_path="$AGENTDESK_CONFIG"
    elif [ -f "$ADK_REL/config/agentdesk.yaml" ]; then
        config_path="$ADK_REL/config/agentdesk.yaml"
    elif [ -f "$ADK_REL/agentdesk.yaml" ]; then
        config_path="$ADK_REL/agentdesk.yaml"
    fi

    if [ -z "$config_path" ]; then
        printf '%s\n' "$fallback_port"
        return 0
    fi

    if configured_port=$(python3 - "$config_path" "$fallback_port" <<'PY'
import sys

import yaml

path, fallback = sys.argv[1:]
with open(path, encoding="utf-8") as handle:
    config = yaml.safe_load(handle)
server = config.get("server") if isinstance(config, dict) else None
port = server.get("port", fallback) if isinstance(server, dict) else fallback
if isinstance(port, bool) or not isinstance(port, (int, str)):
    raise ValueError("server.port must be an integer")
port = int(port)
if not 1 <= port <= 65535:
    raise ValueError("server.port must be between 1 and 65535")
print(port)
PY
    ); then
        printf '%s\n' "$configured_port"
        return 0
    fi

    echo "⚠ Could not resolve server.port from $config_path — using :$fallback_port" >&2
    printf '%s\n' "$fallback_port"
}

_notify_channel() {
    local content="$1"
    [ -n "$REPORT_CHANNEL_ID" ] || return 0

    local payload
    payload=$(printf '%s' "$content" | jq -Rs --arg source "project-agentdesk" --arg target "channel:$REPORT_CHANNEL_ID" '{target:$target, content: ., source:$source, bot:"notify"}')

    local rel_port
    rel_port="${REL_PORT:-$(_resolve_release_server_port)}"
    curl -sf -X POST "http://${ADK_DEFAULT_LOOPBACK}:${rel_port}/api/discord/send" \
        -H "Origin: http://${ADK_DEFAULT_LOOPBACK}:${rel_port}" \
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

_ensure_dashboard_dependencies() {
    local dashboard_dir="$REPO/dashboard"
    [ -d "$dashboard_dir" ] || return 0

    if ! command -v node >/dev/null 2>&1; then
        echo "✗ node is required to build dashboard before deploy"
        exit 1
    fi
    if ! command -v npm >/dev/null 2>&1; then
        echo "✗ npm is required to build dashboard before deploy"
        exit 1
    fi
    if [ ! -f "$dashboard_dir/package-lock.json" ]; then
        echo "✗ dashboard/package-lock.json missing — cannot install deterministic dashboard dependencies"
        exit 1
    fi

    if [ ! -x "$dashboard_dir/node_modules/.bin/tsc" ]; then
        echo "▸ Installing dashboard dependencies (npm ci)..."
        (cd "$dashboard_dir" && npm ci --no-audit --no-fund)
    fi
}

_resolve_default_release_binary() {
    local profile_dir="${1:-release}"
    local target_dir
    target_dir="$(cd "$REPO" && cargo metadata --format-version 1 --no-deps 2>/dev/null | jq -r '.target_directory // empty' 2>/dev/null || true)"
    if [ -z "$target_dir" ]; then
        target_dir="${CARGO_TARGET_DIR:-$REPO/target}"
    fi
    case "$target_dir" in
        /*) ;;
        *) target_dir="$REPO/$target_dir" ;;
    esac
    printf '%s/%s/agentdesk\n' "$target_dir" "$profile_dir"
}

_latest_postgres_migration_path() {
    local migrations_dir="$REPO/migrations/postgres"
    if [ ! -d "$migrations_dir" ]; then
        return 0
    fi
    find "$migrations_dir" -maxdepth 1 -type f -name '[0-9][0-9][0-9][0-9]_*.sql' 2>/dev/null \
        | sort \
        | tail -n 1
}

_sha256_file() {
    local path="$1"
    if [ -z "$path" ] || [ ! -f "$path" ]; then
        return 0
    fi
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$path" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$path" | awk '{print $1}'
    fi
}

_write_release_source_manifest() {
    mkdir -p "$ADK_REL/runtime"

    local manifest_tmp="$ADK_REL/runtime/release-source.json.new"
    local manifest_path="$ADK_REL/runtime/release-source.json"
    local generated_at repo_head repo_branch repo_upstream repo_upstream_sha repo_dirty latest_migration latest_migration_name latest_migration_sha

    generated_at="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    repo_head="$(git -C "$REPO" rev-parse HEAD 2>/dev/null || true)"
    repo_branch="$(git -C "$REPO" rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
    repo_upstream="$(git -C "$REPO" rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' 2>/dev/null || true)"
    repo_upstream_sha=""
    if [ -n "$repo_upstream" ]; then
        repo_upstream_sha="$(git -C "$REPO" rev-parse "$repo_upstream" 2>/dev/null || true)"
    fi
    repo_dirty="unknown"
    if git -C "$REPO" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        if [ -n "$(git -C "$REPO" status --porcelain 2>/dev/null)" ]; then
            repo_dirty="true"
        else
            repo_dirty="false"
        fi
    fi

    latest_migration="$(_latest_postgres_migration_path)"
    latest_migration_name=""
    latest_migration_sha=""
    if [ -n "$latest_migration" ]; then
        latest_migration_name="$(basename "$latest_migration")"
        latest_migration_sha="$(_sha256_file "$latest_migration")"
    fi

    AGENTDESK_MANIFEST_GENERATED_AT="$generated_at" \
    AGENTDESK_MANIFEST_REPO="$REPO" \
    AGENTDESK_MANIFEST_REPO_BRANCH="$repo_branch" \
    AGENTDESK_MANIFEST_REPO_HEAD="$repo_head" \
    AGENTDESK_MANIFEST_REPO_UPSTREAM="$repo_upstream" \
    AGENTDESK_MANIFEST_REPO_UPSTREAM_SHA="$repo_upstream_sha" \
    AGENTDESK_MANIFEST_REPO_DIRTY="$repo_dirty" \
    AGENTDESK_MANIFEST_SOURCE_BINARY="${SOURCE_BINARY:-}" \
    AGENTDESK_MANIFEST_BUILD_PROFILE="$DEPLOY_BUILD_PROFILE" \
    AGENTDESK_MANIFEST_LATEST_MIGRATION="$latest_migration_name" \
    AGENTDESK_MANIFEST_LATEST_MIGRATION_SHA="$latest_migration_sha" \
    AGENTDESK_MANIFEST_SIGNING_MODE="${RESOLVED_RELEASE_SIGNING_MODE:-unknown}" \
    AGENTDESK_MANIFEST_CODESIGN_IDENTITY="$CODESIGN_IDENTITY" \
    AGENTDESK_MANIFEST_ALLOW_ADHOC_RELEASE_SIGN="$ALLOW_ADHOC_RELEASE_SIGN" \
    AGENTDESK_MANIFEST_SKIP_TURN_DRAIN="${AGENTDESK_SKIP_TURN_DRAIN:-1}" \
    AGENTDESK_MANIFEST_SKIP_FRESHNESS="${AGENTDESK_DEPLOY_SKIP_FRESHNESS:-0}" \
    AGENTDESK_MANIFEST_SKIP_REMOTE_FRESHNESS="${AGENTDESK_DEPLOY_SKIP_REMOTE_FRESHNESS:-0}" \
    python3 - "$manifest_tmp" <<PY
import json
import os
import sys

path = sys.argv[1]
payload = {
    "generated_at": os.environ.get("AGENTDESK_MANIFEST_GENERATED_AT", ""),
    "repo_path": os.environ.get("AGENTDESK_MANIFEST_REPO", ""),
    "repo_branch": os.environ.get("AGENTDESK_MANIFEST_REPO_BRANCH", ""),
    "repo_head": os.environ.get("AGENTDESK_MANIFEST_REPO_HEAD", ""),
    "repo_upstream": os.environ.get("AGENTDESK_MANIFEST_REPO_UPSTREAM", ""),
    "repo_upstream_sha": os.environ.get("AGENTDESK_MANIFEST_REPO_UPSTREAM_SHA", ""),
    "repo_dirty": os.environ.get("AGENTDESK_MANIFEST_REPO_DIRTY", "unknown"),
    "source_binary": os.environ.get("AGENTDESK_MANIFEST_SOURCE_BINARY", ""),
    "build_profile": os.environ.get("AGENTDESK_MANIFEST_BUILD_PROFILE", ""),
    "latest_postgres_migration": os.environ.get("AGENTDESK_MANIFEST_LATEST_MIGRATION", ""),
    "latest_postgres_migration_sha256": os.environ.get("AGENTDESK_MANIFEST_LATEST_MIGRATION_SHA", ""),
    "signing_mode": os.environ.get("AGENTDESK_MANIFEST_SIGNING_MODE", ""),
    "codesign_identity": os.environ.get("AGENTDESK_MANIFEST_CODESIGN_IDENTITY", ""),
    "allow_adhoc_release_sign": os.environ.get("AGENTDESK_MANIFEST_ALLOW_ADHOC_RELEASE_SIGN", ""),
    "skip_turn_drain": os.environ.get("AGENTDESK_MANIFEST_SKIP_TURN_DRAIN", "1"),
    "skip_freshness": os.environ.get("AGENTDESK_MANIFEST_SKIP_FRESHNESS", "0"),
    "skip_remote_freshness": os.environ.get("AGENTDESK_MANIFEST_SKIP_REMOTE_FRESHNESS", "0"),
}
with open(path, "w", encoding="utf-8") as handle:
    json.dump(payload, handle, ensure_ascii=False, indent=2, sort_keys=True)
    handle.write("\n")
PY
    mv -f "$manifest_tmp" "$manifest_path"
    echo "▸ Release source manifest: $manifest_path"
}

_clean_release_build_cache_after_staging() {
    [ "${AGENTDESK_DEPLOY_SKIP_BUILD_CACHE_CLEANUP:-0}" != "1" ] || return 0
    [ -z "${AGENTDESK_DEPLOY_BINARY:-}" ] || return 0

    local -a clean_cmd
    echo "▸ Cleaning ${DEPLOY_BUILD_PROFILE} build cache after staging binary..."
    if [ "$DEPLOY_BUILD_PROFILE" = "release" ]; then
        clean_cmd=(cargo clean --release)
    else
        clean_cmd=(cargo clean --profile "$DEPLOY_BUILD_PROFILE")
    fi
    if (cd "$REPO" && "${clean_cmd[@]}"); then
        echo "  ✓ ${DEPLOY_BUILD_PROFILE} build cache cleaned"
    else
        echo "⚠ cargo clean for ${DEPLOY_BUILD_PROFILE} failed; continuing with staged release artifact"
    fi
}

_check_repo_remote_freshness() {
    [ "${AGENTDESK_DEPLOY_SKIP_REMOTE_FRESHNESS:-0}" != "1" ] || return 0
    [ "${AGENTDESK_DEPLOY_SKIP_FRESHNESS:-0}" != "1" ] || return 0
    [ -z "${AGENTDESK_DEPLOY_BINARY:-}" ] || return 0
    git -C "$REPO" rev-parse --is-inside-work-tree >/dev/null 2>&1 || return 0

    local upstream_ref remote_name remote_branch head_sha upstream_sha behind_count
    upstream_ref="$(git -C "$REPO" rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' 2>/dev/null || true)"
    if [ -z "$upstream_ref" ]; then
        echo "⚠ No git upstream configured for $(git -C "$REPO" branch --show-current 2>/dev/null || echo HEAD); skipping remote freshness check"
        return 0
    fi

    remote_name="${upstream_ref%%/*}"
    remote_branch="${upstream_ref#*/}"
    echo "▸ Checking git freshness against ${upstream_ref}..."
    if ! git -C "$REPO" fetch --quiet "$remote_name" "$remote_branch"; then
        echo "✗ Could not refresh ${upstream_ref}; refusing release deploy from unverifiable source"
        echo "  Set AGENTDESK_DEPLOY_SKIP_REMOTE_FRESHNESS=1 only for an intentional offline deploy."
        exit 1
    fi

    head_sha="$(git -C "$REPO" rev-parse HEAD)"
    upstream_sha="$(git -C "$REPO" rev-parse "$upstream_ref")"
    [ "$head_sha" != "$upstream_sha" ] || return 0

    behind_count="$(git -C "$REPO" rev-list --count "HEAD..$upstream_ref" 2>/dev/null || echo 0)"
    if [ "$behind_count" != "0" ]; then
        echo "✗ Repo HEAD is behind ${upstream_ref} by ${behind_count} commit(s); refusing stale release deploy"
        echo "  Pull/rebase before deploy, or set AGENTDESK_DEPLOY_SKIP_REMOTE_FRESHNESS=1 only when intentional."
        exit 1
    fi
}

_check_repo_source_identity() {
    [ "${AGENTDESK_DEPLOY_SKIP_FRESHNESS:-0}" != "1" ] || return 0
    [ -z "${AGENTDESK_DEPLOY_BINARY:-}" ] || return 0
    git -C "$REPO" rev-parse --is-inside-work-tree >/dev/null 2>&1 || return 0

    local branch head_sha head_short main_sha main_short dirty_status dirty_flag
    branch="$(git -C "$REPO" rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)"
    head_sha="$(git -C "$REPO" rev-parse HEAD)"
    head_short="$(git -C "$REPO" rev-parse --short=12 HEAD)"
    dirty_status="$(git -C "$REPO" status --porcelain)"
    if [ -n "$dirty_status" ]; then
        dirty_flag=true
    else
        dirty_flag=false
    fi

    if [ "${AGENTDESK_DEPLOY_SKIP_REMOTE_FRESHNESS:-0}" != "1" ]; then
        if ! git -C "$REPO" fetch --quiet origin main; then
            echo "✗ Could not refresh origin/main; refusing release deploy from unverifiable source"
            echo "  Set AGENTDESK_DEPLOY_SKIP_REMOTE_FRESHNESS=1 only for an intentional offline deploy."
            exit 1
        fi
    fi
    main_sha="$(git -C "$REPO" rev-parse origin/main 2>/dev/null || true)"
    main_short=""
    if [ -n "$main_sha" ]; then
        main_short="$(git -C "$REPO" rev-parse --short=12 origin/main 2>/dev/null || true)"
    fi

    echo "▸ Build source: branch=${branch} head=${head_short} origin/main=${main_short:-unknown} dirty=${dirty_flag}"

    if [ "${AGENTDESK_DEPLOY_ALLOW_NON_MAIN:-0}" != "1" ]; then
        if [ "$branch" != "main" ]; then
            echo "✗ Refusing release deploy from non-main branch: ${branch}"
            echo "  Switch to main and fast-forward, or set AGENTDESK_DEPLOY_ALLOW_NON_MAIN=1 for an intentional branch deploy."
            exit 1
        fi
        if [ -n "$main_sha" ] && [ "$head_sha" != "$main_sha" ]; then
            echo "✗ Refusing release deploy: HEAD (${head_short}) does not match origin/main (${main_short})"
            echo "  Fast-forward to origin/main, or set AGENTDESK_DEPLOY_ALLOW_NON_MAIN=1 for an intentional local-source deploy."
            exit 1
        fi
    fi

    if [ "$dirty_flag" = true ] && [ "${AGENTDESK_DEPLOY_ALLOW_DIRTY:-0}" != "1" ]; then
        echo "✗ Refusing release deploy from a dirty worktree:"
        printf '%s\n' "$dirty_status" | sed 's/^/  /'
        echo "  Commit/stash local changes, or set AGENTDESK_DEPLOY_ALLOW_DIRTY=1 for an intentional dirty deploy."
        exit 1
    fi
}

_assert_release_binary_runtime_surface() {
    # If this source tree contains durable routines, the staged binary must expose
    # the matching worker/API surface. This catches deploying an older binary that
    # can pass /api/health while silently dropping scheduled routine execution.
    [ -f "$REPO/src/services/routines/runtime.rs" ] || return 0
    [ -f "$REPO/src/server/routes/routines.rs" ] || return 0
    command -v strings >/dev/null 2>&1 || {
        echo "✗ 'strings' is required for release binary surface validation"
        exit 1
    }

    local surface_dump
    surface_dump="$(mktemp "${TMPDIR:-/tmp}/agentdesk-binary-surface.XXXXXX")"
    strings "$SOURCE_BINARY" >"$surface_dump"
    if ! grep -Fq "routine-runtime" "$surface_dump"; then
        rm -f "$surface_dump"
        echo "✗ Source binary is missing the routine-runtime worker surface: $SOURCE_BINARY"
        echo "  Rebuild from a routines-enabled checkout before deploying release."
        exit 1
    fi
    if ! grep -Fq "/api/routines" "$surface_dump"; then
        rm -f "$surface_dump"
        echo "✗ Source binary is missing the /api/routines API surface: $SOURCE_BINARY"
        echo "  Rebuild from a routines-enabled checkout before deploying release."
        exit 1
    fi
    rm -f "$surface_dump"
}

_finalize_detached_helper() {
    local status="${1:-0}"
    [ "$DEPLOY_DETACHED_CHILD" = "1" ] || return 0
    [ -n "$REPORT_CHANNEL_ID" ] || return 0

    local content
    if [ "$status" -eq 0 ]; then
        content="✅ release deploy complete"
    else
        # Emit a deterministic failure marker into the helper log so an operator
        # tailing the log can poll for a single regex covering both outcomes
        # (success: `═══ Deploy Complete ═══`, failure: this line).
        echo "═══ DEPLOY FAILED (exit=${status}) ═══"
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

_manifest_latest_migration_name() {
    # Latest postgres migration recorded by the LAST SUCCESSFUL deploy. The
    # manifest is only rewritten on the success path (after DEPLOY_OK), so during
    # a failing deploy it still reflects the binary that is now the rollback
    # target (.prev). Prints the migration filename; returns non-zero when the
    # manifest or field is absent so the caller can fail closed. See #4348.
    local manifest="$ADK_REL/runtime/release-source.json"
    [ -f "$manifest" ] || return 1
    python3 - "$manifest" <<'PY' 2>/dev/null
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        data = json.load(handle)
except Exception:
    sys.exit(1)
value = data.get("latest_postgres_migration") or ""
if not value:
    sys.exit(1)
print(value)
PY
}

_rollback_would_brick_on_migration() {
    # #4348 Defect 2: refuse a rollback that would strand the previous binary
    # behind a migration the new binary already applied to the SHARED Postgres.
    # The old binary aborts boot with "migration N was previously applied but is
    # missing in the resolved migrations", and because the row lives in the
    # shared DB, every OTHER node bricks on its next restart too. Returns 0 =>
    # rollback unsafe (fail-forward); returns 1 => rollback safe. Fails CLOSED on
    # any ambiguity (safety > minimal-change): a rollback must never brick.
    if [ "${AGENTDESK_DEPLOY_FORCE_ROLLBACK:-0}" = "1" ]; then
        echo "  ▸ [rollback-guard] AGENTDESK_DEPLOY_FORCE_ROLLBACK=1 — skipping migration-advance guard" >&2
        return 1
    fi
    local new_path new_name old_name
    new_path="$(_latest_postgres_migration_path 2>/dev/null || true)"
    if [ -z "$new_path" ]; then
        echo "  ⚠ [rollback-guard] cannot resolve the new binary's latest migration ($REPO/migrations/postgres) — treating rollback as unsafe" >&2
        return 0
    fi
    new_name="$(basename "$new_path")"
    old_name="$(_manifest_latest_migration_name || true)"
    if [ -z "$old_name" ]; then
        echo "  ⚠ [rollback-guard] no previous-deploy migration record ($ADK_REL/runtime/release-source.json) — cannot prove the rollback binary handles ${new_name}; treating rollback as unsafe" >&2
        return 0
    fi
    if _migration_advanced "$new_name" "$old_name"; then
        echo "  ▸ [rollback-guard] new migration ${new_name} is ahead of rollback target ${old_name}" >&2
        return 0
    fi
    echo "  ▸ [rollback-guard] rollback target ${old_name} is at/ahead of new migration ${new_name} — safe to roll back" >&2
    return 1
}

# #3858: restore the last-known-good release binary and restart the service.
# Invoked from the EXIT trap (via _cleanup_on_exit) whenever the binary was
# promoted but the deploy never reached DEPLOY_OK — i.e. ANY non-zero exit after
# promotion, not only the explicit health-check branch (an unguarded
# post-promotion command failing under `set -e` is covered too). Every step
# except the restart is best-effort so a failed re-lock can NEVER skip the
# restart (#3858 finding 3): the service must always come back up.
_rollback_release_binary() {
    local rel_binary="${REL_BINARY:-}"
    local rel_backup="${REL_BINARY_BACKUP:-}"
    local plist="${PLIST_REL:-}"
    local rel_port="${REL_PORT:-${AGENTDESK_REL_PORT:-${ADK_DEFAULT_PORT:-8791}}}"
    local domain

    [ -n "$rel_binary" ] && [ -n "$plist" ] || return 0
    if [ ! -f "$rel_backup" ]; then
        echo "⚠ No rollback backup available (${rel_backup:-unset} missing) — cannot auto-rollback"
        return 0
    fi

    # #4348 Defect 2: fail-forward instead of bricking when the new binary
    # advanced the shared Postgres schema past what the rollback target can boot.
    if _rollback_would_brick_on_migration; then
        echo ""
        echo "🛑 ROLLBACK REFUSED — schema migrations advanced beyond the rollback target (#4348)"
        echo "   The new binary already applied a Postgres migration to the SHARED database that"
        echo "   the previous binary ($rel_backup) does not embed. Restarting the old binary would"
        echo "   fail with 'migration was previously applied but is missing in the resolved"
        echo "   migrations' and REFUSE TO BOOT. Because the migration row lives in the shared"
        echo "   Postgres, rolling back would ALSO brick every other node on its next restart —"
        echo "   turning a one-node deploy failure into a cluster-wide outage."
        echo ""
        echo "   FAIL-FORWARD: leaving the NEW binary live (it is what is currently running under"
        echo "   launchd). The rollback backup at $rel_backup is preserved for manual use."
        echo ""
        echo "   MANUAL INTERVENTION REQUIRED:"
        echo "     1. Check whether the new binary is actually serving:"
        echo "          curl -s http://${ADK_DEFAULT_LOOPBACK}:${rel_port}/api/health"
        echo "        If it reports server_up/db/dashboard true, the deploy likely tripped a"
        echo "        readiness edge case — confirm it is serving and no rollback is needed."
        echo "     2. If the new binary is genuinely broken, FIX FORWARD: patch the code and"
        echo "        redeploy. Do NOT downgrade the binary while the newer migration is applied."
        echo "     3. A manual downgrade is only safe AFTER you revert the migration on the shared"
        echo "        Postgres. To force the classic auto-rollback on a re-run (once the DB is"
        echo "        reverted), set AGENTDESK_DEPLOY_FORCE_ROLLBACK=1."
        echo "     4. Release logs: ${ADK_REL:-}/logs/"
        echo ""
        return 0
    fi

    echo "↩ Rolling back release binary to previous good version..."
    domain="$(_launchd_domain)" || domain="gui/$(id -u 2>/dev/null)"
    # Stop the crash-looping new process before swapping the binary back.
    launchctl bootout "$domain/$plist" 2>/dev/null || true
    tmux kill-session -t "${AGENTDESK_RELEASE_TMUX_SESSION:-AgentDesk-dcserver-release-manual}" 2>/dev/null || true
    # The bad binary is never locked (uchg is deferred to the success path), so
    # nouchg here is defensive. mv is an atomic same-dir rename: the backup
    # replaces the bad binary in one step — at no instant are both copies gone.
    chflags nouchg "$rel_binary" 2>/dev/null || true
    if ! mv -f "$rel_backup" "$rel_binary"; then
        echo "✗ Failed to restore previous binary from $rel_backup — manual intervention required"
        return 0
    fi
    # #3858 finding 3: re-lock is best-effort and MUST NOT abort the restart
    # below. A failed chflags can never leave the good binary restored but the
    # service stopped.
    chflags uchg "$rel_binary" 2>/dev/null || true
    echo "↩ Previous binary restored — restarting release..."
    xattr -d com.apple.quarantine "$HOME/Library/LaunchAgents/$plist.plist" 2>/dev/null || true
    if ! launchctl bootstrap "$domain" "$HOME/Library/LaunchAgents/$plist.plist"; then
        echo "⚠ launchd bootstrap failed during rollback — using tmux fallback"
        start_release_tmux_fallback || true
    fi
    if wait_for_http_service_health "$plist" "$rel_port" "$DEPLOY_HEALTH_RETRIES" "$DEPLOY_HEALTH_DELAY_SECS" 1 1 1; then
        echo "✓ Rollback succeeded — release healthy on :${rel_port} with previous binary"
    else
        echo "✗ Rollback restart did not reach healthy state — manual intervention required (logs: ${ADK_REL:-}/logs/)"
    fi
}

_cleanup_on_exit() {
    local status=$?
    # #3858: if the binary was promoted (ROLLBACK_ARMED) but the deploy never
    # reached DEPLOY_OK, restore the last-known-good binary and restart BEFORE the
    # staging cleanup below. This catches ANY non-zero exit after promotion — an
    # unguarded post-promotion command under `set -e`, not only the explicit
    # health-check branch — so a crash-on-boot binary can never stay live (#3858).
    if [ "${ROLLBACK_ARMED:-0}" = 1 ] && [ "${DEPLOY_OK:-0}" != 1 ]; then
        _rollback_release_binary
    fi
    if [ -n "${STAGED_BINARY:-}" ] && [ -e "$STAGED_BINARY" ]; then
        rm -f "$STAGED_BINARY" 2>/dev/null || true
    fi
    if [ -n "${POLICIES_STAGED:-}" ] && [ -d "$POLICIES_STAGED" ]; then
        rm -rf "$POLICIES_STAGED" 2>/dev/null || true
    fi
    if [ -n "${LAUNCHD_MIGRATED_STAGED:-}" ] && [ -d "$LAUNCHD_MIGRATED_STAGED" ]; then
        rm -rf "$LAUNCHD_MIGRATED_STAGED" 2>/dev/null || true
    fi
    if [ -n "${RELEASE_ROOT_SCRIPTS_STAGED:-}" ] && [ -d "$RELEASE_ROOT_SCRIPTS_STAGED" ]; then
        rm -rf "$RELEASE_ROOT_SCRIPTS_STAGED" 2>/dev/null || true
    fi
    if [ -n "${DEPLOY_MKDIR_LOCK_DIR:-}" ] && [ -d "$DEPLOY_MKDIR_LOCK_DIR" ]; then
        rm -rf "$DEPLOY_MKDIR_LOCK_DIR" 2>/dev/null || true
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

_resolve_deploy_peers() {
    if [ "${#DEPLOY_PEERS_OVERRIDE[@]}" -gt 0 ]; then
        printf '%s\n' "${DEPLOY_PEERS_OVERRIDE[@]}"
        return 0
    fi
    if [ -n "${AGENTDESK_DEPLOY_PEERS:-}" ]; then
        printf '%s\n' "$AGENTDESK_DEPLOY_PEERS" \
            | tr ',' '\n' \
            | sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//' \
            | grep -vE '^$'
        return 0
    fi
    if [ -f "$DEPLOY_PEERS_FILE" ]; then
        sed -E 's/[[:space:]]*#.*$//; s/^[[:space:]]+//; s/[[:space:]]+$//' "$DEPLOY_PEERS_FILE" \
            | grep -vE '^$'
        return 0
    fi
    printf ''
}

_deploy_peer_env_prelude() {
    printf 'AGENTDESK_DEPLOY_PEER_INVOCATION=1'
    local name value
    for name in \
        AGENTDESK_CODESIGN_IDENTITY \
        AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN \
        AGENTDESK_CODESIGN_KEYCHAIN_PW_FILE \
        AGENTDESK_CODESIGN_KEYCHAIN_NAME \
        AGENTDESK_DEPLOY_ALL_NODES \
        AGENTDESK_DEPLOY_BINARY \
        AGENTDESK_DEPLOY_DELAY_SECS \
        AGENTDESK_DEPLOY_FAST \
        AGENTDESK_DEPLOY_HEALTH_DELAY_SECS \
        AGENTDESK_DEPLOY_HEALTH_RETRIES \
        AGENTDESK_DEPLOY_LOCK_FILE \
        AGENTDESK_DEPLOY_PEERS \
        AGENTDESK_DEPLOY_PEERS_FILE \
        AGENTDESK_DEPLOY_SKIP_BUILD_CACHE_CLEANUP \
        AGENTDESK_DEPLOY_SKIP_FRESHNESS \
        AGENTDESK_DEPLOY_SKIP_REMOTE_FRESHNESS \
        AGENTDESK_DEPLOY_FORCE_RESOURCE_PREFLIGHT \
        AGENTDESK_DEPLOY_MAX_LOADAVG \
        AGENTDESK_DEPLOY_MAX_MEM_PRESSURE_LEVEL \
        AGENTDESK_DEPLOY_HIGH_CPU_PCT \
        AGENTDESK_DEPLOY_RUNAWAY_CPU_RATIO \
        AGENTDESK_DEPLOY_RUNAWAY_MIN_ELAPSED \
        AGENTDESK_DEPLOY_TEST_MODE \
        AGENTDESK_REL_PORT \
        AGENTDESK_REPORT_CHANNEL_ID \
        AGENTDESK_REPORT_PROVIDER \
        AGENTDESK_SKIP_TURN_DRAIN \
        AGENTDESK_DEPLOY_LOCK_TIMEOUT_SECS \
        AGENTDESK_BUNDLE_ID \
        AGENTDESK_DCSERVER_LABEL \
        AGENTDESK_PLIST_REL \
        AGENTDESK_ROOT_DIR \
        AGENTDESK_REPO_DIR \
        OBSIDIAN_VAULT_ROOT \
        AGENTDESK_OBSIDIAN_AGENTS_SRC
    do
        value="${!name:-}"
        [ -n "$value" ] || continue
        printf ' %s=%q' "$name" "$value"
    done
}

_deploy_to_one_peer() {
    local peer="$1"
    shift
    local quoted_args=""
    local env_prelude
    local remote_cd_command
    local remote_deploy_command
    local remote_presync_command
    env_prelude="$(_deploy_peer_env_prelude)"
    if [ "$#" -gt 0 ]; then
        quoted_args=$(printf ' %q' "$@")
    fi
    if [ -n "${AGENTDESK_PEER_REPO_DIR:-}" ]; then
        remote_cd_command="cd $(printf '%q' "$AGENTDESK_PEER_REPO_DIR")"
    else
        remote_cd_command='remote_root="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"; cd "${AGENTDESK_REPO_DIR:-$remote_root/workspaces/agentdesk}"'
    fi
    remote_presync_command="set -e
${remote_cd_command}
git fetch --quiet origin main
git checkout --quiet main
git merge --quiet --ff-only origin/main"
    remote_deploy_command="${remote_cd_command} && ${env_prelude} bash scripts/deploy-release.sh${quoted_args}"

    echo "▸ [peer:$peer] Pre-syncing repo (fast-forward only)..."
    if ! ssh -o ConnectTimeout="$DEPLOY_SSH_CONNECT_TIMEOUT" "$peer" "bash -lc $(printf '%q' "$remote_presync_command")"; then
        echo "✗ [peer:$peer] Pre-sync failed (diverged, fetch error, or unreachable within ${DEPLOY_SSH_CONNECT_TIMEOUT}s). Resolve on the peer and retry."
        return 1
    fi

    # Operator-private routines are excluded from the repo (.gitignore:50), so the
    # peer's own `git fetch` above cannot deliver them. Push them before the peer
    # deploys: leadership can move between nodes, and the routine runtime is a
    # LeaderOnly worker that resolves `script_ref` against the local disk. A node
    # missing these files fails every routine row with "routine script ... is not
    # loaded". No --delete: the peer may hold routines this node does not.
    if [ -d "$ADK_REL/routines" ]; then
        local peer_adk_rel
        if ! peer_adk_rel="$(ssh -o ConnectTimeout="$DEPLOY_SSH_CONNECT_TIMEOUT" "$peer" \
            'bash -lc '"$(printf '%q' 'echo "${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"')"'')"; then
            echo "✗ [peer:$peer] could not resolve remote AGENTDESK_ROOT_DIR"
            return 1
        fi
        peer_adk_rel="$(printf '%s' "$peer_adk_rel" | tr -d '\r')"
        echo "▸ [peer:$peer] Syncing operator routine scripts..."
        if ! rsync -a -e "ssh -o ConnectTimeout=$DEPLOY_SSH_CONNECT_TIMEOUT" \
            "$ADK_REL/routines/" "$peer:$peer_adk_rel/routines/"; then
            echo "✗ [peer:$peer] routine script sync failed"
            return 1
        fi
    fi

    echo "▸ [peer:$peer] Running deploy-release.sh..."
    if ! ssh -o ConnectTimeout="$DEPLOY_SSH_CONNECT_TIMEOUT" "$peer" "bash -lc $(printf '%q' "$remote_deploy_command")"; then
        echo "✗ [peer:$peer] deploy-release.sh failed"
        return 1
    fi

    echo "✓ [peer:$peer] deploy completed"
    return 0
}

_deploy_to_all_peers() {
    [ "$DEPLOY_PEER_INVOCATION" != "1" ] || {
        # Avoid recursive cluster deploy when this run is itself an SSH-driven peer leg.
        return 0
    }

    local peers
    peers=$(_resolve_deploy_peers)
    if [ -z "$peers" ]; then
        echo "▸ --all-nodes set but no peers resolved; skipping cluster deploy."
        echo "  configure peers via:"
        echo "    - $DEPLOY_PEERS_FILE  (one SSH alias per line, '#' comments allowed)"
        echo "    - AGENTDESK_DEPLOY_PEERS=mac-book,other-node  (comma-separated env)"
        echo "    - --peer <ssh-alias>  (repeatable flag)"
        return 0
    fi

    echo "═══ Cluster Deploy → Peers ═══"
    local failures=0
    while IFS= read -r peer; do
        [ -n "$peer" ] || continue
        if ! _deploy_to_one_peer "$peer" "$@"; then
            failures=$((failures + 1))
        fi
    done <<<"$peers"

    if [ "$failures" -gt 0 ]; then
        echo "✗ Cluster deploy: $failures peer(s) failed"
        exit 1
    fi
    echo "═══ Cluster Deploy Complete (all peers healthy) ═══"
}

_acquire_release_deploy_lock() {
    if [ "${AGENTDESK_DEPLOY_LOCK_HELD:-0}" = "1" ]; then
        echo "▸ [gate] Release deploy lock acquired"
        return 0
    fi

    mkdir -p "$(dirname "$DEPLOY_LOCK_FILE")"
    echo "▸ [gate] Waiting for release deploy lock: $DEPLOY_LOCK_FILE"

    if command -v lockf >/dev/null 2>&1; then
        exec env AGENTDESK_DEPLOY_LOCK_HELD=1 \
            lockf -k -t "$DEPLOY_LOCK_TIMEOUT_SECS" "$DEPLOY_LOCK_FILE" "$0" "$@"
    fi

    if command -v flock >/dev/null 2>&1; then
        exec env AGENTDESK_DEPLOY_LOCK_HELD=1 \
            flock -w "$DEPLOY_LOCK_TIMEOUT_SECS" "$DEPLOY_LOCK_FILE" "$0" "$@"
    fi

    local lock_dir="${DEPLOY_LOCK_FILE}.d"
    local waited=0
    while ! mkdir "$lock_dir" 2>/dev/null; do
        if [ "$waited" -ge "$DEPLOY_LOCK_TIMEOUT_SECS" ]; then
            echo "✗ [gate] Timed out waiting for release deploy lock after ${DEPLOY_LOCK_TIMEOUT_SECS}s"
            if [ -f "$lock_dir/pid" ]; then
                echo "  holder pid: $(cat "$lock_dir/pid" 2>/dev/null || echo "?")"
            fi
            exit 1
        fi
        sleep 2
        waited=$((waited + 2))
    done
    DEPLOY_MKDIR_LOCK_DIR="$lock_dir"
    printf '%s\n' "$$" > "$lock_dir/pid" 2>/dev/null || true
    echo "▸ [gate] Release deploy lock acquired"
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
export AGENTDESK_CODESIGN_KEYCHAIN_PW_FILE=$(printf '%q' "${AGENTDESK_CODESIGN_KEYCHAIN_PW_FILE:-}")
export AGENTDESK_CODESIGN_KEYCHAIN_NAME=$(printf '%q' "${AGENTDESK_CODESIGN_KEYCHAIN_NAME:-}")
export AGENTDESK_DEPLOY_BINARY=$(printf '%q' "${AGENTDESK_DEPLOY_BINARY:-}")
export AGENTDESK_DEPLOY_FAST=$(printf '%q' "${AGENTDESK_DEPLOY_FAST:-0}")
export AGENTDESK_DEPLOY_SKIP_FRESHNESS=$(printf '%q' "${AGENTDESK_DEPLOY_SKIP_FRESHNESS:-0}")
export AGENTDESK_DEPLOY_SKIP_REMOTE_FRESHNESS=$(printf '%q' "${AGENTDESK_DEPLOY_SKIP_REMOTE_FRESHNESS:-0}")
export AGENTDESK_DEPLOY_FORCE_RESOURCE_PREFLIGHT=$(printf '%q' "${AGENTDESK_DEPLOY_FORCE_RESOURCE_PREFLIGHT:-0}")
export AGENTDESK_DEPLOY_MAX_LOADAVG=$(printf '%q' "${AGENTDESK_DEPLOY_MAX_LOADAVG:-}")
export AGENTDESK_DEPLOY_MAX_MEM_PRESSURE_LEVEL=$(printf '%q' "${AGENTDESK_DEPLOY_MAX_MEM_PRESSURE_LEVEL:-}")
export AGENTDESK_DEPLOY_HIGH_CPU_PCT=$(printf '%q' "${AGENTDESK_DEPLOY_HIGH_CPU_PCT:-}")
export AGENTDESK_DEPLOY_RUNAWAY_CPU_RATIO=$(printf '%q' "${AGENTDESK_DEPLOY_RUNAWAY_CPU_RATIO:-}")
export AGENTDESK_DEPLOY_RUNAWAY_MIN_ELAPSED=$(printf '%q' "${AGENTDESK_DEPLOY_RUNAWAY_MIN_ELAPSED:-}")
export AGENTDESK_DEPLOY_ALLOW_NON_MAIN=$(printf '%q' "${AGENTDESK_DEPLOY_ALLOW_NON_MAIN:-0}")
export AGENTDESK_DEPLOY_ALLOW_DIRTY=$(printf '%q' "${AGENTDESK_DEPLOY_ALLOW_DIRTY:-0}")
export AGENTDESK_DEPLOY_LOCK_FILE=$(printf '%q' "$DEPLOY_LOCK_FILE")
export AGENTDESK_DEPLOY_LOCK_TIMEOUT_SECS=$(printf '%q' "$DEPLOY_LOCK_TIMEOUT_SECS")
export AGENTDESK_DEPLOY_ALL_NODES=$(printf '%q' "${AGENTDESK_DEPLOY_ALL_NODES:-0}")
export AGENTDESK_DEPLOY_PEERS=$(printf '%q' "${AGENTDESK_DEPLOY_PEERS:-}")
export AGENTDESK_DEPLOY_PEERS_FILE=$(printf '%q' "${AGENTDESK_DEPLOY_PEERS_FILE:-}")
export AGENTDESK_DEPLOY_PEER_INVOCATION=$(printf '%q' "${AGENTDESK_DEPLOY_PEER_INVOCATION:-0}")
export AGENTDESK_BUNDLE_ID=$(printf '%q' "$BUNDLE_ID")
export AGENTDESK_DCSERVER_LABEL=$(printf '%q' "$PLIST_REL")
export AGENTDESK_PLIST_REL=$(printf '%q' "${AGENTDESK_PLIST_REL:-}")
export OBSIDIAN_VAULT_ROOT=$(printf '%q' "${OBSIDIAN_VAULT_ROOT:-}")
export AGENTDESK_OBSIDIAN_AGENTS_SRC=$(printf '%q' "${AGENTDESK_OBSIDIAN_AGENTS_SRC:-}")
unset AGENTDESK_DEPLOY_LOCK_HELD
cd $(printf '%q' "$REPO")
exec $(printf '%q' "$SCRIPT_DIR/deploy-release.sh")${quoted_args}
EOF
    chmod +x "$helper_script"
    tmux new-session -d -s "$helper_session" "$helper_script"

    echo "▸ Self-hosted release deploy detected — using detached helper"
    echo "  helper tmux: $helper_session"
    echo "  helper log: $log_path"
    echo ""
    echo "  ⚠ DO NOT end the turn yet."
    echo "    The deploy runs detached so this operator turn is not killed mid-restart,"
    echo "    but the success/failure outcome must be verified BEFORE you reply."
    echo ""
    echo "    Poll the helper log in this turn until one terminal line appears:"
    echo "      success: ═══ Deploy Complete ═══"
    echo "      failure: ═══ DEPLOY FAILED (exit=N) ═══"
    echo ""
    echo "    One-shot wait command (polling loop — self-terminates after match):"
    echo "      LOG=$log_path; until [ -f \"\$LOG\" ] && grep -qm1 -E '═══ Deploy Complete ═══|═══ DEPLOY FAILED' \"\$LOG\"; do sleep 3; done; grep -E '═══ Deploy Complete ═══|═══ DEPLOY FAILED' \"\$LOG\" | tail -1"
    echo ""
    echo "    ⚠ DO NOT use 'tail -F | grep -m1' — grep -m1 exits on match but tail -F stays alive"
    echo "      on inotify wait, leaving the bash task hung past helper completion."
    echo ""
    echo "    On failure: read the log tail, diagnose the root cause (e.g. freshness gate,"
    echo "    codesign, health timeout), fix it in this same turn, and re-run deploy-release.sh."
}

if _self_hosted_release_session; then
    _spawn_detached_helper "$@"
    exit 0
fi

_acquire_release_deploy_lock "$@"

# #4255: resource-contention pre-flight — refuse (or, with the force hatch,
# warn) BEFORE any expensive build work when the machine is already saturated by
# another builder / high-load process, which twice KILLED a mid-flight deploy
# (07-05 concurrent UE build, 07-07 runaway ugrep). Runs on EVERY node: each
# peer invokes this same script under its own lock, so it checks its own local
# resources. Exact-name builder matching (pgrep -x) means the ssh client, sshd,
# and the deploy script itself are never mistaken for contention, and the
# high-CPU scan excludes this deploy's own process group. Skipped in the
# detached-helper dry run (DEPLOY_TEST_MODE=1), which never builds.
if [ "$DEPLOY_TEST_MODE" != "1" ]; then
    if ! _preflight_resource_contention; then
        exit 1
    fi
fi

# #743: Zero-inflight gate for create-pr dispatches on the release runtime.
# A restart during an in-flight create-pr dispatch leaves its completion
# unstamped after the new code rolls out. If the release API is unreachable
# the gate skips itself (recovery deploys must not be false-blocked).
REL_PORT="$(_resolve_release_server_port)"
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

if DASHBOARD_SOURCE=$(_resolve_dashboard_source); then
    echo "▸ Dashboard source: $DASHBOARD_SOURCE"
else
    echo "▸ Dashboard source missing — will build before staging"
    echo "  looked for: $REPO/dashboard/dist/index.html"
fi
if [ ! -d "$REPO/skills" ]; then
    echo "✗ Managed skills not found in workspace — aborting deploy"
    echo "  expected: $REPO/skills"
    exit 1
fi
if [ ! -d "$REPO/policies" ]; then
    echo "✗ Policies not found in workspace — aborting deploy"
    echo "  expected: $REPO/policies"
    exit 1
fi

_check_repo_source_identity

if [ "$DEPLOY_TEST_MODE" = "1" ]; then
    echo "▸ TEST MODE: skipping release bootout/copy/bootstrap"
    echo "✓ Detached helper dry run complete"
    exit 0
fi

# Ensure release dir exists
mkdir -p "$ADK_REL"/{bin,config,data,logs}

export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-10G}"
if setup_sccache_env; then
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
_ensure_dashboard_dependencies
_check_repo_remote_freshness
if [ -n "${AGENTDESK_DEPLOY_BINARY:-}" ]; then
    SOURCE_BINARY="$AGENTDESK_DEPLOY_BINARY"
else
    SOURCE_BINARY="$(_resolve_default_release_binary "$DEPLOY_BUILD_PROFILE")"
fi
if [ -z "${AGENTDESK_DEPLOY_BINARY:-}" ]; then
    if [ "$DEPLOY_BUILD_PROFILE" = "release" ]; then
        echo "▸ Building release binary..."
        (cd "$REPO" && cargo build --release --bin agentdesk)
    else
        echo "▸ Building ${DEPLOY_BUILD_PROFILE} binary (opt-in fast deploy profile)..."
        (cd "$REPO" && cargo build --profile "$DEPLOY_BUILD_PROFILE" --bin agentdesk)
    fi
    # Cargo tracks embedded migration inputs via build.rs. The freshness gate
    # below is mtime-based, and a successful current-HEAD cargo build can still
    # reuse an existing artifact, so align the mtime after build.
    [ -e "$SOURCE_BINARY" ] && touch "$SOURCE_BINARY"
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
OBSIDIAN_DEFAULT_VAULT_ROOT="$HOME/ObsidianVault"
if [ -d "$ADK_REL/ObsidianVault" ]; then
    OBSIDIAN_DEFAULT_VAULT_ROOT="$ADK_REL/ObsidianVault"
fi
OBSIDIAN_AGENTS_SRC="${AGENTDESK_OBSIDIAN_AGENTS_SRC:-${OBSIDIAN_VAULT_ROOT:-$OBSIDIAN_DEFAULT_VAULT_ROOT}/RemoteVault/adk-config/agents}"
if [ -d "$OBSIDIAN_AGENTS_SRC" ]; then
    echo "▸ Staging agent prompts from Obsidian vault..."
    PROMPTS_STAGED="$ADK_REL/config/agents.new"
    rm -rf "$PROMPTS_STAGED"
    mkdir -p "$PROMPTS_STAGED"
    rsync -a "$OBSIDIAN_AGENTS_SRC/" "$PROMPTS_STAGED/"
else
    if [ -n "${AGENTDESK_OBSIDIAN_AGENTS_SRC:-}" ]; then
        echo "⚠ Optional connector obsidian_agent_prompts invalid: $OBSIDIAN_AGENTS_SRC"
        echo "  state=missing_path reason=missing_path; core release deploy will continue."
    else
        echo "ℹ Optional connector obsidian_agent_prompts skipped: $OBSIDIAN_AGENTS_SRC"
        echo "  state=missing_config reason=missing_config; core release deploy will continue."
    fi
    echo "  Existing $ADK_REL/config/agents/ will be retained."
fi

# Stage managed skills before stopping release so skill sync never sees partial content.
echo "▸ Staging managed skills..."
SKILLS_STAGED="$ADK_REL/skills.new"
rm -rf "$SKILLS_STAGED"
mkdir -p "$SKILLS_STAGED"
rsync -a --delete "$REPO/skills/" "$SKILLS_STAGED/"

# Stage policies before stopping release so the runtime never sees a partial
# modular policy tree.
echo "▸ Staging policies..."
POLICIES_STAGED="$ADK_REL/policies.new"
rm -rf "$POLICIES_STAGED"
mkdir -p "$POLICIES_STAGED"
rsync -a --delete "$REPO/policies/" "$POLICIES_STAGED/"

# Stage routine scripts before stopping release so the runtime never executes a
# stale JS asset after a binary deploy.
if [ -d "$REPO/routines" ]; then
    echo "▸ Staging routines..."
    ROUTINES_STAGED="$ADK_REL/routines.new"
    rm -rf "$ROUTINES_STAGED"
    mkdir -p "$ROUTINES_STAGED"
    # Operator-private routines (.gitignore:50) live only under $ADK_REL/routines,
    # never in the repo. Seed the staging dir with them first so the repo overlay
    # below cannot delete a routine whose row still exists in `routines`; the
    # runtime would then fail it with "routine script ... is not loaded".
    if [ -d "$ADK_REL/routines" ]; then
        rsync -a "$ADK_REL/routines/" "$ROUTINES_STAGED/"
    fi
    # Engine-owned routines win on conflict. No --delete: see above.
    rsync -a "$REPO/routines/" "$ROUTINES_STAGED/"
else
    echo "⚠ Routines source missing: $REPO/routines"
    echo "  Skipping routine staging — existing $ADK_REL/routines/ will be retained."
fi

# Stage launchd-migrated shell entrypoints before stopping release so routines
# can invoke the same release-owned path on whichever node holds leadership.
if [ -d "$REPO/scripts/launchd-migrated" ]; then
    echo "▸ Staging launchd-migrated entrypoints..."
    LAUNCHD_MIGRATED_STAGED="$ADK_REL/scripts/launchd-migrated.new"
    rm -rf "$LAUNCHD_MIGRATED_STAGED"
    mkdir -p "$LAUNCHD_MIGRATED_STAGED"
    rsync -a --delete "$REPO/scripts/launchd-migrated/" "$LAUNCHD_MIGRATED_STAGED/"
else
    echo "⚠ Launchd-migrated entrypoint source missing: $REPO/scripts/launchd-migrated"
    echo "  Skipping launchd-migrated entrypoint staging — existing $ADK_REL/scripts/launchd-migrated/ will be retained."
fi

# Stage release-owned root shell entrypoints referenced by bundled migrated
# routines. queue-stability-batch.sh sources _defaults.sh from the same
# directory, so deploy both files together.
if [ -f "$REPO/scripts/queue-stability-batch.sh" ]; then
    echo "▸ Staging release root script entrypoints..."
    RELEASE_ROOT_SCRIPTS_STAGED="$ADK_REL/scripts.root.new"
    rm -rf "$RELEASE_ROOT_SCRIPTS_STAGED"
    mkdir -p "$RELEASE_ROOT_SCRIPTS_STAGED"
    cp "$REPO/scripts/_defaults.sh" "$RELEASE_ROOT_SCRIPTS_STAGED/_defaults.sh"
    cp "$REPO/scripts/queue-stability-batch.sh" "$RELEASE_ROOT_SCRIPTS_STAGED/queue-stability-batch.sh"
    chmod +x "$RELEASE_ROOT_SCRIPTS_STAGED/queue-stability-batch.sh"
else
    echo "⚠ Queue stability entrypoint source missing: $REPO/scripts/queue-stability-batch.sh"
    echo "  Skipping queue stability entrypoint staging — existing $ADK_REL/scripts/queue-stability-batch.sh will be retained."
fi

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
    if [ "$DEPLOY_BUILD_PROFILE" = "release" ]; then
        echo "  Run 'cargo build --release' or './scripts/build-release.sh' first."
    else
        echo "  Run 'cargo build --profile ${DEPLOY_BUILD_PROFILE} --bin agentdesk' first, or retry without --fast."
    fi
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
        if [ "$DEPLOY_BUILD_PROFILE" = "release" ]; then
            echo "  Rebuild with 'cargo build --release' before deploying, or override with"
        else
            echo "  Rebuild with 'cargo build --profile ${DEPLOY_BUILD_PROFILE} --bin agentdesk' before deploying, or override with"
        fi
        echo "  AGENTDESK_DEPLOY_SKIP_FRESHNESS=1 when intentional."
        exit 1
    fi
fi

_assert_release_binary_runtime_surface

if [ -f "$REL_LAUNCHD_ENV_FILE" ]; then
    echo "▸ Applying release launchd env for doctor preflight..."
    _apply_launchd_env_file_to_shell "$REL_LAUNCHD_ENV_FILE"
fi

echo "▸ Preflight PostgreSQL migration integrity via doctor..."
DOCTOR_JSON_TMP=$(mktemp "${TMPDIR:-/tmp}/agentdesk-doctor.XXXXXX.json")
set +e
"$SOURCE_BINARY" doctor --json >"$DOCTOR_JSON_TMP" 2>/dev/null
DOCTOR_RC=$?
set -e
if [ ! -s "$DOCTOR_JSON_TMP" ]; then
    echo "✗ Doctor preflight did not return JSON output."
    rm -f "$DOCTOR_JSON_TMP"
    exit 1
fi
if ! python3 - "$DOCTOR_JSON_TMP" <<'PY'
import json
import sys

path = sys.argv[1]
with open(path, "r", encoding="utf-8") as f:
    data = json.load(f)

checks = data.get("checks", [])
postgres = next((c for c in checks if c.get("id") == "postgres_connection"), None)
if not postgres:
    print("✗ Doctor preflight missing postgres_connection check.")
    raise SystemExit(1)

status = str(postgres.get("status", "")).lower()
evidence = postgres.get("evidence") or {}
drift_fields = {
    "missing_from_resolved": evidence.get("missing_from_resolved") or [],
    "unsuccessful_versions": evidence.get("unsuccessful_versions") or [],
    "checksum_mismatches": evidence.get("checksum_mismatches") or [],
}
drift = {key: value for key, value in drift_fields.items() if value}
if status in {"pass", "ok", "info"} and not drift:
    raise SystemExit(0)

detail = postgres.get("detail") or "no detail"
actual = postgres.get("actual") or "unknown"
if drift:
    drift_json = json.dumps(drift, sort_keys=True)
    print(f"✗ Doctor postgres preflight failed: status={status}, drift={drift_json}, detail={detail}, actual={actual}")
else:
    print(f"✗ Doctor postgres preflight failed: status={status}, detail={detail}, actual={actual}")
raise SystemExit(1)
PY
then
    rm -f "$DOCTOR_JSON_TMP"
    exit 1
fi
if [ "$DOCTOR_RC" -ne 0 ]; then
    echo "⚠ doctor command returned non-zero ($DOCTOR_RC), but postgres preflight check passed."
fi
rm -f "$DOCTOR_JSON_TMP"

# Copy and sign the binary before stopping release. This keeps a missing
# certificate or failed codesign from taking down a healthy dcserver.
echo "▸ Staging signed binary from $SOURCE_BINARY..."
STAGED_BINARY="$(_staged_deploy_binary_path)"
cp "$SOURCE_BINARY" "$STAGED_BINARY"
chmod +x "$STAGED_BINARY"
xattr -d com.apple.provenance "$STAGED_BINARY" 2>/dev/null || true
sign_binary_with_fallback "$STAGED_BINARY"
_clean_release_build_cache_after_staging

# #4381: a deploy restarts dcserver, so a short relay gap is EXPECTED here.
# Touch the marker the out-of-band relay watchdog checks; while it is fresh
# (deploy_quiet_secs) the watchdog logs instead of alerting.
touch "$ADK_REL/logs/relay-watchdog.deploy-marker" 2>/dev/null || true

# Stop release — wait for process to actually die (flock release)
echo "▸ Stopping release..."
LOCK_FILE="$ADK_REL/runtime/dcserver.lock"
OLD_PID=""
if [ -f "$LOCK_FILE" ]; then
    OLD_PID=$(cat "$LOCK_FILE" 2>/dev/null || true)
fi
LAUNCHD_DOMAIN="$(_launchd_domain)"
launchctl bootout "$LAUNCHD_DOMAIN/$PLIST_REL" 2>/dev/null || true
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

_post_deploy_smoke_log_identity_and_size() {
    local log_path="$1"
    if [ "$(uname -s)" = "Darwin" ]; then
        stat -f '%i %z' "$log_path"
    else
        stat -c '%i %s' "$log_path"
    fi
}

_post_deploy_smoke_log_head_fingerprint() {
    local log_path="$1"
    local byte_count="$2"
    case "$byte_count" in
        ''|*[!0-9]*) return 1 ;;
    esac
    if [ "$byte_count" -eq 0 ]; then
        if command -v shasum >/dev/null 2>&1; then
            shasum -a 256 < /dev/null \
                | awk 'NF { print "sha256:" $1; found = 1 } END { if (!found) exit 1 }'
        elif command -v sha256sum >/dev/null 2>&1; then
            sha256sum < /dev/null \
                | awk 'NF { print "sha256:" $1; found = 1 } END { if (!found) exit 1 }'
        elif command -v cksum >/dev/null 2>&1; then
            cksum < /dev/null \
                | awk 'NF >= 2 { print "cksum:" $1 ":" $2; found = 1 } END { if (!found) exit 1 }'
        else
            return 1
        fi
        return
    fi
    if command -v shasum >/dev/null 2>&1; then
        head -c "$byte_count" "$log_path" \
            | shasum -a 256 \
            | awk 'NF { print "sha256:" $1; found = 1 } END { if (!found) exit 1 }'
    elif command -v sha256sum >/dev/null 2>&1; then
        head -c "$byte_count" "$log_path" \
            | sha256sum \
            | awk 'NF { print "sha256:" $1; found = 1 } END { if (!found) exit 1 }'
    elif command -v cksum >/dev/null 2>&1; then
        head -c "$byte_count" "$log_path" \
            | cksum \
            | awk 'NF >= 2 { print "cksum:" $1 ":" $2; found = 1 } END { if (!found) exit 1 }'
    else
        return 1
    fi
}

# Watermark the stdout log only after the old dcserver has exited. The sampler
# uses the byte offset while inode, size, and the bounded head fingerprint still
# prove append-only growth. Rotation, shrink, or a rewritten head selects the
# entire current file (#4511).
POST_DEPLOY_SMOKE_LOG_PATH="$ADK_REL/logs/dcserver.stdout.log"
POST_DEPLOY_SMOKE_LOG_FINGERPRINT_CAP=4096
POST_DEPLOY_SMOKE_LOG_OFFSET=""
POST_DEPLOY_SMOKE_LOG_INODE=""
POST_DEPLOY_SMOKE_LOG_FINGERPRINT=""
if [ ! -e "$POST_DEPLOY_SMOKE_LOG_PATH" ]; then
    POST_DEPLOY_SMOKE_LOG_OFFSET=0
    POST_DEPLOY_SMOKE_LOG_INODE=0
elif POST_DEPLOY_SMOKE_LOG_STAT=$(
    _post_deploy_smoke_log_identity_and_size "$POST_DEPLOY_SMOKE_LOG_PATH" 2>/dev/null
); then
    read -r POST_DEPLOY_SMOKE_LOG_INODE POST_DEPLOY_SMOKE_LOG_OFFSET \
        <<< "$POST_DEPLOY_SMOKE_LOG_STAT"
fi
case "$POST_DEPLOY_SMOKE_LOG_OFFSET" in
    ''|*[!0-9]*) ;;
    *)
        POST_DEPLOY_SMOKE_LOG_FINGERPRINT_BYTES="$POST_DEPLOY_SMOKE_LOG_OFFSET"
        if [ "$POST_DEPLOY_SMOKE_LOG_FINGERPRINT_BYTES" -gt "$POST_DEPLOY_SMOKE_LOG_FINGERPRINT_CAP" ]; then
            POST_DEPLOY_SMOKE_LOG_FINGERPRINT_BYTES="$POST_DEPLOY_SMOKE_LOG_FINGERPRINT_CAP"
        fi
        POST_DEPLOY_SMOKE_LOG_FINGERPRINT_PATH="$POST_DEPLOY_SMOKE_LOG_PATH"
        if [ ! -e "$POST_DEPLOY_SMOKE_LOG_FINGERPRINT_PATH" ]; then
            POST_DEPLOY_SMOKE_LOG_FINGERPRINT_PATH=/dev/null
        fi
        if ! POST_DEPLOY_SMOKE_LOG_FINGERPRINT=$(
            _post_deploy_smoke_log_head_fingerprint \
                "$POST_DEPLOY_SMOKE_LOG_FINGERPRINT_PATH" \
                "$POST_DEPLOY_SMOKE_LOG_FINGERPRINT_BYTES"
        ); then
            POST_DEPLOY_SMOKE_LOG_FINGERPRINT=""
        fi
        ;;
esac
unset POST_DEPLOY_SMOKE_LOG_STAT
unset POST_DEPLOY_SMOKE_LOG_FINGERPRINT_BYTES POST_DEPLOY_SMOKE_LOG_FINGERPRINT_PATH

# Promote the already signed staged binary atomically. In-place codesign can
# corrupt the OS signing cache if it fails mid-write.
#
# #3858: back up the current good binary BEFORE overwriting it so a runtime-only
# crash (passes compile/doctor/sign but crash-loops on boot) can be rolled back
# instead of leaving the release down with the last-good binary already gone.
REL_BINARY="$ADK_REL/bin/agentdesk"
REL_BINARY_BACKUP="$ADK_REL/bin/agentdesk.prev"
chflags nouchg "$REL_BINARY" 2>/dev/null || true
if [ -f "$REL_BINARY_BACKUP" ]; then
    # #3858 (re-entrancy / finding 2): treat .prev as last-KNOWN-GOOD. A leftover
    # .prev means a PRIOR deploy failed before its success-path cleanup, so it
    # holds that deploy's last good binary (captured when the then-live binary was
    # still healthy). The CURRENT live binary may be the unverified/bad binary the
    # prior deploy promoted — do NOT overwrite a good .prev with it. Preserve the
    # existing last-known-good as the rollback target so a re-run can still recover.
    echo "▸ Preserving existing last-known-good backup for rollback (prior deploy left one)..."
    # Ensure it is mutable so the rollback's `mv -f` can consume it.
    chflags nouchg "$REL_BINARY_BACKUP" 2>/dev/null || true
elif [ -f "$REL_BINARY" ]; then
    # No prior backup: the current live binary is the last successful deploy's
    # health-confirmed binary (the success path drops .prev once health passes).
    # Capture it as the rollback target. cp (not mv) so the last-good binary is
    # never absent — both the backup and the live binary exist until the staged
    # binary atomically replaces it; no window where both copies are gone.
    # -p preserves mode/owner (and, since REL_BINARY was just unlocked above, the
    # copy is not immutable).
    #
    # #3858 (re-review finding 1): write the backup ATOMICALLY. A cp -p straight
    # to the final .prev name leaves a truncated .prev if the copy is interrupted
    # (SIGKILL / disk-full / power-loss); a later run's "leftover .prev =
    # last-known-good" branch above would then preserve that corrupt backup, and a
    # post-promotion failure could roll back onto a broken binary. Copy to a temp
    # sibling on the same filesystem, then rename(2): .prev is only ever the
    # complete old or complete new file, and an interrupted copy leaves only a
    # .prev.tmp, which the `[ -f "$REL_BINARY_BACKUP" ]` guard never consumes.
    echo "▸ Backing up current release binary for rollback..."
    cp -p "$REL_BINARY" "$REL_BINARY_BACKUP.tmp"
    mv -f "$REL_BINARY_BACKUP.tmp" "$REL_BINARY_BACKUP"
fi

echo "▸ Promoting staged binary..."
mv -f "$STAGED_BINARY" "$REL_BINARY"
STAGED_BINARY=""
# #3858: arm the EXIT-trap rollback the instant the binary is live. From here,
# ANY non-zero exit before DEPLOY_OK (set on the success path) restores the
# last-known-good backup and restarts the service — see _rollback_release_binary.
ROLLBACK_ARMED=1
# NOTE: the immutable re-lock (chflags uchg) is deferred until AFTER the health
# check passes (see below). Locking here would force the rollback path to fight
# the uchg flag on the bad binary, and the lock's only job — blocking unsigned
# overwrites of a serving binary — is not needed for the few seconds of deploy.

if [ "$PLIST_REL" = "com.agentdesk.release" ]; then
    echo "▸ Regenerating release launchd plist..."
    mkdir -p "$HOME/Library/LaunchAgents"
    "$ADK_REL/bin/agentdesk" emit-launchd-plist \
        --flavor release \
        --home "$HOME" \
        --root-dir "$ADK_REL" \
        --agentdesk-bin "$ADK_REL/bin/agentdesk" \
        --output "$HOME/Library/LaunchAgents/$PLIST_REL.plist"
else
    echo "⚠ Skipping launchd plist regeneration for custom label: $PLIST_REL"
fi

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

rm -rf "$ADK_REL/policies.old"
[ -d "$ADK_REL/policies" ] && mv "$ADK_REL/policies" "$ADK_REL/policies.old"
mv "$POLICIES_STAGED" "$ADK_REL/policies"
POLICIES_STAGED=""
rm -rf "$ADK_REL/policies.old"

# #3288: self-heal policies.dir config drift. The release runtime must load
# policies from the deployed snapshot ($ADK_REL/policies, staged above from the
# deploy-time git shape) — never from a dev workspace working tree, whose
# checked-out branch can silently diverge from the deployed binary. Runs while
# dcserver is stopped, so the rewrite is picked up by the post-deploy start.
AGENTDESK_YAML="$ADK_REL/config/agentdesk.yaml"
if [ -f "$AGENTDESK_YAML" ]; then
    POLICIES_DIR_MIGRATION=$(python3 - "$AGENTDESK_YAML" "$ADK_REL/policies" <<'PYEOF' 2>&1
import os
import re
import shutil
import sys
import tempfile

path, want = sys.argv[1], sys.argv[2]
with open(path) as f:
    lines = f.readlines()

out = []
in_policies = False
changed = False
previous = None
unsupported = None
for line in lines:
    body = line.rstrip("\n")
    if re.match(r"^policies:\s*\{", body):
        # Flow-style mapping (policies: {dir: ...}) — refuse to edit rather
        # than risk a bad rewrite; surfaced as a WARN by the caller.
        unsupported = "inline-map"
        in_policies = False
    elif re.match(r"^policies:\s*(#.*)?$", body):
        in_policies = True
    elif in_policies and body.strip() and not body[:1].isspace():
        in_policies = False
    if in_policies:
        # A '#' starts a comment only after whitespace (YAML); an unquoted
        # value may itself contain '#'. Bare/comment-only dir is healed too.
        empty = re.match(r"^(\s+dir:)((?:\s+#.*)|\s*)$", body)
        value = None if empty else re.match(r"^(\s+dir:\s*)([\"']?)(.+?)\2(\s+#.*)?\s*$", body)
        if empty:
            previous = ""
            comment = empty.group(2) if "#" in empty.group(2) else ""
            line = f"{empty.group(1)} {want}{comment}\n"
            changed = True
        elif value:
            previous = value.group(3)
            if previous != want:
                quote = value.group(2)
                tail = value.group(4) or ""
                line = f"{value.group(1)}{quote}{want}{quote}{tail}\n"
                changed = True
    out.append(line)

if changed:
    shutil.copy2(path, path + ".bak-policies-dir")
    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path) or ".", prefix=".agentdesk.yaml.")
    try:
        with os.fdopen(fd, "w") as f:
            f.writelines(out)
        shutil.copymode(path, tmp)
        os.replace(tmp, path)
    except BaseException:
        os.unlink(tmp)
        raise
if unsupported:
    print(f"changed=unsupported style={unsupported} previous={previous}")
else:
    print(f"changed={changed} previous={previous}")
PYEOF
) || POLICIES_DIR_MIGRATION="error: python exited $?"
    case "$POLICIES_DIR_MIGRATION" in
        changed=True*)
            echo "▸ Migrated policies.dir → $ADK_REL/policies ($POLICIES_DIR_MIGRATION; backup: $AGENTDESK_YAML.bak-policies-dir) [#3288]"
            ;;
        changed=False*)
            # Already aligned, or no explicit dir key (the binary's ./policies
            # default resolves to $ADK_REL/policies under the launchd CWD).
            ;;
        *)
            echo "⚠ policies.dir drift check failed (non-fatal): $POLICIES_DIR_MIGRATION"
            echo "  Verify $AGENTDESK_YAML policies.dir points at $ADK_REL/policies [#3288]"
            ;;
    esac
fi

if [ -n "${ROUTINES_STAGED:-}" ] && [ -d "$ROUTINES_STAGED" ]; then
    rm -rf "$ADK_REL/routines.old"
    [ -d "$ADK_REL/routines" ] && mv "$ADK_REL/routines" "$ADK_REL/routines.old"
    mv "$ROUTINES_STAGED" "$ADK_REL/routines"
    ROUTINES_STAGED=""
    rm -rf "$ADK_REL/routines.old"
fi

if [ -n "${LAUNCHD_MIGRATED_STAGED:-}" ] && [ -d "$LAUNCHD_MIGRATED_STAGED" ]; then
    mkdir -p "$ADK_REL/scripts"
    rm -rf "$ADK_REL/scripts/launchd-migrated.old"
    [ -d "$ADK_REL/scripts/launchd-migrated" ] && mv "$ADK_REL/scripts/launchd-migrated" "$ADK_REL/scripts/launchd-migrated.old"
    mv "$LAUNCHD_MIGRATED_STAGED" "$ADK_REL/scripts/launchd-migrated"
    LAUNCHD_MIGRATED_STAGED=""
    rm -rf "$ADK_REL/scripts/launchd-migrated.old"
fi

if [ -n "${RELEASE_ROOT_SCRIPTS_STAGED:-}" ] && [ -d "$RELEASE_ROOT_SCRIPTS_STAGED" ]; then
    mkdir -p "$ADK_REL/scripts"
    mv -f "$RELEASE_ROOT_SCRIPTS_STAGED/_defaults.sh" "$ADK_REL/scripts/_defaults.sh"
    mv -f "$RELEASE_ROOT_SCRIPTS_STAGED/queue-stability-batch.sh" "$ADK_REL/scripts/queue-stability-batch.sh"
    chmod +x "$ADK_REL/scripts/queue-stability-batch.sh"
    rm -rf "$RELEASE_ROOT_SCRIPTS_STAGED"
    RELEASE_ROOT_SCRIPTS_STAGED=""
fi

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

if [ -f "$REL_LAUNCHD_ENV_FILE" ]; then
    echo "▸ Syncing release launchd env..."
    sync_launchd_plist_environment_from_file "$HOME/Library/LaunchAgents/$PLIST_REL.plist" "$REL_LAUNCHD_ENV_FILE"
fi

# Start release
echo "▸ Starting release..."
xattr -d com.apple.quarantine "$HOME/Library/LaunchAgents/$PLIST_REL.plist" 2>/dev/null || true
LAUNCHD_DOMAIN="$(_launchd_domain)"
if ! launchctl bootstrap "$LAUNCHD_DOMAIN" "$HOME/Library/LaunchAgents/$PLIST_REL.plist"; then
    echo "⚠ launchd bootstrap failed for $LAUNCHD_DOMAIN/$PLIST_REL — using tmux fallback"
    start_release_tmux_fallback
fi

# Health check (server health + dashboard availability)
REL_PORT="$(_resolve_release_server_port)"
echo "▸ Waiting for release health on :${REL_PORT}..."
REL_HEALTHY=false
# #4348 Defect 1: the trailing `1` opts the DEPLOY readiness gate into treating a
# serving node that is unhealthy SOLELY because no provider runtimes are
# registered (leader-only / no-agent-session node) as deploy-ready. Runtime
# /api/health keeps reporting unhealthy for monitoring; only this gate relaxes.
if wait_for_http_service_health "$PLIST_REL" "$REL_PORT" "$DEPLOY_HEALTH_RETRIES" "$DEPLOY_HEALTH_DELAY_SECS" 1 1 1; then
    REL_HEALTHY=true
fi

if [ "$REL_HEALTHY" != true ]; then
    echo "✗ Release health check failed after $DEPLOY_HEALTH_RETRIES attempts — check logs: $ADK_REL/logs/"
    # #3858: do NOT roll back inline here. DEPLOY_OK stays unset, so the EXIT trap
    # (_rollback_release_binary, armed at promotion) restores the previous good
    # binary and restarts the service on this exit — the SAME path that covers any
    # other post-promotion failure. Unifying them guarantees a single rollback (no
    # double restore) and identical recovery whether the failure is the health
    # check or an unguarded post-promotion command crash.
    exit 1
fi

# #3858: health passed — the new binary is proven good and serving. Mark the
# deploy successful FIRST so the EXIT-trap rollback is disarmed BEFORE we drop the
# backup below — otherwise a failure between here and the backup removal would try
# to roll back with no .prev, and a hiccup in a non-critical step (lock, manifest)
# must never tear down a healthy, health-confirmed binary.
DEPLOY_OK=1
# Lock it against unsigned overwrites (deferred from promotion) and drop the
# now-unneeded rollback backup. chflags is best-effort: failing to re-lock a
# healthy serving binary must not fail the deploy.
chflags uchg "$REL_BINARY" 2>/dev/null || true
chflags nouchg "$REL_BINARY_BACKUP" 2>/dev/null || true
rm -f "$REL_BINARY_BACKUP" 2>/dev/null || true
# #3858 (re-review finding 1): also drop any stray atomic-backup temp so an
# interrupted prior backup copy never lingers in bin/.
rm -f "$REL_BINARY_BACKUP.tmp" 2>/dev/null || true

if _health_json_unhealthy_only_no_provider_runtimes "${WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON:-}"; then
    echo "✓ Release is serving on :${REL_PORT} (deploy-ready: no provider runtimes registered —"
    echo "  leader-only / no-agent-session node; runtime /api/health stays unhealthy for"
    echo "  monitoring, but the server, DB, and dashboard are up [#4348])"
elif _health_json_field_exists "${WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON:-}" "fully_recovered" \
  && ! _health_json_field_is_true "${WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON:-}" "fully_recovered"; then
    echo "✓ Release is serving on :${REL_PORT} (startup recovery still in progress)"
elif _health_json_reconcile_only "${WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON:-}"; then
    echo "✓ Release is serving on :${REL_PORT} (provider reconcile in progress)"
else
    echo "✓ Release is healthy on :${REL_PORT}"
fi

# ── Post-deploy functional smoke (#4262) ─────────────────────────────────────
# Named and intentionally local to this stage so the dashboard API contract is
# easy to edit. Every path is a confirmed GET route under src/server/routes/;
# /api/claude-accounts is the functional surface whose 502 exposed #4126.
POST_DEPLOY_SMOKE_CORE_API_ENDPOINTS=(
    "/api/health"
    "/api/health/detail"
    "/api/agents"
    "/api/sessions"
    "/api/claude-accounts"
    "/api/docs"
)
POST_DEPLOY_SMOKE_LOG_LINES="${AGENTDESK_POST_DEPLOY_SMOKE_LOG_LINES:-500}"
POST_DEPLOY_SMOKE_WARN_LIMIT="${AGENTDESK_POST_DEPLOY_SMOKE_WARN_LIMIT:-5}"
POST_DEPLOY_SMOKE_WEDGE_SETTLE_SECS=4
POST_DEPLOY_SMOKE_RELAY_CELL="${AGENTDESK_POST_DEPLOY_SMOKE_RELAY_CELL:-claude-tui}"
POST_DEPLOY_SMOKE_CREATE_ISSUE="${AGENTDESK_POST_DEPLOY_SMOKE_CREATE_ISSUE:-off}"
POST_DEPLOY_SMOKE_STAMP="$(date -u '+%Y%m%dT%H%M%SZ' 2>/dev/null || printf 'unknown-%s' "$$")"
POST_DEPLOY_SMOKE_EVIDENCE="$ADK_REL/logs/post-deploy-smoke-${POST_DEPLOY_SMOKE_STAMP}.log"
POST_DEPLOY_SMOKE_TMP_DIR=""
POST_DEPLOY_SMOKE_HEALTH_BODY=""
POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY=""
POST_DEPLOY_SMOKE_SESSIONS_BODY=""
POST_DEPLOY_SMOKE_FAILURES=()

_post_deploy_smoke_note() {
    local message="$1"
    printf '%s\n' "$message"
    printf '%s\n' "$message" >> "$POST_DEPLOY_SMOKE_EVIDENCE" || return 1
}

_post_deploy_smoke_fail() {
    local finding="$1"
    POST_DEPLOY_SMOKE_FAILURES+=("$finding")
    _post_deploy_smoke_note "FAIL: $finding" || return 1
    return 1
}

_post_deploy_smoke_probe_apis() {
    local endpoint body_path http_code
    local failed=0

    for endpoint in "${POST_DEPLOY_SMOKE_CORE_API_ENDPOINTS[@]}"; do
        body_path="$POST_DEPLOY_SMOKE_TMP_DIR/${endpoint//\//_}.json"
        if http_code=$(curl -sS --connect-timeout 2 --max-time 15 \
            -H "Origin: http://${ADK_DEFAULT_LOOPBACK}:${REL_PORT}" \
            -o "$body_path" -w '%{http_code}' \
            "http://${ADK_DEFAULT_LOOPBACK}:${REL_PORT}${endpoint}"); then
            _post_deploy_smoke_note "api endpoint=${endpoint} status=${http_code}" || return 1
        else
            _post_deploy_smoke_fail "core API ${endpoint}: curl failed" || true
            failed=1
            continue
        fi
        if [ "$endpoint" = "/api/health" ]; then
            POST_DEPLOY_SMOKE_HEALTH_BODY="$body_path"
        elif [ "$endpoint" = "/api/health/detail" ]; then
            POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY="$body_path"
        elif [ "$endpoint" = "/api/sessions" ]; then
            POST_DEPLOY_SMOKE_SESSIONS_BODY="$body_path"
        fi
        if [ "$http_code" != "200" ]; then
            _post_deploy_smoke_fail "core API ${endpoint}: expected HTTP 200, got ${http_code}" || true
            failed=1
        elif [ ! -s "$body_path" ]; then
            _post_deploy_smoke_fail "core API ${endpoint}: HTTP 200 body was empty" || true
            failed=1
        fi
    done

    [ "$failed" -eq 0 ]
}

_post_deploy_smoke_wedge_markers_from_file() {
    local health_detail_path="$1"
    # Reuse the health/detail markers consumed by the relay E2E validator:
    # explicit non-healthy relay_stall_state, ownerless/detached inflight,
    # desync, stale thread proof, or stale watcher attachment. Ordinary active
    # turns and queues are deliberately not classified as wedges.
    jq -r '
        [
          .degraded_reasons[]?
          | strings
          | select(test("relay.*(wedge|dead|stall|stuck)|(?:wedge|dead|stall|stuck).*relay"; "i"))
          | "degraded_reason=" + .
        ] + [
          .mailboxes[]?
          | . as $mailbox
          | (($mailbox.relay_stall_state // "healthy") | ascii_downcase) as $stall
          | select(
              ($stall != "" and $stall != "healthy")
              or ($mailbox.relay_health.desynced == true)
              or ($mailbox.relay_health.stale_thread_proof == true)
              or ($mailbox.relay_health.watcher_attached_stale == true)
              or (
                $mailbox.inflight_state_present == true
                and (($mailbox.relay_owner_kind // "none") | ascii_downcase) as $owner
                | ($owner == "" or $owner == "none" or $owner == "unknown")
              )
              or (
                $mailbox.inflight_state_present == true
                and $mailbox.watcher_attached == false
              )
            )
          | "mailbox provider=\($mailbox.provider // "unknown") channel=\($mailbox.channel_id // "unknown") stall=\($stall)"
        ]
        | .[]
    ' "$health_detail_path" 2>> "$POST_DEPLOY_SMOKE_EVIDENCE"
}

_post_deploy_smoke_fully_recovered_from_file() {
    local health_path="$1"
    jq -r '
        if (.fully_recovered | type) == "boolean" then
            .fully_recovered
        else
            error("fully_recovered is not boolean")
        end
    ' "$health_path" 2>> "$POST_DEPLOY_SMOKE_EVIDENCE"
}

_post_deploy_smoke_check_wedges() {
    local fully_recovered wedge_markers wedge_markers_resampled persistent_markers wedge_summary
    local resample_path="$POST_DEPLOY_SMOKE_TMP_DIR/api_health_detail_resample.json"
    if [ -z "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY" ] \
      || [ ! -s "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY" ]; then
        _post_deploy_smoke_fail "relay wedge check: /api/health/detail body unavailable" || true
        return 1
    fi

    if ! fully_recovered=$(
        _post_deploy_smoke_fully_recovered_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
    ); then
        _post_deploy_smoke_note \
            "relay wedge=skipped: startup recovery state unavailable" || return 1
        return 0
    fi
    if [ "$fully_recovered" = "false" ]; then
        _post_deploy_smoke_note \
            "relay wedge=skipped: startup recovery in progress" || return 1
        return 0
    fi

    if ! wedge_markers=$(
        _post_deploy_smoke_wedge_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
    ); then
        _post_deploy_smoke_fail "relay wedge check: health/detail JSON could not be parsed" || true
        return 1
    fi
    if [ -z "$wedge_markers" ]; then
        _post_deploy_smoke_note "relay wedge markers=absent" || return 1
        return 0
    fi

    _post_deploy_smoke_note \
        "relay wedge marker observed; settling ${POST_DEPLOY_SMOKE_WEDGE_SETTLE_SECS}s before resample" \
        || return 1
    sleep "$POST_DEPLOY_SMOKE_WEDGE_SETTLE_SECS"
    if ! curl -fsS --connect-timeout 2 --max-time 15 \
        -H "Origin: http://${ADK_DEFAULT_LOOPBACK}:${REL_PORT}" \
        -o "$resample_path" \
        "http://${ADK_DEFAULT_LOOPBACK}:${REL_PORT}/api/health/detail"; then
        _post_deploy_smoke_note \
            "relay wedge=skipped: settle resample unavailable" || return 1
        return 0
    fi
    if [ ! -s "$resample_path" ]; then
        _post_deploy_smoke_note \
            "relay wedge=skipped: settle resample body empty" || return 1
        return 0
    fi
    if ! fully_recovered=$(_post_deploy_smoke_fully_recovered_from_file "$resample_path"); then
        _post_deploy_smoke_note \
            "relay wedge=skipped: settle recovery state unavailable" || return 1
        return 0
    fi
    if [ "$fully_recovered" = "false" ]; then
        _post_deploy_smoke_note \
            "relay wedge=skipped: startup recovery in progress" || return 1
        return 0
    fi
    if ! wedge_markers_resampled=$(
        _post_deploy_smoke_wedge_markers_from_file "$resample_path"
    ); then
        _post_deploy_smoke_note \
            "relay wedge=skipped: settle resample JSON could not be parsed" || return 1
        return 0
    fi
    if ! persistent_markers=$(comm -12 \
        <(printf '%s\n' "$wedge_markers" | LC_ALL=C sort -u) \
        <(printf '%s\n' "$wedge_markers_resampled" | LC_ALL=C sort -u)); then
        _post_deploy_smoke_note \
            "relay wedge=skipped: settle resample comparison failed" || return 1
        return 0
    fi
    if [ -z "$persistent_markers" ]; then
        _post_deploy_smoke_note \
            "relay wedge markers=cleared after ${POST_DEPLOY_SMOKE_WEDGE_SETTLE_SECS}s settle" \
            || return 1
        return 0
    fi

    wedge_summary="${persistent_markers//$'\n'/; }"
    _post_deploy_smoke_fail \
        "relay wedge marker persisted after ${POST_DEPLOY_SMOKE_WEDGE_SETTLE_SECS}s settle: ${wedge_summary}" \
        || true
    return 1
}

_post_deploy_smoke_check_fail_closed_warn_rate() {
    local log_path="$POST_DEPLOY_SMOKE_LOG_PATH"
    local sample_path="$POST_DEPLOY_SMOKE_TMP_DIR/recent-dcserver.log"
    local current_log_stat current_inode current_size current_head_fingerprint
    local fingerprint_bytes sample_start_byte
    local sampled_lines warn_lines fail_closed_warns
    case "$POST_DEPLOY_SMOKE_LOG_LINES" in
        ''|*[!0-9]*)
            _post_deploy_smoke_fail "fail-closed WARN sample: invalid line count ${POST_DEPLOY_SMOKE_LOG_LINES}" || true
            return 1
            ;;
    esac
    case "$POST_DEPLOY_SMOKE_WARN_LIMIT" in
        ''|*[!0-9]*)
            _post_deploy_smoke_fail "fail-closed WARN sample: invalid threshold ${POST_DEPLOY_SMOKE_WARN_LIMIT}" || true
            return 1
            ;;
    esac
    if [ "$POST_DEPLOY_SMOKE_LOG_LINES" -eq 0 ] || [ "$POST_DEPLOY_SMOKE_WARN_LIMIT" -lt 2 ]; then
        _post_deploy_smoke_fail "fail-closed WARN sample: require lines > 0 and threshold >= 2" || true
        return 1
    fi
    if [ ! -r "$log_path" ]; then
        _post_deploy_smoke_fail "fail-closed WARN sample: unreadable log ${log_path}" || true
        return 1
    fi
    case "$POST_DEPLOY_SMOKE_LOG_OFFSET" in
        ''|*[!0-9]*)
            _post_deploy_smoke_fail "fail-closed WARN sample: restart log watermark unavailable" || true
            return 1
            ;;
    esac
    case "$POST_DEPLOY_SMOKE_LOG_INODE" in
        ''|*[!0-9]*)
            _post_deploy_smoke_fail "fail-closed WARN sample: restart log identity unavailable" || true
            return 1
            ;;
    esac
    if [ -z "$POST_DEPLOY_SMOKE_LOG_FINGERPRINT" ]; then
        _post_deploy_smoke_fail "fail-closed WARN sample: restart log fingerprint unavailable" || true
        return 1
    fi
    if ! current_log_stat=$(
        _post_deploy_smoke_log_identity_and_size "$log_path" 2>/dev/null
    ); then
        _post_deploy_smoke_fail "fail-closed WARN sample: could not stat current log ${log_path}" || true
        return 1
    fi
    read -r current_inode current_size <<< "$current_log_stat"
    case "$current_inode:$current_size" in
        *[!0-9:]*|:*|*:)
            _post_deploy_smoke_fail "fail-closed WARN sample: invalid current log identity or size" || true
            return 1
            ;;
    esac
    fingerprint_bytes="$POST_DEPLOY_SMOKE_LOG_OFFSET"
    if [ "$fingerprint_bytes" -gt "$POST_DEPLOY_SMOKE_LOG_FINGERPRINT_CAP" ]; then
        fingerprint_bytes="$POST_DEPLOY_SMOKE_LOG_FINGERPRINT_CAP"
    fi
    if ! current_head_fingerprint=$(
        _post_deploy_smoke_log_head_fingerprint "$log_path" "$fingerprint_bytes"
    ); then
        _post_deploy_smoke_fail "fail-closed WARN sample: could not fingerprint current log ${log_path}" || true
        return 1
    fi
    if [ "$current_inode" != "$POST_DEPLOY_SMOKE_LOG_INODE" ] \
      || [ "$current_size" -lt "$POST_DEPLOY_SMOKE_LOG_OFFSET" ] \
      || [ "$current_head_fingerprint" != "$POST_DEPLOY_SMOKE_LOG_FINGERPRINT" ]; then
        sample_start_byte=1
    else
        sample_start_byte=$((POST_DEPLOY_SMOKE_LOG_OFFSET + 1))
    fi
    if ! tail -c "+${sample_start_byte}" "$log_path" \
      | tail -n "$POST_DEPLOY_SMOKE_LOG_LINES" > "$sample_path"; then
        _post_deploy_smoke_fail "fail-closed WARN sample: could not read post-restart log lines" || true
        return 1
    fi
    sampled_lines=$(wc -l < "$sample_path" | tr -d ' ') || return 1
    warn_lines=$(awk 'tolower($0) ~ /warn/ { count++ } END { print count + 0 }' "$sample_path") || return 1
    fail_closed_warns=$(awk '
        {
            line = tolower($0)
            if (line ~ /warn/ && line ~ /fail[-_ ]closed/) count++
        }
        END { print count + 0 }
    ' "$sample_path") || return 1
    _post_deploy_smoke_note \
        "fail-closed WARN sample=${sampled_lines} warn_lines=${warn_lines} fail_closed_warns=${fail_closed_warns} threshold=${POST_DEPLOY_SMOKE_WARN_LIMIT}" \
        || return 1
    # A count threshold over a bounded recent window is the density guard:
    # default 5 / 500 lines (1%). It intentionally does not block on one WARN.
    if [ "$fail_closed_warns" -ge "$POST_DEPLOY_SMOKE_WARN_LIMIT" ]; then
        _post_deploy_smoke_fail \
            "fail-closed WARN spike: ${fail_closed_warns} in last ${sampled_lines} post-restart log lines (threshold ${POST_DEPLOY_SMOKE_WARN_LIMIT})" \
            || true
        return 1
    fi
}

_post_deploy_smoke_check_relay_round_trip() {
    local cluster_standby channel_id relay_output relay_log resolve_rc cell_busy cell_guard_rc
    local config_path="$ADK_REL/config/agentdesk.yaml"
    if [ -z "$POST_DEPLOY_SMOKE_HEALTH_BODY" ] || [ ! -s "$POST_DEPLOY_SMOKE_HEALTH_BODY" ]; then
        _post_deploy_smoke_fail "relay E-1: /api/health body unavailable for standby gate" || true
        return 1
    fi
    if ! cluster_standby=$(jq -er '
        if (.cluster_standby | type) == "boolean" then
            .cluster_standby
        else
            error("cluster_standby is not boolean")
        end
    ' "$POST_DEPLOY_SMOKE_HEALTH_BODY" 2>> "$POST_DEPLOY_SMOKE_EVIDENCE"); then
        _post_deploy_smoke_fail "relay E-1: could not prove node is non-standby; round-trip skipped" || true
        return 1
    fi
    if [ "$cluster_standby" = "true" ]; then
        _post_deploy_smoke_note "relay E-1=skipped cluster_standby=true (no standby injection)" || return 1
        return 0
    fi

    # Reuse the #3729 wrapper's config resolver: channel ids remain
    # machine-local agentdesk.yaml data and are never hard-coded here.
    if channel_id=$(python3 - "$REPO" "$config_path" "$POST_DEPLOY_SMOKE_RELAY_CELL" \
        2>> "$POST_DEPLOY_SMOKE_EVIDENCE" <<'PY'
import sys
from pathlib import Path

repo, config, cell = sys.argv[1:]
sys.path.insert(0, str(Path(repo) / "scripts" / "e2e"))
from post_deploy_relay_continuity import SmokeConfigError, load_channel_id_from_config

try:
    channel_id = load_channel_id_from_config(Path(config), cell)
except (FileNotFoundError, SmokeConfigError) as error:
    print(error, file=sys.stderr)
    raise SystemExit(2) from error
except Exception as error:
    print(f"unexpected E2E cell config error: {type(error).__name__}: {error}", file=sys.stderr)
    raise SystemExit(1) from error
print(channel_id)
PY
    ); then
        :
    else
        resolve_rc=$?
        if [ "$resolve_rc" -eq 2 ]; then
            _post_deploy_smoke_note "relay E-1=skipped: no E2E cell configured" || return 1
            return 0
        fi
        _post_deploy_smoke_fail \
            "relay E-1: could not resolve ${POST_DEPLOY_SMOKE_RELAY_CELL} channel from ${config_path}" \
            || true
        return 1
    fi

    # E-1 is a real live turn, so reuse the E2E driver's mailbox/session busy
    # predicates against the authenticated core-probe snapshots before sending.
    # An unreadable snapshot skips the injection fail-open: safety requires
    # proving the target cell idle, while the already-recorded API finding (if
    # any) remains the smoke result.
    if cell_busy=$(python3 - "$REPO" "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY" \
        "$POST_DEPLOY_SMOKE_SESSIONS_BODY" "$POST_DEPLOY_SMOKE_RELAY_CELL" "$channel_id" \
        2>> "$POST_DEPLOY_SMOKE_EVIDENCE" <<'PY'
import json
import sys
from pathlib import Path

repo, health_path, sessions_path, cell, channel_id = sys.argv[1:]
sys.path.insert(0, str(Path(repo) / "scripts" / "e2e"))
import run_tui_relay as cell_driver

try:
    with Path(health_path).open(encoding="utf-8") as handle:
        detail = json.load(handle)
    with Path(sessions_path).open(encoding="utf-8") as handle:
        sessions_payload = json.load(handle)
    mailboxes = detail.get("mailboxes") if isinstance(detail, dict) else None
    if not isinstance(mailboxes, list):
        raise ValueError("health/detail mailboxes is not a list")
    sessions = (
        sessions_payload.get("sessions")
        if isinstance(sessions_payload, dict)
        else sessions_payload
    )
    if not isinstance(sessions, list):
        raise ValueError("sessions payload is not a list")

    provider = cell_driver.cell_provider(cell)
    busy = []
    for mailbox in mailboxes:
        if not isinstance(mailbox, dict):
            continue
        if cell_driver._mailbox_channel_id(mailbox) != str(channel_id):
            continue
        if cell_driver._mailbox_provider(mailbox) != provider:
            continue
        reasons = cell_driver._mailbox_busy_reasons(mailbox)
        if reasons:
            busy.append(
                f"mailbox {provider}:{channel_id} [{', '.join(reasons)}]"
            )

    workspace_substring = cell_driver.cell_workspace_substring(cell)
    for session in sessions:
        if not isinstance(session, dict):
            continue
        status = str(session.get("status") or "").lower()
        if status not in {"turn_active", "turn_busy", "active"}:
            continue
        session_key = str(session.get("session_key") or "")
        session_channel = str(
            session.get("channel_id") or session.get("channelId") or ""
        )
        if session_channel == str(channel_id) or workspace_substring in session_key:
            busy.append(
                f"session {session_key or session_channel or '<unknown>'} status={status}"
            )
except Exception as error:
    print(f"{type(error).__name__}: {error}", file=sys.stderr)
    raise SystemExit(2) from error

if busy:
    print("; ".join(busy))
    raise SystemExit(0)
raise SystemExit(1)
PY
    ); then
        _post_deploy_smoke_note "relay E-1=skipped: foreign active turn on cell" || return 1
        _post_deploy_smoke_note "relay E-1 cell-busy evidence=${cell_busy}" || return 1
        return 0
    else
        cell_guard_rc=$?
        if [ "$cell_guard_rc" -ne 1 ]; then
            _post_deploy_smoke_note "relay E-1=skipped: could not verify E2E cell is idle" || return 1
            return 0
        fi
    fi

    relay_output="$ADK_REL/logs/post-deploy-smoke-relay-${POST_DEPLOY_SMOKE_STAMP}"
    relay_log="$POST_DEPLOY_SMOKE_TMP_DIR/relay-e1.log"
    _post_deploy_smoke_note \
        "relay E-1 cell=${POST_DEPLOY_SMOKE_RELAY_CELL} channel=${channel_id} output=${relay_output}" \
        || return 1
    if ! (
        cd "$REPO" || exit 1
        python3 scripts/e2e/run_tui_relay.py \
            --base-url "http://${ADK_DEFAULT_LOOPBACK}:${REL_PORT}" \
            --cell "$POST_DEPLOY_SMOKE_RELAY_CELL" \
            --channel-id "$channel_id" \
            --scenarios "$REPO/tests/e2e/tui_relay/scenarios" \
            --filter E-1 \
            --output "$relay_output" \
            --queue-runtime-root "$ADK_REL/runtime" \
            --required-agent-mode real_live \
            --required-coverage-class live
    ) > "$relay_log" 2>&1; then
        tail -n 40 "$relay_log" >> "$POST_DEPLOY_SMOKE_EVIDENCE" 2>/dev/null || true
        _post_deploy_smoke_fail "relay E-1 round-trip failed (evidence: ${relay_output})" || true
        return 1
    fi
    tail -n 20 "$relay_log" >> "$POST_DEPLOY_SMOKE_EVIDENCE" 2>/dev/null || true
    _post_deploy_smoke_note "relay E-1 round-trip=pass" || return 1
}

_run_post_deploy_functional_smoke() {
    local failed=0
    mkdir -p "$ADK_REL/logs" || return 1
    : > "$POST_DEPLOY_SMOKE_EVIDENCE" || return 1
    POST_DEPLOY_SMOKE_TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-post-deploy-smoke.XXXXXX") || return 1
    _post_deploy_smoke_note "post-deploy functional smoke start stamp=${POST_DEPLOY_SMOKE_STAMP} port=${REL_PORT}" || return 1

    if ! _post_deploy_smoke_probe_apis; then
        failed=1
    fi
    if ! _post_deploy_smoke_check_wedges; then
        failed=1
    fi
    if ! _post_deploy_smoke_check_fail_closed_warn_rate; then
        failed=1
    fi
    if ! _post_deploy_smoke_check_relay_round_trip; then
        failed=1
    fi
    rm -rf "$POST_DEPLOY_SMOKE_TMP_DIR" 2>/dev/null || true
    POST_DEPLOY_SMOKE_TMP_DIR=""
    [ "$failed" -eq 0 ]
}

_report_post_deploy_smoke_failure() {
    local draft_path="$ADK_REL/logs/post-deploy-smoke-issue-draft-${POST_DEPLOY_SMOKE_STAMP}.md"
    local commit_sha node_name issue_url finding alert_text
    commit_sha=$(git -C "$REPO" rev-parse HEAD 2>/dev/null || printf 'unknown')
    node_name=$(hostname 2>/dev/null || printf 'unknown')

    if ! {
        printf '# Post-deploy functional smoke regression\n\n'
        printf -- '- Detected: `%s`\n' "$POST_DEPLOY_SMOKE_STAMP"
        printf -- '- Node: `%s`\n' "$node_name"
        printf -- '- Commit: `%s`\n' "$commit_sha"
        printf -- '- Port: `%s`\n' "$REL_PORT"
        printf -- '- Evidence: `%s`\n\n' "$POST_DEPLOY_SMOKE_EVIDENCE"
        printf '## Findings\n\n'
        for finding in "${POST_DEPLOY_SMOKE_FAILURES[@]}"; do
            printf -- '- %s\n' "$finding"
        done
        printf '\n## Deploy disposition\n\n'
        printf 'Fail-open: the health-confirmed release was not rolled back and peer propagation/source-manifest work continued.\n'
    } > "$draft_path"; then
        echo "⚠ Post-deploy smoke issue draft write FAILED: $draft_path"
        draft_path="unavailable"
    fi

    alert_text="⚠ post-deploy functional smoke FAILED (fail-open; release remains serving)
node: ${node_name}
commit: ${commit_sha}
draft: ${draft_path}
evidence: ${POST_DEPLOY_SMOKE_EVIDENCE}"
    for finding in "${POST_DEPLOY_SMOKE_FAILURES[@]}"; do
        alert_text="${alert_text}
- ${finding}"
    done
    _notify_channel "$alert_text"

    # Default OFF. The literal `confirmed` is an operator assertion that this
    # is a real regression, not a relay/API flake; only then may automation file.
    if [ "$POST_DEPLOY_SMOKE_CREATE_ISSUE" = "confirmed" ] && [ -f "$draft_path" ]; then
        if command -v gh >/dev/null 2>&1; then
            if issue_url=$(gh issue create \
                --repo itismyfield/AgentDesk \
                --title "ops: post-deploy functional smoke regression (${node_name})" \
                --body-file "$draft_path" 2>> "$POST_DEPLOY_SMOKE_EVIDENCE"); then
                echo "⚠ Post-deploy smoke issue created (confirmed mode): $issue_url"
            else
                echo "⚠ Post-deploy smoke issue creation FAILED; draft retained: $draft_path"
            fi
        else
            echo "⚠ Post-deploy smoke issue creation requested but gh is unavailable; draft retained: $draft_path"
        fi
    elif [ "$POST_DEPLOY_SMOKE_CREATE_ISSUE" != "off" ]; then
        echo "⚠ Ignoring AGENTDESK_POST_DEPLOY_SMOKE_CREATE_ISSUE=${POST_DEPLOY_SMOKE_CREATE_ISSUE}; use literal 'confirmed' or 'off'"
    fi
    return 0
}

echo "▸ Running post-deploy functional smoke (#4262)..."
# INVARIANT: the ENTIRE smoke block is fail-open. We are past DEPLOY_OK, so a
# functional failure must degrade to a loud warning + channel alert + local
# issue draft and let the script continue. It must NEVER roll back, exit 1,
# poison the healthy deploy's exit code, or skip watchdog/PG-tunnel install,
# _write_release_source_manifest, or _deploy_to_all_peers below.
#
# The smoke function runs from an `if` guard, suspending `set -e` within it;
# each fallible step is nevertheless explicitly guarded or carries `|| return`.
if _run_post_deploy_functional_smoke; then
    echo "✓ Post-deploy functional smoke passed (evidence: $POST_DEPLOY_SMOKE_EVIDENCE)"
else
    echo "⚠ POST-DEPLOY FUNCTIONAL SMOKE FAILED — deploy remains healthy (fail-open)"
    echo "  evidence: $POST_DEPLOY_SMOKE_EVIDENCE"
    if [ -n "$POST_DEPLOY_SMOKE_TMP_DIR" ]; then
        rm -rf "$POST_DEPLOY_SMOKE_TMP_DIR" 2>/dev/null || true
        POST_DEPLOY_SMOKE_TMP_DIR=""
    fi
    if [ "${#POST_DEPLOY_SMOKE_FAILURES[@]}" -eq 0 ]; then
        POST_DEPLOY_SMOKE_FAILURES+=("smoke harness failed before recording a functional finding")
    fi
    _report_post_deploy_smoke_failure || true
fi

# ── Out-of-band relay watchdog (#4381) ────────────────────────────────────────
# Deliberately OUTSIDE dcserver's launchd job: the watchdog must survive exactly
# the failures it watches for (dcserver crash-looping on PG loss, #4379). The
# repo is the source of truth — the machine-local prototype (and the 06-29
# relay-gap-watch before it) evaporated because nothing deployed it. Runs after
# DEPLOY_OK on purpose: a failed deploy leaves the previous watchdog untouched.
WATCHDOG_LABEL="com.agentdesk.relay-watchdog"
WATCHDOG_PLIST_PATH="$HOME/Library/LaunchAgents/$WATCHDOG_LABEL.plist"
WATCHDOG_BIN="$ADK_REL/bin/relay-watchdog.py"
WATCHDOG_CONFIG="$ADK_REL/config/relay-watchdog.json"
echo "▸ Installing out-of-band relay watchdog (#4381)..."
if install -m 0755 "$REPO/scripts/relay_watchdog.py" "$WATCHDOG_BIN"; then
    if [ -f "$WATCHDOG_CONFIG" ]; then
        WATCHDOG_PYTHON="$(command -v python3 || echo /usr/bin/python3)"
        # INVARIANT: the ENTIRE watchdog block is fail-open. We are past
        # DEPLOY_OK, so any failure here (permissions, full disk, launchd)
        # must degrade to a loud ⚠ warning and let the script continue —
        # aborting would poison the exit code of a HEALTHY deploy and skip
        # _write_release_source_manifest / _deploy_to_all_peers below.
        # The function body runs from an `if` guard, so `set -e` is suspended
        # inside it; every step therefore carries its own `|| return 1`.
        #
        # Runtime python preflight: relay_watchdog.py declares MIN_PYTHON=3.10
        # and exits 1 below it. If `command -v python3` resolved to the macOS
        # system 3.9, arming the plist would put KeepAlive into a silent ~30s
        # crash-loop — refuse to arm instead (r4 review, PR #4399).
        _xml_escape() {
            # Plist bodies are XML: raw &, <, > (and quotes, for safety) in an
            # operator path would render the plist plutil-invalid and the
            # watchdog silently unarmed (r4 review, PR #4399).
            local s=$1
            s=${s//&/\&amp;}
            s=${s//</\&lt;}
            s=${s//>/\&gt;}
            s=${s//\"/\&quot;}
            s=${s//\'/\&apos;}
            printf '%s' "$s"
        }
        _install_relay_watchdog_plist() {
            local label_x python_x bin_x root_x
            label_x=$(_xml_escape "$WATCHDOG_LABEL") || return 1
            python_x=$(_xml_escape "$WATCHDOG_PYTHON") || return 1
            bin_x=$(_xml_escape "$WATCHDOG_BIN") || return 1
            root_x=$(_xml_escape "$ADK_REL") || return 1
            mkdir -p "$HOME/Library/LaunchAgents" || return 1
            cat > "$WATCHDOG_PLIST_PATH.tmp" <<PLIST_EOF || return 1
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label_x</string>
  <key>ProgramArguments</key>
  <array><string>$python_x</string><string>$bin_x</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>StandardOutPath</key><string>$root_x/logs/relay-watchdog.launchd.out.log</string>
  <key>StandardErrorPath</key><string>$root_x/logs/relay-watchdog.launchd.err.log</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    <key>AGENTDESK_ROOT_DIR</key><string>$root_x</string>
  </dict>
</dict>
</plist>
PLIST_EOF
            # Atomic publish: launchd never sees a half-written plist, and an
            # interrupted write leaves only the .tmp (cleaned by the caller).
            mv -f "$WATCHDOG_PLIST_PATH.tmp" "$WATCHDOG_PLIST_PATH" || return 1
        }
        if ! "$WATCHDOG_PYTHON" -c 'import sys; raise SystemExit(0 if sys.version_info >= (3, 10) else 1)' 2>/dev/null; then
            echo "⚠ Relay watchdog requires python3 >= 3.10 (MIN_PYTHON in relay_watchdog.py);"
            echo "  resolved runner: $WATCHDOG_PYTHON — NOT armed (arming would KeepAlive-crash-loop)."
            echo "  Install a newer python3 (e.g. brew install python) and redeploy."
        elif _install_relay_watchdog_plist; then
            xattr -d com.apple.quarantine "$WATCHDOG_PLIST_PATH" 2>/dev/null || true
            # bootout+bootstrap (not kickstart) so a script/plist change is picked up.
            launchctl bootout "$LAUNCHD_DOMAIN/$WATCHDOG_LABEL" 2>/dev/null || true
            if launchctl bootstrap "$LAUNCHD_DOMAIN" "$WATCHDOG_PLIST_PATH"; then
                echo "✓ Relay watchdog armed ($WATCHDOG_LABEL)"
            else
                echo "⚠ Relay watchdog bootstrap FAILED — relay gaps will go unwatched"
            fi
        else
            rm -f "$WATCHDOG_PLIST_PATH.tmp" 2>/dev/null || true
            echo "⚠ Relay watchdog plist write FAILED ($WATCHDOG_PLIST_PATH) — not armed"
            echo "  Deploy continues (fail-open): fix permissions/disk space and redeploy."
        fi
    else
        echo "⚠ Relay watchdog config missing: $WATCHDOG_CONFIG"
        echo "  Watchdog NOT armed on this node. Channel ids are operator config"
        echo "  (never hardcoded in the repo); create the config — see the"
        echo "  scripts/relay_watchdog.py docstring — then redeploy."
    fi
else
    echo "⚠ Relay watchdog staging FAILED (source: $REPO/scripts/relay_watchdog.py)"
fi

# ── Consumer-owned PostgreSQL SSH tunnel supervisor (#4378) ──────────────────
# This is deliberately a separate launchd lifetime from dcserver: dcserver may
# crash-loop while PG is absent (#4379), but the process that restores PG must
# remain alive.  Like the relay watchdog above, this block is after DEPLOY_OK
# and entirely fail-open so an ops-side install failure cannot turn a healthy,
# health-confirmed binary deploy into a rollback or skip peer propagation.
PG_TUNNEL_LABEL="com.agentdesk.pg-tunnel"
PG_TUNNEL_PLIST_PATH="$HOME/Library/LaunchAgents/$PG_TUNNEL_LABEL.plist"
PG_TUNNEL_BIN="$ADK_REL/bin/pg-tunnel.sh"
PG_TUNNEL_CONFIG="$ADK_REL/config/pg-tunnel.env"
echo "▸ Staging consumer-owned PG tunnel supervisor (#4378)..."
if install -m 0755 "$REPO/scripts/pg_tunnel.sh" "$PG_TUNNEL_BIN"; then
    # Machine-local config is the node-identity gate.  It is intentionally not
    # shipped by the repo: mac-mini and future nodes must never arm a tunnel
    # pointed back at themselves merely because cluster deploy propagated.
    if [ -f "$PG_TUNNEL_CONFIG" ]; then
        _pg_xml_escape() {
            local s=$1
            s=${s//&/\&amp;}
            s=${s//</\&lt;}
            s=${s//>/\&gt;}
            s=${s//\"/\&quot;}
            s=${s//\'/\&apos;}
            printf '%s' "$s"
        }
        _install_pg_tunnel_plist() {
            local label_x bin_x config_x root_x
            label_x=$(_pg_xml_escape "$PG_TUNNEL_LABEL") || return 1
            bin_x=$(_pg_xml_escape "$PG_TUNNEL_BIN") || return 1
            config_x=$(_pg_xml_escape "$PG_TUNNEL_CONFIG") || return 1
            root_x=$(_pg_xml_escape "$ADK_REL") || return 1
            mkdir -p "$HOME/Library/LaunchAgents" || return 1
            cat > "$PG_TUNNEL_PLIST_PATH.tmp" <<PLIST_EOF || return 1
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label_x</string>
  <key>ProgramArguments</key>
  <array>
    <string>$bin_x</string>
    <string>$config_x</string>
    <string>-N</string>
    <string>-T</string>
    <string>-o</string><string>BatchMode=yes</string>
    <string>-o</string><string>ConnectTimeout=10</string>
    <string>-o</string><string>ServerAliveInterval=15</string>
    <string>-o</string><string>ServerAliveCountMax=3</string>
    <string>-o</string><string>ExitOnForwardFailure=yes</string>
    <string>-L</string><string>127.0.0.1:15432:127.0.0.1:5432</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>$root_x/logs/pg-tunnel.launchd.out.log</string>
  <key>StandardErrorPath</key><string>$root_x/logs/pg-tunnel.launchd.err.log</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/usr/bin:/bin:/usr/sbin:/sbin</string>
    <key>AGENTDESK_ROOT_DIR</key><string>$root_x</string>
  </dict>
</dict>
</plist>
PLIST_EOF
            # Atomic publish: launchd never observes a partially-written XML.
            mv -f "$PG_TUNNEL_PLIST_PATH.tmp" "$PG_TUNNEL_PLIST_PATH" || return 1
        }
        if ! "$PG_TUNNEL_BIN" --check-config "$PG_TUNNEL_CONFIG"; then
            echo "⚠ PG tunnel config invalid: $PG_TUNNEL_CONFIG — NOT armed"
            echo "  Required: PG_TUNNEL_SSH_TARGET=mac-mini (or PG_TUNNEL_HOST alias)."
        elif _install_pg_tunnel_plist; then
            xattr -d com.apple.quarantine "$PG_TUNNEL_PLIST_PATH" 2>/dev/null || true
            echo "⚠ PG tunnel deploy prerequisite: on mac-mini, bootout and remove BOTH"
            echo "  reverse plists (com.agentdesk.macbook-pg-tunnel and"
            echo "  com.agentdesk.macbook-memento-tunnel) before this job is activated."
            # bootout+bootstrap (not kickstart): pick up both wrapper and plist.
            launchctl bootout "$LAUNCHD_DOMAIN/$PG_TUNNEL_LABEL" 2>/dev/null || true
            if launchctl bootstrap "$LAUNCHD_DOMAIN" "$PG_TUNNEL_PLIST_PATH"; then
                echo "✓ PG tunnel supervisor armed ($PG_TUNNEL_LABEL)"
            else
                echo "⚠ PG tunnel bootstrap FAILED — dcserver PG path is unsupervised"
            fi
        else
            rm -f "$PG_TUNNEL_PLIST_PATH.tmp" 2>/dev/null || true
            echo "⚠ PG tunnel plist write FAILED ($PG_TUNNEL_PLIST_PATH) — not armed"
            echo "  Deploy continues (fail-open): fix permissions/disk space and redeploy."
        fi
    else
        echo "▸ PG tunnel config absent: $PG_TUNNEL_CONFIG"
        echo "  Supervisor NOT armed on this node (machine-local node gate)."
    fi
else
    echo "⚠ PG tunnel staging FAILED (source: $REPO/scripts/pg_tunnel.sh)"
fi

_write_release_source_manifest

echo "═══ Deploy Complete ═══"

if [ "$DEPLOY_ALL_NODES" = "1" ]; then
    _deploy_to_all_peers "$@"
fi
