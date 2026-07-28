#!/usr/bin/env bash
set -euo pipefail

# --- macOS: always run detached (decouple from the invoking shell/session) ---
# On macOS the deploy restarts the release dcserver mid-run. When invoked from a
# tmux/agent session's shell, that restart can perturb the caller and, worse,
# tie the deploy's lifetime to a session that is itself being restarted. Re-exec
# under nohup, detached from the controlling terminal and job table, so the
# deploy always runs to completion independently of the caller. Opt out with
# AGENTDESK_DEPLOY_NO_DETACH=1 (e.g. to stream logs in the foreground).
if [[ "$(uname)" == "Darwin" \
      && "${AGENTDESK_DEPLOY_DETACHED:-0}" != "1" \
      && "${AGENTDESK_DEPLOY_PEER_INVOCATION:-0}" != "1" \
      && "${AGENTDESK_DEPLOY_NO_DETACH:-0}" != "1" ]]; then
    _adk_deploy_log="${AGENTDESK_DEPLOY_LOG:-$HOME/.adk/release/logs/deploy-release.$$.log}"
    mkdir -p "$(dirname "$_adk_deploy_log")" 2>/dev/null || true
    AGENTDESK_DEPLOY_DETACHED=1 nohup "$0" "$@" >"$_adk_deploy_log" 2>&1 </dev/null &
    _adk_deploy_pid=$!
    disown "$_adk_deploy_pid" 2>/dev/null || true
    echo "▸ [detach] macOS deploy re-launched detached (pid ${_adk_deploy_pid})"
    echo "▸ [detach] log: ${_adk_deploy_log}"
    exit 0
fi

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
#   AGENTDESK_DEPLOY_BUNDLE_MANIFEST   required JSON generation manifest when
#                                      AGENTDESK_DEPLOY_BINARY is set. It binds
#                                      source git SHA, binary SHA256, and both
#                                      repository routine asset tree digests.
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
# shellcheck source=routine-asset-surface.sh
. "$SCRIPT_DIR/routine-asset-surface.sh"

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
CODESIGN_IDENTITY="${AGENTDESK_CODESIGN_IDENTITY:-Developer ID Application: Wonchang Oh (A7LJY7HNGA)}"
ALLOW_ADHOC_RELEASE_SIGN="${AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN:-0}"
CODESIGN_KEYCHAIN_PW_FILE="${AGENTDESK_CODESIGN_KEYCHAIN_PW_FILE:-}"
CODESIGN_KEYCHAIN_NAME="${AGENTDESK_CODESIGN_KEYCHAIN_NAME:-agentdesk-codesign.keychain}"
CODESIGN_KEYCHAIN_UNLOCKED=0
RESOLVED_RELEASE_SIGNING_MODE=""
DASHBOARD_SOURCE=""
STAGED_BINARY=""
POLICIES_STAGED=""
ROUTINE_ASSET_TXN=""
REL_ROLLBACK_MATERIAL_MODE=""
LEGACY_ROUTINE_HELPERS_SENTINEL_NAME=".agentdesk-legacy-empty-v1"
ROUTINE_ASSET_INCOMING="${AGENTDESK_ROUTINE_ASSET_INCOMING:-}"
ROUTINE_ASSET_INCOMING_CLAIMED=0
LAUNCHD_MIGRATED_STAGED=""
RELEASE_ROOT_SCRIPTS_STAGED=""
PG_TUNNEL_PREFLIGHT_PID=""
PG_TUNNEL_PREFLIGHT_CONNINFO_DIR=""
PG_TUNNEL_PREFLIGHT_PASSWORD_FILE=""
PG_TUNNEL_ROLLBACK_ARMED=0
PG_TUNNEL_ROLLBACK_DIR=""
PG_TUNNEL_ROLLBACK_JOB_LOADED=0
PG_TUNNEL_ROLLBACK_MANUAL_KIND="none"
PG_TUNNEL_ROLLBACK_MANUAL_CONFIG=""
PG_TUNNEL_ROLLBACK_WRAPPER_SOURCE=""
FORWARD_MIGRATION_APPLIED=0
FORWARD_MIGRATION_RECOVERY_STATE=none
FORWARD_MIGRATION_CANDIDATE_SHA=""
FORWARD_MIGRATION_CLASSIFIED_STATE=""
FORWARD_ROLLBACK_MIGRATION=""
FORWARD_TARGET_MIGRATION=""
RELEASE_BINARY_FLAG_SNAPSHOT=0
RELEASE_BINARY_OLD_IMMUTABLE=0
RELEASE_CANDIDATE_PID=""
RELEASE_CANDIDATE_IDENTITY=""
RELEASE_CANDIDATE_CAPTURED=0
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
ROLLBACK_ARMED=0
RELEASE_SERVICE_RECOVERY_ARMED=0
RELEASE_SERVICE_STOP_CONFIRMED=0
RELEASE_SERVICE_RESTART_SAFE=0

# Parse flags non-destructively into shell vars + env so the detached-helper
# tmux script sees the same configuration without reconstructing $@.
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

echo "═══ ADK Deploy → Release ═══"

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

# #4727: pure-shell fallback for `server.port` when python3 lacks PyYAML.
# Non-interactive SSH (peer/mac-mini deploy) may resolve a system python3 without
# PyYAML on PATH even though the interactive shell's homebrew python3 has it.
# Parses the simple top-level `server:` mapping agentdesk.yaml uses:
#   server:
#     port: 8791
# Emits the DIRECT-child `server.port` only — matching PyYAML's
# `config['server']['port']` semantics: it locks onto the server block's
# child-indent level (set by the block's first child) and accepts a `port:`
# only at exactly that indent, so a deeper `server.tls.port` is never mis-picked.
# The value must be a clean integer after stripping a trailing `# comment` and
# one surrounding quote pair; any non-digit residue (e.g. `8791abc`, `"87-91"`)
# is rejected → no output → caller fails closed. Range is checked by the caller.
_extract_yaml_server_port_shell() {
    local path="$1"
    [ -f "$path" ] || return 1
    awk -v sq="'" '
        function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s); return s }
        # Top-level key line (column 0, non-comment) delimits the server block.
        /^[^[:space:]#]/ { in_server = ($0 ~ /^server:[[:space:]]*(#.*)?$/); child_indent = -1; next }
        in_server {
            if ($0 ~ /^[[:space:]]*$/) next          # blank line
            if ($0 ~ /^[[:space:]]*#/) next          # comment line
            match($0, /^ */); ind = RLENGTH          # leading-space count (YAML forbids tabs)
            if (child_indent == -1) child_indent = ind   # first child fixes the direct-child level
            if (ind != child_indent) next            # deeper (grandchild) or shallower — not server.port
            if ($1 != "port:") next
            v = $0
            sub(/#.*/, "", v)                        # strip inline comment
            sub(/^[[:space:]]*port:[[:space:]]*/, "", v)  # strip the key
            v = trim(v)
            # strip a single matching surrounding quote pair (double or single)
            if (v ~ /^".*"$/) v = substr(v, 2, length(v) - 2)
            else if (length(v) >= 2 && substr(v, 1, 1) == sq && substr(v, length(v), 1) == sq) v = substr(v, 2, length(v) - 2)
            v = trim(v)
            if (v ~ /^[0-9]+$/) print v              # clean integer only — else reject
            exit                                     # exactly one server.port; valid or not, stop
        }
    ' "$path"
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

    if python3 -c 'import yaml' >/dev/null 2>&1; then
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

        echo "✗ Cannot resolve server.port from $config_path: invalid or unreadable configuration; aborting deploy" >&2
        return 1
    fi

    # #4727: PyYAML-less python3 on the resolved PATH (typically a peer deploy over
    # non-interactive SSH). Fall back to a pure-shell parse so the deploy no longer
    # depends on which python3 is first on PATH.
    if configured_port=$(_extract_yaml_server_port_shell "$config_path") \
        && [ -n "$configured_port" ] \
        && [ "$configured_port" -ge 1 ] 2>/dev/null \
        && [ "$configured_port" -le 65535 ] 2>/dev/null; then
        echo "▸ Resolved server.port=$configured_port from $config_path via shell fallback (python3 PyYAML unavailable)" >&2
        printf '%s\n' "$configured_port"
        return 0
    fi

    echo "✗ Cannot resolve server.port from $config_path: python3 PyYAML unavailable and shell fallback could not read server.port; aborting deploy" >&2
    return 1
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

    [ -n "$path" ] || return 0
    adk_sha256_file "$path"
}

_sha256_tree() {
    adk_sha256_tree "$1"
}

_validate_external_deploy_bundle() {
    [ -n "${AGENTDESK_DEPLOY_BINARY:-}" ] || return 0

    local manifest="${AGENTDESK_DEPLOY_BUNDLE_MANIFEST:-}"
    local source_sha binary_sha routines_sha helpers_sha inputs_sha
    if [ -z "$manifest" ]; then
        echo "✗ AGENTDESK_DEPLOY_BINARY requires AGENTDESK_DEPLOY_BUNDLE_MANIFEST" >&2
        echo "  binary-only overrides cannot prove binary/routine asset generation identity" >&2
        return 1
    fi
    if [ ! -f "$manifest" ] || [ -L "$manifest" ]; then
        echo "✗ External deploy bundle manifest is missing or unsafe: $manifest" >&2
        return 1
    fi
    if [ ! -f "$SOURCE_BINARY" ] || [ -L "$SOURCE_BINARY" ]; then
        echo "✗ External deploy binary is missing or symlinked: $SOURCE_BINARY" >&2
        return 1
    fi
    source_sha="$(git -C "$REPO" rev-parse HEAD 2>/dev/null || true)"
    inputs_sha="$(adk_executable_input_digest "$REPO")"
    binary_sha="$(_sha256_file "$SOURCE_BINARY")"
    routines_sha="$(_sha256_tree "$REPO/routines")"
    helpers_sha="$(_sha256_tree "$REPO/routine-helpers")"
    if [ -z "$source_sha" ] || [ -z "$inputs_sha" ] || [ -z "$binary_sha" ] \
      || [ -z "$routines_sha" ] || [ -z "$helpers_sha" ]; then
        echo "✗ Could not resolve external deploy generation identity" >&2
        return 1
    fi
    AGENTDESK_BUNDLE_SOURCE_SHA="$source_sha" \
    AGENTDESK_BUNDLE_INPUTS_SHA="$inputs_sha" \
    AGENTDESK_BUNDLE_BINARY_SHA="$binary_sha" \
    AGENTDESK_BUNDLE_ROUTINES_SHA="$routines_sha" \
    AGENTDESK_BUNDLE_HELPERS_SHA="$helpers_sha" \
    python3 - "$manifest" <<'PY' || {
import json
import os
import re
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        data = json.load(handle)
except Exception as exc:
    print(f"invalid JSON: {exc}", file=sys.stderr)
    raise SystemExit(1)
expected = {
    "source_git_sha": os.environ["AGENTDESK_BUNDLE_SOURCE_SHA"],
    "executable_inputs_sha256": os.environ["AGENTDESK_BUNDLE_INPUTS_SHA"],
    "binary_sha256": os.environ["AGENTDESK_BUNDLE_BINARY_SHA"],
    "routines_sha256": os.environ["AGENTDESK_BUNDLE_ROUTINES_SHA"],
    "routine_helpers_sha256": os.environ["AGENTDESK_BUNDLE_HELPERS_SHA"],
}
if data.get("format") != "agentdesk-release-bundle-v2":
    print("unsupported bundle manifest format", file=sys.stderr)
    raise SystemExit(1)
for key, value in expected.items():
    recorded = data.get(key)
    if not isinstance(recorded, str) or not re.fullmatch(r"[0-9a-f]+", recorded):
        print(f"invalid bundle manifest field: {key}", file=sys.stderr)
        raise SystemExit(1)
    if recorded != value:
        print(f"bundle manifest generation mismatch: {key}", file=sys.stderr)
        raise SystemExit(1)
PY
        echo "✗ External deploy bundle manifest did not match binary/current assets" >&2
        return 1
    }
    DEPLOY_EXECUTABLE_INPUT_SHA="$inputs_sha"
    echo "▸ External deploy bundle generation verified: ${source_sha:0:12}"
}

_write_release_source_manifest() {
    mkdir -p "$ADK_REL/runtime"

    local manifest_tmp="$ADK_REL/runtime/release-source.json.new"
    local manifest_path="$ADK_REL/runtime/release-source.json"
    local generated_at repo_head repo_branch repo_upstream repo_upstream_sha repo_dirty latest_migration latest_migration_name latest_migration_sha
    local manifest_binary binary_sha routines_sha helpers_sha inputs_sha

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
    manifest_binary="${REL_BINARY:-${SOURCE_BINARY:-}}"
    binary_sha="$(_sha256_file "$manifest_binary")"
    inputs_sha="$(adk_executable_input_digest "$REPO")"
    # These are bundle-generation digests, so bind the repository payload that
    # travels with the binary. Operator-preserved runtime files intentionally do
    # not alter the reusable source bundle manifest.
    routines_sha="$(_sha256_tree "$REPO/routines")"
    helpers_sha="$(_sha256_tree "$REPO/routine-helpers")"

    AGENTDESK_MANIFEST_GENERATED_AT="$generated_at" \
    AGENTDESK_MANIFEST_REPO="$REPO" \
    AGENTDESK_MANIFEST_REPO_BRANCH="$repo_branch" \
    AGENTDESK_MANIFEST_REPO_HEAD="$repo_head" \
    AGENTDESK_MANIFEST_INPUTS_SHA="$inputs_sha" \
    AGENTDESK_MANIFEST_REPO_UPSTREAM="$repo_upstream" \
    AGENTDESK_MANIFEST_REPO_UPSTREAM_SHA="$repo_upstream_sha" \
    AGENTDESK_MANIFEST_REPO_DIRTY="$repo_dirty" \
    AGENTDESK_MANIFEST_SOURCE_BINARY="${SOURCE_BINARY:-}" \
    AGENTDESK_MANIFEST_BUILD_PROFILE="$DEPLOY_BUILD_PROFILE" \
    AGENTDESK_MANIFEST_LATEST_MIGRATION="$latest_migration_name" \
    AGENTDESK_MANIFEST_LATEST_MIGRATION_SHA="$latest_migration_sha" \
    AGENTDESK_MANIFEST_BINARY_SHA="$binary_sha" \
    AGENTDESK_MANIFEST_ROUTINES_SHA="$routines_sha" \
    AGENTDESK_MANIFEST_HELPERS_SHA="$helpers_sha" \
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
    "format": "agentdesk-release-bundle-v2",
    "generated_at": os.environ.get("AGENTDESK_MANIFEST_GENERATED_AT", ""),
    "repo_path": os.environ.get("AGENTDESK_MANIFEST_REPO", ""),
    "repo_branch": os.environ.get("AGENTDESK_MANIFEST_REPO_BRANCH", ""),
    "repo_head": os.environ.get("AGENTDESK_MANIFEST_REPO_HEAD", ""),
    "source_git_sha": os.environ.get("AGENTDESK_MANIFEST_REPO_HEAD", ""),
    "executable_inputs_sha256": os.environ.get("AGENTDESK_MANIFEST_INPUTS_SHA", ""),
    "repo_upstream": os.environ.get("AGENTDESK_MANIFEST_REPO_UPSTREAM", ""),
    "repo_upstream_sha": os.environ.get("AGENTDESK_MANIFEST_REPO_UPSTREAM_SHA", ""),
    "repo_dirty": os.environ.get("AGENTDESK_MANIFEST_REPO_DIRTY", "unknown"),
    "source_binary": os.environ.get("AGENTDESK_MANIFEST_SOURCE_BINARY", ""),
    "build_profile": os.environ.get("AGENTDESK_MANIFEST_BUILD_PROFILE", ""),
    "latest_postgres_migration": os.environ.get("AGENTDESK_MANIFEST_LATEST_MIGRATION", ""),
    "latest_postgres_migration_sha256": os.environ.get("AGENTDESK_MANIFEST_LATEST_MIGRATION_SHA", ""),
    "binary_sha256": os.environ.get("AGENTDESK_MANIFEST_BINARY_SHA", ""),
    "routines_sha256": os.environ.get("AGENTDESK_MANIFEST_ROUTINES_SHA", ""),
    "routine_helpers_sha256": os.environ.get("AGENTDESK_MANIFEST_HELPERS_SHA", ""),
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

_manifest_source_git_sha() {
    local manifest="$ADK_REL/runtime/release-source.json"
    [ -f "$manifest" ] || return 1
    python3 - "$manifest" <<'PY' 2>/dev/null
import json
import re
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        data = json.load(handle)
except Exception:
    raise SystemExit(1)
value = data.get("source_git_sha") or data.get("repo_head") or ""
if not re.fullmatch(r"[0-9a-fA-F]{40,64}", value):
    raise SystemExit(1)
print(value.lower())
PY
}

_manifest_routine_helpers_binding_state() {
    # A missing field identifies releases created before routine helpers became
    # a generation-bound surface. Once the field exists, an absent live helper
    # tree is corruption and must never be normalized as a legacy release.
    local manifest="$ADK_REL/runtime/release-source.json"
    [ -f "$manifest" ] && [ ! -L "$manifest" ] || return 1
    python3 - "$manifest" <<'PY' 2>/dev/null
import json
import re
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        data = json.load(handle)
except Exception:
    raise SystemExit(2)
if "routine_helpers_sha256" not in data:
    print("legacy-unbound")
    raise SystemExit(0)
value = data.get("routine_helpers_sha256")
if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]+", value):
    raise SystemExit(2)
print("bound")
PY
}

_release_migration_name_is_valid() {
    python3 - "$1" <<'PY' >/dev/null 2>&1
import re
import sys

raise SystemExit(
    0 if re.fullmatch(r"[0-9]{4}_[A-Za-z0-9._-]+\.sql", sys.argv[1]) else 1
)
PY
}

_validate_legacy_routine_helpers_sentinel() {
    local helper_root="$1"
    local source_git_sha="$2"
    local latest_migration="$3"
    local sentinel="$helper_root/$LEGACY_ROUTINE_HELPERS_SENTINEL_NAME"

    [ -d "$helper_root" ] && [ ! -L "$helper_root" ] \
        && [ -f "$sentinel" ] && [ ! -L "$sentinel" ] \
        || return 1
    _adk_assert_no_symlink_tree "$helper_root" || return 1
    if find "$helper_root" -mindepth 1 -maxdepth 1 \
        ! -name "$LEGACY_ROUTINE_HELPERS_SENTINEL_NAME" -print -quit \
        | grep -q .; then
        return 1
    fi
    python3 - "$sentinel" "$source_git_sha" "$latest_migration" <<'PY' \
        >/dev/null 2>&1
import sys

path, source_git_sha, latest_migration = sys.argv[1:]
expected = (
    "format=agentdesk-legacy-empty-routine-helpers-v1\n"
    f"source_git_sha={source_git_sha}\n"
    f"latest_postgres_migration={latest_migration}\n"
)
with open(path, encoding="utf-8") as handle:
    actual = handle.read()
raise SystemExit(0 if actual == expected else 1)
PY
}

_normalize_legacy_release_routine_helpers() {
    local helper_root="$ADK_REL/routine-helpers"
    local source_git_sha="$1"
    local latest_migration="$2"
    local binding_state="$3"
    local sentinel="$helper_root/$LEGACY_ROUTINE_HELPERS_SENTINEL_NAME"
    local staged_legacy_root root_mode

    if [ -d "$helper_root" ] && [ ! -L "$helper_root" ]; then
        if [ -e "$sentinel" ] || [ -L "$sentinel" ]; then
            [ "$binding_state" = legacy-unbound ] \
                && _validate_legacy_routine_helpers_sentinel \
                    "$helper_root" "$source_git_sha" "$latest_migration" \
                || {
                    echo "✗ Legacy routine-helper sentinel does not match release-source.json" >&2
                    return 1
                }
        else
            _adk_assert_no_symlink_tree "$helper_root" || return 1
        fi
        return 0
    fi
    if [ -e "$helper_root" ] || [ -L "$helper_root" ]; then
        echo "✗ Refusing unsafe live routine-helper surface: $helper_root" >&2
        return 1
    fi
    if [ "$binding_state" != legacy-unbound ]; then
        echo "✗ release-source.json binds routine helpers, but the live surface is missing" >&2
        return 1
    fi
    [ -n "${ROUTINE_ASSET_TXN:-}" ] && [ -d "$ROUTINE_ASSET_TXN" ] \
        && [ ! -L "$ROUTINE_ASSET_TXN" ] || return 1
    staged_legacy_root="$ROUTINE_ASSET_TXN/legacy-empty-routine-helpers"
    [ ! -e "$staged_legacy_root" ] || return 1
    root_mode="$(_adk_path_mode "$ADK_REL/routines")" || return 1
    mkdir "$staged_legacy_root" || return 1
    if ! printf '%s\n' \
        'format=agentdesk-legacy-empty-routine-helpers-v1' \
        "source_git_sha=$source_git_sha" \
        "latest_postgres_migration=$latest_migration" \
        > "$staged_legacy_root/$LEGACY_ROUTINE_HELPERS_SENTINEL_NAME" \
      || ! chmod "$root_mode" "$staged_legacy_root" \
      || ! chmod 600 "$staged_legacy_root/$LEGACY_ROUTINE_HELPERS_SENTINEL_NAME" \
      || ! mv "$staged_legacy_root" "$helper_root"; then
        rm -rf "$staged_legacy_root" 2>/dev/null || true
        return 1
    fi
    _validate_legacy_routine_helpers_sentinel \
        "$helper_root" "$source_git_sha" "$latest_migration" || return 1
    echo "▸ Normalized legacy release with explicit empty routine-helper sentinel"
}

_strip_legacy_helper_sentinel_from_staged_generation() {
    local staged_root="$1"
    local sentinel="$staged_root/$LEGACY_ROUTINE_HELPERS_SENTINEL_NAME"

    [ -d "$staged_root" ] && [ ! -L "$staged_root" ] || return 1
    if [ -L "$sentinel" ] || { [ -e "$sentinel" ] && [ ! -f "$sentinel" ]; }; then
        echo "✗ Refusing unsafe reserved helper sentinel in staged generation" >&2
        return 1
    fi
    rm -f "$sentinel" || return 1
    adk_validate_routine_helper_surface "$staged_root"
}

_write_rollback_backup_metadata() {
    local backup_binary="$1"
    local metadata_path="$2"
    local asset_txn="${3:-${ROUTINE_ASSET_TXN:-}}"
    local binary_sha latest_migration source_git_sha routines_sha helpers_sha txn_id

    [ -f "$backup_binary" ] && [ ! -L "$backup_binary" ] \
        && [ ! -L "$metadata_path" ] || return 1
    [ -n "$asset_txn" ] || return 1
    txn_id="$(basename "$asset_txn")"
    case "$txn_id" in
        routine-assets.txn.*) ;;
        *) return 1 ;;
    esac
    case "${txn_id#routine-assets.txn.}" in
        ''|*[!A-Za-z0-9]*) return 1 ;;
    esac
    binary_sha="$(_sha256_file "$backup_binary")"
    [ -n "$binary_sha" ] || return 1
    latest_migration="$(_manifest_latest_migration_name 2>/dev/null || true)"
    source_git_sha="$(_manifest_source_git_sha 2>/dev/null || true)"
    routines_sha="$(_sha256_tree "$ADK_REL/routines")" || return 1
    helpers_sha="$(_sha256_tree "$ADK_REL/routine-helpers")" || return 1
    [ -n "$latest_migration" ] && [ -n "$source_git_sha" ] \
        && [ -n "$routines_sha" ] && [ -n "$helpers_sha" ] || return 1
    AGENTDESK_BACKUP_BINARY_SHA256="$binary_sha" \
    AGENTDESK_BACKUP_LATEST_MIGRATION="$latest_migration" \
    AGENTDESK_BACKUP_SOURCE_GIT_SHA="$source_git_sha" \
    AGENTDESK_BACKUP_ASSET_TXN="$txn_id" \
    AGENTDESK_BACKUP_ROUTINES_SHA256="$routines_sha" \
    AGENTDESK_BACKUP_HELPERS_SHA256="$helpers_sha" \
    python3 - "$metadata_path" <<'PY'
import json
import os
import sys

payload = {
    "format_version": 2,
    "binary_sha256": os.environ["AGENTDESK_BACKUP_BINARY_SHA256"],
    "source_git_sha": os.environ["AGENTDESK_BACKUP_SOURCE_GIT_SHA"],
    "asset_transaction": os.environ["AGENTDESK_BACKUP_ASSET_TXN"],
    "routines_sha256": os.environ["AGENTDESK_BACKUP_ROUTINES_SHA256"],
    "routine_helpers_sha256": os.environ["AGENTDESK_BACKUP_HELPERS_SHA256"],
    "latest_postgres_migration": os.environ.get(
        "AGENTDESK_BACKUP_LATEST_MIGRATION", ""
    ),
}
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    json.dump(payload, handle, sort_keys=True)
    handle.write("\n")
PY
}

_rollback_backup_latest_migration_name() {
    # Read migration compatibility from metadata cryptographically bound to
    # the exact .prev bytes. The mutable live release manifest is never used to
    # reason about a preserved backup from an older deploy generation.
    local backup_binary="${REL_BINARY_BACKUP:-}"
    local metadata_path="${REL_BINARY_BACKUP_META:-${backup_binary}.meta}"
    local transaction_mode="${1:-current}"
    local binary_sha routines_sha helpers_sha expected_txn expected_source_sha
    local routines_root helpers_root

    [ -n "$backup_binary" ] \
        && [ -f "$backup_binary" ] \
        && [ ! -L "$backup_binary" ] \
        && [ -f "$metadata_path" ] \
        && [ ! -L "$metadata_path" ] || return 1
    binary_sha="$(_sha256_file "$backup_binary")"
    [ -n "$binary_sha" ] || return 1
    if [ -d "$ADK_REL/routines.old" ] && [ -d "$ADK_REL/routine-helpers.old" ]; then
        routines_root="$ADK_REL/routines.old"
        helpers_root="$ADK_REL/routine-helpers.old"
    elif [ -d "$ADK_REL/routines" ] && [ -d "$ADK_REL/routine-helpers" ]; then
        routines_root="$ADK_REL/routines"
        helpers_root="$ADK_REL/routine-helpers"
    else
        return 1
    fi
    routines_sha="$(_sha256_tree "$routines_root")" || return 1
    helpers_sha="$(_sha256_tree "$helpers_root")" || return 1
    expected_source_sha="$(_manifest_source_git_sha 2>/dev/null || true)"
    [ -n "$expected_source_sha" ] || return 1
    expected_txn=""
    if [ "$transaction_mode" = current ]; then
        [ -n "${ROUTINE_ASSET_TXN:-}" ] || return 1
        expected_txn="$(basename "$ROUTINE_ASSET_TXN")"
    elif [ "$transaction_mode" != allow-prior ]; then
        return 1
    fi
    AGENTDESK_BACKUP_ACTUAL_SHA256="$binary_sha" \
    AGENTDESK_BACKUP_ACTUAL_ROUTINES_SHA256="$routines_sha" \
    AGENTDESK_BACKUP_ACTUAL_HELPERS_SHA256="$helpers_sha" \
    AGENTDESK_BACKUP_EXPECTED_TXN="$expected_txn" \
    AGENTDESK_BACKUP_EXPECTED_SOURCE_SHA="$expected_source_sha" \
    python3 - "$metadata_path" <<'PY' 2>/dev/null
import json
import os
import re
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        data = json.load(handle)
except Exception:
    sys.exit(1)
if data.get("format_version") != 2:
    sys.exit(1)
if data.get("binary_sha256") != os.environ["AGENTDESK_BACKUP_ACTUAL_SHA256"]:
    sys.exit(1)
if data.get("routines_sha256") != os.environ["AGENTDESK_BACKUP_ACTUAL_ROUTINES_SHA256"]:
    sys.exit(1)
if data.get("routine_helpers_sha256") != os.environ["AGENTDESK_BACKUP_ACTUAL_HELPERS_SHA256"]:
    sys.exit(1)
source_sha = data.get("source_git_sha") or ""
expected_source_sha = os.environ["AGENTDESK_BACKUP_EXPECTED_SOURCE_SHA"]
if not re.fullmatch(r"[0-9a-f]{40,64}", source_sha) or source_sha != expected_source_sha:
    sys.exit(1)
transaction = data.get("asset_transaction") or ""
if not re.fullmatch(r"routine-assets\.txn\.[A-Za-z0-9]+", transaction):
    sys.exit(1)
expected_transaction = os.environ.get("AGENTDESK_BACKUP_EXPECTED_TXN") or ""
if expected_transaction and transaction != expected_transaction:
    sys.exit(1)
migration = data.get("latest_postgres_migration") or ""
if not re.fullmatch(r"[0-9]{4}_[A-Za-z0-9._-]+\.sql", migration):
    sys.exit(1)
print(migration)
PY
}

_release_host_uses_immutable_flags() {
    [ "$(uname 2>/dev/null || true)" = Darwin ]
}

_release_binary_immutable_state() {
    local path="$1"
    local flags

    [ -f "$path" ] && [ ! -L "$path" ] || return 1
    if ! _release_host_uses_immutable_flags; then
        printf '0\n'
        return 0
    fi
    flags="$(stat -f '%Sf' "$path" 2>/dev/null)" || return 1
    case ",$flags," in
        *,uchg,*|*,uimmutable,*) printf '1\n' ;;
        *) printf '0\n' ;;
    esac
}

_snapshot_release_binary_immutable_flag() {
    local state=0

    RELEASE_BINARY_FLAG_SNAPSHOT=0
    RELEASE_BINARY_OLD_IMMUTABLE=0
    if [ -e "$REL_BINARY" ]; then
        state="$(_release_binary_immutable_state "$REL_BINARY")" || return 1
        case "$state" in
            0|1) ;;
            *) return 1 ;;
        esac
        RELEASE_BINARY_OLD_IMMUTABLE="$state"
    fi
    RELEASE_BINARY_FLAG_SNAPSHOT=1
}

_set_release_binary_immutable_state() {
    local path="$1"
    local desired="$2"
    local actual

    [ "$desired" = 0 ] || [ "$desired" = 1 ] || return 1
    [ -f "$path" ] && [ ! -L "$path" ] || return 1
    if ! _release_host_uses_immutable_flags; then
        return 0
    fi
    if [ "$desired" = 1 ]; then
        chflags uchg "$path" || return 1
    else
        chflags nouchg "$path" || return 1
    fi
    actual="$(_release_binary_immutable_state "$path")" || return 1
    [ "$actual" = "$desired" ]
}

_restore_release_binary_immutable_flag() {
    [ "${RELEASE_BINARY_FLAG_SNAPSHOT:-0}" = 1 ] || return 1
    _set_release_binary_immutable_state \
        "$REL_BINARY" "${RELEASE_BINARY_OLD_IMMUTABLE:-0}"
}

_rollback_material_mode_path() {
    printf '%s/rollback-material-mode\n' "$1"
}

_durable_rollback_material_mode() {
    local txn_root="$1"
    local path mode

    path="$(_rollback_material_mode_path "$txn_root")" || return 1
    [ -f "$path" ] && [ ! -L "$path" ] || return 1
    mode="$(sed -n '1p' "$path")" || return 1
    case "$mode" in preserve|capture|none) ;; *) return 1 ;; esac
    printf '%s\n' "$mode"
}

_persist_rollback_material_mode() {
    local txn_root="$1"
    local mode="$2"

    _adk_assert_active_txn "$ADK_REL" "$txn_root" || return 1
    case "$mode" in preserve|capture|none) ;; *) return 1 ;; esac
    _adk_write_atomic_file \
        "$(_rollback_material_mode_path "$txn_root")" "$mode" \
        rollback-material-mode
}

_restore_rollback_material_mode() {
    local mode

    mode="$(_durable_rollback_material_mode "$1")" || return 1
    REL_ROLLBACK_MATERIAL_MODE="$mode"
}

_prepare_release_rollback_generation() {
    # Decide every rollback-artifact branch while the old service is still
    # running. In particular, a pre-helper release gets one explicit empty
    # sentinel surface so backup metadata can bind and later restore that exact
    # legacy generation instead of failing only after launchd has been stopped.
    local source_git_sha latest_migration binding_state routines_sha helpers_sha
    local rollback_path preflight_binary preflight_metadata

    REL_ROLLBACK_MATERIAL_MODE=""
    [ -n "${REL_BINARY:-}" ] \
        && [ -n "${REL_BINARY_BACKUP:-}" ] \
        && [ -n "${REL_BINARY_BACKUP_META:-}" ] || return 1

    for rollback_path in \
        "$REL_BINARY_BACKUP" \
        "$REL_BINARY_BACKUP_META" \
        "$REL_BINARY_BACKUP.tmp" \
        "$REL_BINARY_BACKUP_META.tmp"; do
        if [ -L "$rollback_path" ]; then
            echo "✗ Refusing symlinked rollback artifact before release stop: $rollback_path" >&2
            return 1
        fi
        if [ -e "$rollback_path" ] && [ ! -f "$rollback_path" ]; then
            echo "✗ Refusing non-regular rollback artifact before release stop: $rollback_path" >&2
            return 1
        fi
    done
    if [ -L "$REL_BINARY" ] \
      || { [ -e "$REL_BINARY" ] && [ ! -f "$REL_BINARY" ]; }; then
        echo "✗ Refusing unsafe live release binary before release stop: $REL_BINARY" >&2
        return 1
    fi
    if ! _snapshot_release_binary_immutable_flag; then
        echo "✗ Could not snapshot the live release binary immutable flag" >&2
        return 1
    fi
    if [ -f "$REL_BINARY_BACKUP" ] && [ ! -f "$REL_BINARY_BACKUP_META" ]; then
        echo "✗ Existing rollback binary has no generation metadata" >&2
        return 1
    fi

    # Atomic-copy leftovers and metadata without its binary are never rollback
    # authority. Normalize them while the old release is still healthy.
    chflags nouchg "$REL_BINARY_BACKUP.tmp" "$REL_BINARY_BACKUP_META.tmp" \
        "$REL_BINARY_BACKUP_META" 2>/dev/null || true
    rm -f "$REL_BINARY_BACKUP.tmp" "$REL_BINARY_BACKUP_META.tmp" || return 1
    if [ ! -f "$REL_BINARY_BACKUP" ] && [ -f "$REL_BINARY_BACKUP_META" ]; then
        rm -f "$REL_BINARY_BACKUP_META" || return 1
    fi

    if [ -f "$REL_BINARY_BACKUP" ]; then
        REL_ROLLBACK_MATERIAL_MODE="preserve"
    elif [ -f "$REL_BINARY" ]; then
        REL_ROLLBACK_MATERIAL_MODE="capture"
    else
        REL_ROLLBACK_MATERIAL_MODE="none"
    fi
    if ! _persist_rollback_material_mode \
        "$ROUTINE_ASSET_TXN" "$REL_ROLLBACK_MATERIAL_MODE"; then
        REL_ROLLBACK_MATERIAL_MODE=""
        return 1
    fi
    [ "$REL_ROLLBACK_MATERIAL_MODE" != none ] || return 0

    source_git_sha="$(_manifest_source_git_sha 2>/dev/null || true)"
    latest_migration="$(_manifest_latest_migration_name 2>/dev/null || true)"
    if [ -z "$source_git_sha" ] \
      || [ -z "$latest_migration" ] \
      || ! _release_migration_name_is_valid "$latest_migration"; then
        echo "✗ Current release-source.json cannot bind rollback metadata" >&2
        REL_ROLLBACK_MATERIAL_MODE=""
        return 1
    fi
    if ! binding_state="$(_manifest_routine_helpers_binding_state)"; then
        echo "✗ Current release-source.json has an invalid routine-helper binding" >&2
        REL_ROLLBACK_MATERIAL_MODE=""
        return 1
    fi
    if [ ! -d "$ADK_REL/routines" ] || [ -L "$ADK_REL/routines" ]; then
        echo "✗ Current release has no safe routine surface to bind for rollback" >&2
        REL_ROLLBACK_MATERIAL_MODE=""
        return 1
    fi
    _adk_assert_no_symlink_tree "$ADK_REL/routines" || return 1
    if ! _normalize_legacy_release_routine_helpers \
        "$source_git_sha" "$latest_migration" "$binding_state"; then
        REL_ROLLBACK_MATERIAL_MODE=""
        return 1
    fi
    routines_sha="$(_sha256_tree "$ADK_REL/routines")" || return 1
    helpers_sha="$(_sha256_tree "$ADK_REL/routine-helpers")" || return 1
    if [ -z "$routines_sha" ] || [ -z "$helpers_sha" ]; then
        REL_ROLLBACK_MATERIAL_MODE=""
        return 1
    fi

    if [ "$REL_ROLLBACK_MATERIAL_MODE" = preserve ]; then
        if ! _rollback_backup_latest_migration_name allow-prior >/dev/null; then
            echo "✗ Existing rollback backup is not bound to the current release generation" >&2
            REL_ROLLBACK_MATERIAL_MODE=""
            return 1
        fi
        preflight_binary="$REL_BINARY_BACKUP"
    else
        preflight_binary="$REL_BINARY"
    fi
    # Exercise the exact metadata writer while the old service is still live.
    # The post-stop write remains a JIT snapshot, but schema/path/legacy-surface
    # failures are forced into this no-downtime boundary.
    preflight_metadata="$ROUTINE_ASSET_TXN/rollback-backup.meta.preflight"
    if [ -L "$preflight_metadata" ] \
      || { [ -e "$preflight_metadata" ] && [ ! -f "$preflight_metadata" ]; }; then
        REL_ROLLBACK_MATERIAL_MODE=""
        return 1
    fi
    rm -f "$preflight_metadata" || return 1
    if ! _write_rollback_backup_metadata \
        "$preflight_binary" "$preflight_metadata" "$ROUTINE_ASSET_TXN"; then
        echo "✗ Rollback metadata preflight failed before release stop" >&2
        REL_ROLLBACK_MATERIAL_MODE=""
        return 1
    fi
    return 0
}

_forward_migration_marker_path() {
    printf '%s/forward-migration-applied.json\n' "$1"
}

_verified_preflight_rollback_migration() {
    local txn_root="${1:-${ROUTINE_ASSET_TXN:-}}"
    local metadata_path
    local backup_binary mode migration

    [ -n "$txn_root" ] || return 1
    metadata_path="$txn_root/rollback-backup.meta.preflight"
    mode="${REL_ROLLBACK_MATERIAL_MODE:-}"
    case "$mode" in
        preserve|capture|none) ;;
        *) mode="$(_durable_rollback_material_mode "$txn_root")" || return 1 ;;
    esac
    case "$mode" in
        preserve)
            backup_binary="$REL_BINARY_BACKUP"
            ;;
        capture)
            # Before promotion the preflight metadata binds the live binary.
            # After promotion those exact old bytes live at .prev, so try the
            # durable backup first and then the still-live pre-promotion path.
            for backup_binary in "$REL_BINARY_BACKUP" "$REL_BINARY"; do
                if migration="$(
                    ROUTINE_ASSET_TXN="$txn_root" \
                    REL_BINARY_BACKUP="$backup_binary" \
                    REL_BINARY_BACKUP_META="$metadata_path" \
                        _rollback_backup_latest_migration_name
                )"; then
                    printf '%s\n' "$migration"
                    return 0
                fi
            done
            return 1
            ;;
        none)
            return 1
            ;;
        *) return 1 ;;
    esac
    ROUTINE_ASSET_TXN="$txn_root" \
    REL_BINARY_BACKUP="$backup_binary" \
    REL_BINARY_BACKUP_META="$metadata_path" \
        _rollback_backup_latest_migration_name
}

_classify_forward_migration_relation() {
    local rollback_migration target_path target_migration mode

    target_path="$(_latest_postgres_migration_path 2>/dev/null || true)"
    [ -n "$target_path" ] || return 1
    target_migration="$(basename "$target_path")"
    _release_migration_name_is_valid "$target_migration" || return 1
    mode="${REL_ROLLBACK_MATERIAL_MODE:-}"
    case "$mode" in
        preserve|capture|none) ;;
        *) mode="$(_durable_rollback_material_mode "$ROUTINE_ASSET_TXN")" \
            || return 1 ;;
    esac
    if [ "$mode" = none ]; then
        # A fresh install has no rollback binary. Even when the database was
        # already current, every post-child failure must retain the only
        # compatible candidate generation rather than invent an old rollback.
        FORWARD_ROLLBACK_MIGRATION="$target_migration"
        FORWARD_TARGET_MIGRATION="$target_migration"
        FORWARD_MIGRATION_CLASSIFIED_STATE=advanced
        return 0
    fi
    rollback_migration="$(_verified_preflight_rollback_migration)" || return 1
    _release_migration_name_is_valid "$rollback_migration" || return 1
    FORWARD_ROLLBACK_MIGRATION="$rollback_migration"
    FORWARD_TARGET_MIGRATION="$target_migration"
    if _migration_advanced "$target_migration" "$rollback_migration"; then
        FORWARD_MIGRATION_CLASSIFIED_STATE=advanced
    else
        FORWARD_MIGRATION_CLASSIFIED_STATE=not-advanced
    fi
}

_persist_forward_migration_applied() {
    local txn_root="$1"
    local candidate_sha="$2"
    local migration_state="$3"
    local rollback_migration="$4"
    local target_migration="$5"
    local marker txn_id source_sha candidate_name candidate_suffix

    _adk_assert_active_txn "$ADK_REL" "$txn_root" || return 1
    case "$candidate_sha" in
        ''|*[!0-9a-f]*) return 1 ;;
    esac
    case "$migration_state" in
        advanced|not-advanced|unknown/interrupted) ;;
        *) return 1 ;;
    esac
    _release_migration_name_is_valid "$rollback_migration" || return 1
    _release_migration_name_is_valid "$target_migration" || return 1
    candidate_name="$(basename "${STAGED_BINARY:-}")" || return 1
    case "$candidate_name" in
        agentdesk.deploy.*) candidate_suffix="${candidate_name#agentdesk.deploy.}" ;;
        *) return 1 ;;
    esac
    case "$candidate_suffix" in
        ''|*[!A-Za-z0-9]*) return 1 ;;
    esac
    txn_id="$(basename "$txn_root")" || return 1
    source_sha="$(git -C "$REPO" rev-parse HEAD 2>/dev/null || true)"
    case "$source_sha" in
        ''|*[!0-9a-f]*) return 1 ;;
    esac
    marker="$(_forward_migration_marker_path "$txn_root")" || return 1
    [ ! -L "$marker" ] || return 1
    AGENTDESK_FORWARD_TXN_ID="$txn_id" \
    AGENTDESK_FORWARD_CANDIDATE_SHA="$candidate_sha" \
    AGENTDESK_FORWARD_CANDIDATE_NAME="$candidate_name" \
    AGENTDESK_FORWARD_MIGRATION_STATE="$migration_state" \
    AGENTDESK_FORWARD_ROLLBACK_MIGRATION="$rollback_migration" \
    AGENTDESK_FORWARD_TARGET_MIGRATION="$target_migration" \
    AGENTDESK_FORWARD_SOURCE_SHA="$source_sha" \
    python3 - "$marker" <<'PY'
import json
import os
import tempfile
import sys

marker = sys.argv[1]
payload = {
    "format": "agentdesk-forward-migration-v2",
    "asset_transaction": os.environ["AGENTDESK_FORWARD_TXN_ID"],
    "candidate_binary_sha256": os.environ["AGENTDESK_FORWARD_CANDIDATE_SHA"],
    "candidate_binary_name": os.environ["AGENTDESK_FORWARD_CANDIDATE_NAME"],
    "migration_state": os.environ["AGENTDESK_FORWARD_MIGRATION_STATE"],
    "rollback_latest_postgres_migration": os.environ[
        "AGENTDESK_FORWARD_ROLLBACK_MIGRATION"
    ],
    "target_latest_postgres_migration": os.environ[
        "AGENTDESK_FORWARD_TARGET_MIGRATION"
    ],
    "source_git_sha": os.environ["AGENTDESK_FORWARD_SOURCE_SHA"],
}
fd, temporary = tempfile.mkstemp(
    prefix=".forward-migration.", dir=os.path.dirname(marker)
)
try:
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        json.dump(payload, handle, sort_keys=True)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, marker)
    directory = os.open(os.path.dirname(marker), os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)
finally:
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
PY
}

_fsync_forward_recovery_generation() {
    local txn_root="$1"
    local candidate_binary="$2"
    local staged_root="$txn_root/staged/release-root"
    local rollback_mode

    [ -f "$candidate_binary" ] && [ ! -L "$candidate_binary" ] || return 1
    [ -d "$staged_root/routines" ] && [ ! -L "$staged_root/routines" ] \
        || return 1
    [ -d "$staged_root/routine-helpers" ] \
        && [ ! -L "$staged_root/routine-helpers" ] || return 1
    rollback_mode="$(_durable_rollback_material_mode "$txn_root")" || return 1
    python3 - "$candidate_binary" "$txn_root" "$rollback_mode" <<'PY'
import os
import stat
import sys

candidate, txn_root, rollback_mode = sys.argv[1:]
staged_root = os.path.join(txn_root, "staged", "release-root")
roots = [
    os.path.join(staged_root, "routines"),
    os.path.join(staged_root, "routine-helpers"),
]

def fsync_regular(path):
    mode = os.lstat(path).st_mode
    if stat.S_ISLNK(mode):
        raise OSError(f"symlinked recovery file: {path}")
    if not stat.S_ISREG(mode):
        raise OSError(f"non-regular recovery file: {path}")
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)

fsync_regular(candidate)
fsync_regular(os.path.join(txn_root, "rollback-material-mode"))
if rollback_mode != "none":
    fsync_regular(os.path.join(txn_root, "rollback-backup.meta.preflight"))
for root in roots:
    for current, directories, files in os.walk(root, topdown=False, followlinks=False):
        for name in files:
            fsync_regular(os.path.join(current, name))
        for name in directories:
            path = os.path.join(current, name)
            if os.path.islink(path):
                continue
            fd = os.open(path, os.O_RDONLY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        fd = os.open(current, os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)

for directory in [
    os.path.dirname(candidate),
    staged_root,
    os.path.dirname(staged_root),
    txn_root,
    os.path.dirname(txn_root),
]:
    fd = os.open(directory, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)
PY
    adk_fsync_active_routine_asset_transaction_metadata \
        "$ADK_REL" "$txn_root"
}

_release_executable_inputs_match_built_candidate() {
    local current_inputs

    [ -n "${DEPLOY_EXECUTABLE_INPUT_SHA:-}" ] || return 1
    current_inputs="$(adk_executable_input_digest "$REPO")" || return 1
    [ "$current_inputs" = "$DEPLOY_EXECUTABLE_INPUT_SHA" ]
}

_apply_release_postgres_migration_with_forward_barrier() {
    local candidate_sha final_state

    candidate_sha="$(_sha256_file "$STAGED_BINARY")" || {
        echo "✗ Could not bind the staged candidate before forward migration" >&2
        return 1
    }
    if ! _release_executable_inputs_match_built_candidate; then
        echo "✗ Executable inputs changed before migration classification" >&2
        return 1
    fi
    _fsync_forward_recovery_generation "$ROUTINE_ASSET_TXN" "$STAGED_BINARY" \
        || { echo "✗ Could not durably flush the forward recovery generation" >&2; return 1; }
    if ! _classify_forward_migration_relation; then
        echo "✗ Could not verify rollback/current migration relation" >&2
        return 1
    fi
    final_state="$FORWARD_MIGRATION_CLASSIFIED_STATE"
    FORWARD_MIGRATION_CANDIDATE_SHA="$candidate_sha"
    FORWARD_MIGRATION_RECOVERY_STATE=unknown/interrupted
    FORWARD_MIGRATION_APPLIED=1
    # Persist the compatible candidate before entering the foreground migration
    # command. Bash may deliver TERM immediately after that child returns, before
    # any following assignment can run; the marker is the fail-forward authority
    # across that otherwise unguarded process boundary.
    if ! _persist_forward_migration_applied \
        "$ROUTINE_ASSET_TXN" "$candidate_sha" unknown/interrupted \
        "$FORWARD_ROLLBACK_MIGRATION" "$FORWARD_TARGET_MIGRATION"; then
        echo "✗ Could not persist the forward-migration recovery barrier" >&2
        return 1
    fi
    if ! _release_executable_inputs_match_built_candidate; then
        echo "✗ Executable inputs changed at the migration child boundary" >&2
        return 1
    fi
    if ! "$STAGED_BINARY" release-migrate-postgres; then
        echo "✗ Release PostgreSQL migration failed; retaining the compatible candidate for fail-forward recovery." >&2
        return 1
    fi
    if ! _persist_forward_migration_applied \
        "$ROUTINE_ASSET_TXN" "$candidate_sha" "$final_state" \
        "$FORWARD_ROLLBACK_MIGRATION" "$FORWARD_TARGET_MIGRATION"; then
        echo "✗ Migration completed but its durable relation state could not be published" >&2
        return 1
    fi
    FORWARD_MIGRATION_RECOVERY_STATE="$final_state"
    if [ "$final_state" = advanced ]; then
        FORWARD_MIGRATION_APPLIED=1
    else
        FORWARD_MIGRATION_APPLIED=0
    fi
}

_forward_migration_marker_value() {
    local txn_root="$1"
    local field="$2"
    local marker

    case "$field" in
        candidate_binary_sha256|candidate_binary_name|migration_state|rollback_latest_postgres_migration|target_latest_postgres_migration) ;;
        *) return 1 ;;
    esac
    marker="$(_forward_migration_marker_path "$txn_root")" || return 1
    [ -f "$marker" ] && [ ! -L "$marker" ] || return 1
    AGENTDESK_FORWARD_EXPECTED_TXN="$(basename "$txn_root")" \
    AGENTDESK_FORWARD_MARKER_FIELD="$field" \
    python3 - "$marker" <<'PY' 2>/dev/null
import json
import os
import re
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        data = json.load(handle)
except Exception:
    raise SystemExit(1)
format_name = data.get("format")
if format_name not in ("agentdesk-forward-migration-v1", "agentdesk-forward-migration-v2"):
    raise SystemExit(1)
if data.get("asset_transaction") != os.environ["AGENTDESK_FORWARD_EXPECTED_TXN"]:
    raise SystemExit(1)
candidate_sha = data.get("candidate_binary_sha256") or ""
candidate_name = data.get("candidate_binary_name") or ""
source_sha = data.get("source_git_sha") or ""
if format_name == "agentdesk-forward-migration-v1":
    state = "unknown/interrupted"
    rollback_migration = data.get("latest_postgres_migration") or ""
    target_migration = rollback_migration
else:
    state = data.get("migration_state") or ""
    rollback_migration = data.get("rollback_latest_postgres_migration") or ""
    target_migration = data.get("target_latest_postgres_migration") or ""
if not re.fullmatch(r"[0-9a-f]{64}", candidate_sha):
    raise SystemExit(1)
if not re.fullmatch(r"agentdesk\.deploy\.[A-Za-z0-9]+", candidate_name):
    raise SystemExit(1)
if not re.fullmatch(r"[0-9a-f]{40,64}", source_sha):
    raise SystemExit(1)
if state not in ("advanced", "not-advanced", "unknown/interrupted"):
    raise SystemExit(1)
for migration in (rollback_migration, target_migration):
    if not re.fullmatch(r"[0-9]{4}_[A-Za-z0-9._-]+\.sql", migration):
        raise SystemExit(1)
values = dict(data)
values["migration_state"] = state
values["rollback_latest_postgres_migration"] = rollback_migration
values["target_latest_postgres_migration"] = target_migration
print(values[os.environ["AGENTDESK_FORWARD_MARKER_FIELD"]])
PY
}

_forward_migration_recovery_state() {
    local txn_root="$1"
    local state rollback_migration target_migration verified_rollback mode

    state="$(_forward_migration_marker_value "$txn_root" migration_state)" \
        || return 1
    case "$state" in
        unknown/interrupted) printf '%s\n' "$state"; return 0 ;;
        advanced|not-advanced) ;;
        *) return 1 ;;
    esac
    rollback_migration="$(_forward_migration_marker_value \
        "$txn_root" rollback_latest_postgres_migration)" || return 1
    target_migration="$(_forward_migration_marker_value \
        "$txn_root" target_latest_postgres_migration)" || return 1
    mode="${REL_ROLLBACK_MATERIAL_MODE:-}"
    case "$mode" in
        preserve|capture|none) ;;
        *) mode="$(_durable_rollback_material_mode "$txn_root" 2>/dev/null || true)" ;;
    esac
    if [ "$mode" = none ]; then
        # Durable `none` proves there never was rollback material. Only the
        # conservative fail-forward state is valid for that fresh generation.
        [ "$state" = advanced ] || state=unknown/interrupted
        printf '%s\n' "$state"
        return 0
    fi
    verified_rollback="$(_verified_preflight_rollback_migration \
        "$txn_root" 2>/dev/null || true)"
    if [ -z "$verified_rollback" ] \
      || [ "$verified_rollback" != "$rollback_migration" ]; then
        printf 'unknown/interrupted\n'
        return 0
    fi
    if _migration_advanced "$target_migration" "$verified_rollback"; then
        [ "$state" = advanced ] || state=unknown/interrupted
    else
        [ "$state" = not-advanced ] || state=unknown/interrupted
    fi
    printf '%s\n' "$state"
}

_forward_migration_candidate_sha() {
    _forward_migration_marker_value "$1" candidate_binary_sha256
}

_forward_migration_staged_binary() {
    local candidate_name candidate_path

    candidate_name="$(_forward_migration_marker_value \
        "$1" candidate_binary_name)" || return 1
    candidate_path="$ADK_REL/bin/$candidate_name"
    [ "$(dirname "$candidate_path")" = "$ADK_REL/bin" ] || return 1
    [ ! -L "$candidate_path" ] || return 1
    printf '%s\n' "$candidate_path"
}

_finish_forward_asset_promotion() {
    local txn_root="$1"
    local candidate_binary="$2"
    local phase

    phase="$(adk_routine_asset_transaction_phase "$ADK_REL" "$txn_root")" \
        || return 1
    case "$phase" in
        staging)
            adk_promote_routine_asset_transaction \
                "$ADK_REL" "$txn_root" "$candidate_binary"
            ;;
        armed)
            _adk_finish_promote_surface "$ADK_REL" "$txn_root" routines \
                || return 1
            _adk_finish_promote_surface "$ADK_REL" "$txn_root" routine-helpers \
                || return 1
            _adk_write_phase "$txn_root" promoted
            ;;
        promoted|committing|committed)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

_release_current_service_pid() {
    local domain pid=""
    local lock_file="${LOCK_FILE:-${ADK_REL:-}/runtime/dcserver.lock}"

    domain="${LAUNCHD_DOMAIN:-$(_launchd_domain 2>/dev/null || true)}"
    if [ -n "$domain" ]; then
        pid="$(
            launchctl print "$domain/$PLIST_REL" 2>/dev/null \
                | awk '$1 == "pid" && $2 == "=" { print $3; exit }'
        )"
    fi
    case "$pid" in
        ''|*[!0-9]*|0)
            pid=""
            if [ -f "$lock_file" ] && [ ! -L "$lock_file" ]; then
                pid="$(sed -n '1p' "$lock_file" 2>/dev/null || true)"
            fi
            ;;
    esac
    case "$pid" in
        ''|*[!0-9]*|0) return 1 ;;
    esac
    printf '%s\n' "$pid"
}

_capture_release_candidate_process() {
    local attempt=0
    local max_attempts="${1:-15}"
    local pid identity

    RELEASE_CANDIDATE_PID=""
    RELEASE_CANDIDATE_IDENTITY=""
    RELEASE_CANDIDATE_CAPTURED=0
    while [ "$attempt" -lt "$max_attempts" ]; do
        pid="$(_release_current_service_pid 2>/dev/null || true)"
        identity="$(adk_process_identity "$pid" 2>/dev/null || true)"
        if [ -n "$pid" ] && [ -n "$identity" ] \
          && kill -0 "$pid" 2>/dev/null; then
            RELEASE_CANDIDATE_PID="$pid"
            RELEASE_CANDIDATE_IDENTITY="$identity"
            RELEASE_CANDIDATE_CAPTURED=1
            return 0
        fi
        sleep 1
        attempt=$((attempt + 1))
    done
    echo "✗ Could not capture the exact post-bootstrap release candidate process" >&2
    return 1
}

_persist_release_candidate_drain_authority() {
    local txn_root="$1"
    local pid="${2:-}"
    local identity="${3:-}"
    local domain candidate_sha candidate_path

    domain="${LAUNCHD_DOMAIN:-$(_launchd_domain)}"
    candidate_sha="${FORWARD_MIGRATION_CANDIDATE_SHA:-}"
    if [ -z "$candidate_sha" ]; then
        if [ -n "${STAGED_BINARY:-}" ] && [ -f "$STAGED_BINARY" ]; then
            candidate_path="$STAGED_BINARY"
        else
            candidate_path="${REL_BINARY:-}"
        fi
        candidate_sha="$(_sha256_file "$candidate_path")" || return 1
    fi
    adk_persist_routine_asset_candidate_drain_authority \
        "$ADK_REL" "$txn_root" deploy-release "$pid" "$identity" \
        "${REL_PORT:-$(_resolve_release_server_port)}" "$domain/$PLIST_REL" \
        "$candidate_sha"
}

_release_candidate_command_matches_live_binary() {
    local pid="$1"
    local command_line

    command_line="$(ps -o command= -p "$pid" 2>/dev/null || true)"
    [ -n "$command_line" ] || return 1
    case "$command_line" in
        "${REL_BINARY}"|"${REL_BINARY} "*) return 0 ;;
        *) return 1 ;;
    esac
}

_resume_release_candidate_drain_authority() {
    local txn_root="$1"
    local expected_candidate_sha="$2"
    local capture_state pid identity marker_port supervisor marker_candidate_sha
    local expected_supervisor current_port live_sha

    adk_routine_asset_candidate_drain_authority_exists "$txn_root" || return 0
    capture_state="$(adk_routine_asset_candidate_drain_authority_value \
        "$txn_root" deploy-release capture_state)" || return 1
    marker_port="$(adk_routine_asset_candidate_drain_authority_value \
        "$txn_root" deploy-release port)" || return 1
    supervisor="$(adk_routine_asset_candidate_drain_authority_value \
        "$txn_root" deploy-release supervisor)" || return 1
    marker_candidate_sha="$(adk_routine_asset_candidate_drain_authority_value \
        "$txn_root" deploy-release candidate_binary_sha256)" || return 1
    LAUNCHD_DOMAIN="${LAUNCHD_DOMAIN:-$(_launchd_domain)}"
    expected_supervisor="$LAUNCHD_DOMAIN/$PLIST_REL"
    current_port="${REL_PORT:-$(_resolve_release_server_port)}"
    [ "$supervisor" = "$expected_supervisor" ] \
        && [ "$marker_port" = "$current_port" ] || return 1
    case "$expected_candidate_sha" in
        ''|*[!0-9a-f]*) return 1 ;;
    esac
    [ -z "$marker_candidate_sha" ] \
        || [ "$marker_candidate_sha" = "$expected_candidate_sha" ] || return 1
    live_sha="$(_sha256_file "$REL_BINARY" 2>/dev/null || true)"
    [ "$live_sha" = "$expected_candidate_sha" ] || return 1
    REL_PORT="$current_port"
    if [ "$capture_state" = provisional ]; then
        if _release_launchd_job_is_loaded; then
            _capture_release_candidate_process || return 1
            _release_candidate_command_matches_live_binary \
                "$RELEASE_CANDIDATE_PID" || return 1
            _persist_release_candidate_drain_authority "$txn_root" \
                "$RELEASE_CANDIDATE_PID" "$RELEASE_CANDIDATE_IDENTITY" \
                || return 1
        elif _release_candidate_port_refuses_connections; then
            return 0
        else
            return 1
        fi
    elif [ "$capture_state" = exact ]; then
        pid="$(adk_routine_asset_candidate_drain_authority_value \
            "$txn_root" deploy-release pid)" || return 1
        identity="$(adk_routine_asset_candidate_drain_authority_value \
            "$txn_root" deploy-release identity)" || return 1
        RELEASE_CANDIDATE_PID="$pid"
        RELEASE_CANDIDATE_IDENTITY="$identity"
        RELEASE_CANDIDATE_CAPTURED=1
    else
        return 1
    fi
    _stop_and_drain_release_candidate "$txn_root" || return 1
    RELEASE_CANDIDATE_PID=""
    RELEASE_CANDIDATE_IDENTITY=""
    RELEASE_CANDIDATE_CAPTURED=0
}

_release_candidate_process_is_alive() {
    [ "${RELEASE_CANDIDATE_CAPTURED:-0}" = 1 ] || return 1
    adk_process_instance_alive \
        "${RELEASE_CANDIDATE_PID:-}" \
        "${RELEASE_CANDIDATE_IDENTITY:-}"
}

_release_candidate_port_refuses_connections() {
    local port="${REL_PORT:-${AGENTDESK_REL_PORT:-${ADK_DEFAULT_PORT:-8791}}}"
    local curl_status

    if curl -sS --connect-timeout 1 --max-time 1 -o /dev/null \
        "http://${ADK_DEFAULT_LOOPBACK:-127.0.0.1}:${port}/api/health" \
        >/dev/null 2>&1; then
        return 1
    else
        curl_status=$?
    fi
    [ "$curl_status" -eq 7 ]
}

_capture_release_old_process() {
    local pid identity

    OLD_PID=""
    OLD_PID_IDENTITY=""
    if ! _release_launchd_job_is_loaded \
      && _release_candidate_port_refuses_connections; then
        return 0
    fi
    pid="$(_release_current_service_pid 2>/dev/null || true)"
    identity="$(adk_process_identity "$pid" 2>/dev/null || true)"
    case "$pid" in
        ''|*[!0-9]*|0) return 1 ;;
    esac
    [ -n "$identity" ] && kill -0 "$pid" 2>/dev/null || return 1
    OLD_PID="$pid"
    OLD_PID_IDENTITY="$identity"
}

_stop_and_drain_release_candidate() {
    local txn_root="${1:-${ROUTINE_ASSET_TXN:-}}"
    local domain wait_secs=0

    [ "${RELEASE_CANDIDATE_CAPTURED:-0}" = 1 ] || {
        echo "🛑 Refusing rollback without an exact candidate PID/identity" >&2
        return 1
    }
    domain="${LAUNCHD_DOMAIN:-$(_launchd_domain)}"
    launchctl bootout "$domain/$PLIST_REL" 2>/dev/null || true
    tmux kill-session \
        -t "${AGENTDESK_RELEASE_TMUX_SESSION:-AgentDesk-dcserver-release-manual}" \
        2>/dev/null || true
    while { _release_launchd_job_is_loaded \
          || _release_candidate_process_is_alive \
          || ! _release_candidate_port_refuses_connections; } \
      && [ "$wait_secs" -lt 15 ]; do
        sleep 1
        wait_secs=$((wait_secs + 1))
    done
    if _release_launchd_job_is_loaded \
      || _release_candidate_process_is_alive \
      || ! _release_candidate_port_refuses_connections; then
        if _release_candidate_process_is_alive; then
            kill -TERM "$RELEASE_CANDIDATE_PID" 2>/dev/null || return 1
            wait_secs=0
            while _release_candidate_process_is_alive && [ "$wait_secs" -lt 5 ]; do
                sleep 1
                wait_secs=$((wait_secs + 1))
            done
        fi
        if _release_candidate_process_is_alive; then
            kill -KILL "$RELEASE_CANDIDATE_PID" 2>/dev/null || return 1
        fi
        wait_secs=0
        while { _release_candidate_process_is_alive \
              || ! _release_candidate_port_refuses_connections; } \
          && [ "$wait_secs" -lt 5 ]; do
            sleep 1
            wait_secs=$((wait_secs + 1))
        done
        if _release_launchd_job_is_loaded \
          || _release_candidate_process_is_alive \
          || ! _release_candidate_port_refuses_connections; then
            echo "🛑 Candidate process/port did not drain; old generation remains untouched" >&2
            return 1
        fi
    fi
    if [ -n "$txn_root" ] \
      && adk_routine_asset_candidate_drain_authority_exists "$txn_root" \
      && ! adk_clear_routine_asset_candidate_drain_authority \
        "$ADK_REL" "$txn_root"; then
        echo "🛑 Candidate drained, but its durable drain authority could not be cleared" >&2
        return 1
    fi
    return 0
}

_retire_release_rollback_material() {
    local rollback_tmp

    [ -n "${REL_BINARY_BACKUP:-}" ] \
        && [ -n "${REL_BINARY_BACKUP_META:-}" ] || return 1
    chflags nouchg "$REL_BINARY_BACKUP" 2>/dev/null || true
    if ! rm -f "$REL_BINARY_BACKUP" 2>/dev/null \
      || [ -e "$REL_BINARY_BACKUP" ]; then
        return 1
    fi
    if ! rm -f "$REL_BINARY_BACKUP_META" 2>/dev/null \
      || [ -e "$REL_BINARY_BACKUP_META" ]; then
        return 1
    fi
    for rollback_tmp in \
        "$REL_BINARY_BACKUP.tmp" "$REL_BINARY_BACKUP_META.tmp"; do
        if ! rm -f "$rollback_tmp" 2>/dev/null \
          || [ -e "$rollback_tmp" ]; then
            return 1
        fi
    done
}

_start_forward_migrated_release() {
    local txn_root="$1"
    local domain

    domain="${LAUNCHD_DOMAIN:-$(_launchd_domain)}"
    _persist_release_candidate_drain_authority "$txn_root" || return 1
    xattr -d com.apple.quarantine \
        "$HOME/Library/LaunchAgents/$PLIST_REL.plist" 2>/dev/null || true
    if ! launchctl bootstrap \
        "$domain" "$HOME/Library/LaunchAgents/$PLIST_REL.plist"; then
        echo "⚠ Forward recovery launchd bootstrap failed — using tmux fallback" >&2
        start_release_tmux_fallback || return 1
    fi
    _capture_release_candidate_process || return 1
    _persist_release_candidate_drain_authority "$txn_root" \
        "$RELEASE_CANDIDATE_PID" "$RELEASE_CANDIDATE_IDENTITY" || return 1
    wait_for_http_service_health \
        "$PLIST_REL" "$REL_PORT" "$DEPLOY_HEALTH_RETRIES" \
        "$DEPLOY_HEALTH_DELAY_SECS" 1 1 1
}

_recover_forward_migrated_release() {
    local txn_root="$1"
    local candidate_sha actual_sha candidate_binary durable_staged phase

    candidate_sha="$(_forward_migration_candidate_sha "$txn_root" 2>/dev/null || true)"
    if [ -z "$candidate_sha" ] \
      && [ "${FORWARD_MIGRATION_APPLIED:-0}" = 1 ]; then
        candidate_sha="${FORWARD_MIGRATION_CANDIDATE_SHA:-}"
    fi
    case "$candidate_sha" in
        ''|*[!0-9a-f]*) return 1 ;;
    esac
    phase="$(adk_routine_asset_transaction_phase "$ADK_REL" "$txn_root")" \
        || return 1

    if [ -n "${STAGED_BINARY:-}" ] && [ -f "$STAGED_BINARY" ] \
      && [ ! -L "$STAGED_BINARY" ]; then
        candidate_binary="$STAGED_BINARY"
    elif durable_staged="$(_forward_migration_staged_binary \
        "$txn_root" 2>/dev/null)" \
      && [ -f "$durable_staged" ] && [ ! -L "$durable_staged" ]; then
        STAGED_BINARY="$durable_staged"
        candidate_binary="$STAGED_BINARY"
    elif [ -n "${REL_BINARY:-}" ] && [ -f "$REL_BINARY" ] \
      && [ ! -L "$REL_BINARY" ] && [ "$phase" != staging ]; then
        candidate_binary="$REL_BINARY"
    else
        return 1
    fi
    actual_sha="$(_sha256_file "$candidate_binary")" || return 1
    [ "$actual_sha" = "$candidate_sha" ] || return 1

    _resume_release_candidate_drain_authority "$txn_root" "$candidate_sha" \
        || return 1

    if [ "$candidate_binary" = "${REL_BINARY:-}" ] \
      && ! _release_service_is_stopped \
      && [ "${RELEASE_CANDIDATE_CAPTURED:-0}" != 1 ]; then
        _capture_release_candidate_process || return 1
    fi
    if [ "${RELEASE_CANDIDATE_CAPTURED:-0}" = 1 ]; then
        _stop_and_drain_release_candidate "$txn_root" || return 1
    elif ! _release_service_is_stopped; then
        LAUNCHD_DOMAIN="${LAUNCHD_DOMAIN:-$(_launchd_domain)}"
        _stop_release_for_promotion || return 1
    fi
    if [ "$candidate_binary" = "${STAGED_BINARY:-}" ]; then
        _finish_forward_asset_promotion "$txn_root" "$candidate_binary" \
            || return 1
        if [ -e "$REL_BINARY" ]; then
            _set_release_binary_immutable_state "$REL_BINARY" 0 || return 1
        fi
        mv -f "$candidate_binary" "$REL_BINARY" || return 1
        STAGED_BINARY=""
    elif [ "$phase" = armed ]; then
        _finish_forward_asset_promotion "$txn_root" "$candidate_binary" \
            || return 1
    elif [ "$phase" = staging ]; then
        return 1
    fi

    echo "↪ Forward-only migration applied — retaining compatible candidate generation" >&2
    _start_forward_migrated_release "$txn_root" || return 1
    _set_release_binary_immutable_state "$REL_BINARY" 1 || {
        echo "🛑 Forward candidate is healthy but immutable protection could not be verified" >&2
        return 1
    }
    _retire_release_rollback_material || {
        echo "🛑 Forward candidate is serving, but rollback material could not be retired" >&2
        echo "   retaining the promoted asset transaction for deterministic recovery" >&2
        return 1
    }
    adk_commit_routine_asset_transaction_forward "$ADK_REL" "$txn_root" \
        || return 1
    ROLLBACK_ARMED=0
    return 0
}

_recover_durable_forward_migration_before_new_deploy() {
    local active_txn active_status marker phase recovery_state staged_candidate

    if active_txn="$(_adk_active_txn "$ADK_REL")"; then
        :
    else
        active_status=$?
        [ "$active_status" -eq 1 ] && return 0
        echo "🛑 Existing routine asset transaction marker is corrupt" >&2
        return 1
    fi
    marker="$(_forward_migration_marker_path "$active_txn")" || return 1
    { [ -e "$marker" ] || [ -L "$marker" ]; } || return 0
    if ! _forward_migration_candidate_sha "$active_txn" >/dev/null 2>&1; then
        echo "🛑 Existing forward-migration recovery marker is invalid: $marker" >&2
        return 1
    fi
    # R8 transactions persist this before the forward marker. Legacy v1
    # markers have no mode file and already map to unknown/interrupted, so a
    # missing file may safely degrade to fail-forward instead of blocking them.
    _restore_rollback_material_mode "$active_txn" 2>/dev/null || true
    recovery_state="$(_forward_migration_recovery_state "$active_txn")" || {
        echo "🛑 Existing forward-migration recovery state is invalid: $marker" >&2
        return 1
    }

    echo "↪ Recovering durable forward-migrated generation before new deploy work" >&2
    REL_PORT="${REL_PORT:-$(_resolve_release_server_port)}"
    LOCK_FILE="${LOCK_FILE:-$ADK_REL/runtime/dcserver.lock}"
    phase="$(adk_routine_asset_transaction_phase "$ADK_REL" "$active_txn")" \
        || return 1
    if [ "$recovery_state" = not-advanced ] \
      && ! adk_routine_asset_candidate_drain_authority_exists "$active_txn" \
      && [ "$phase" = staging ]; then
        staged_candidate="$(_forward_migration_staged_binary \
            "$active_txn" 2>/dev/null || true)"
        [ -n "$staged_candidate" ] || return 1
        if ! adk_abort_routine_asset_transaction "$ADK_REL" "$active_txn"; then
            echo "🛑 Safe no-advance transaction abort remains incomplete" >&2
            return 1
        fi
        rm -f "$staged_candidate" || return 1
        FORWARD_MIGRATION_APPLIED=0
        FORWARD_MIGRATION_RECOVERY_STATE=none
        return 0
    fi
    if [ "$recovery_state" = not-advanced ]; then
        echo "⚠ No-advance marker has post-staging runtime state; treating recovery as interrupted" >&2
        recovery_state=unknown/interrupted
    fi
    if [ "$phase" = staging ] && ! _capture_release_old_process; then
        echo "🛑 Could not capture the exact pre-recovery release process" >&2
        return 1
    fi
    if ! _recover_forward_migrated_release "$active_txn"; then
        echo "🛑 Durable forward-migrated generation remains incomplete" >&2
        return 1
    fi
    if _adk_active_txn "$ADK_REL" >/dev/null 2>&1; then
        echo "🛑 Forward recovery returned without closing its asset transaction" >&2
        return 1
    fi
}

_rollback_would_brick_on_migration() {
    # #4348 Defect 2: refuse a rollback that would strand the previous binary
    # behind a migration the new binary already applied to the SHARED Postgres.
    # The old binary aborts boot with "migration N was previously applied but is
    # missing in the resolved migrations", and because the row lives in the
    # shared DB, every OTHER node bricks on its next restart too. Returns 0 =>
    # rollback unsafe (fail-forward); returns 1 => rollback safe. Fails CLOSED on
    # any ambiguity (safety > minimal-change): a rollback must never brick.
    local new_path new_name old_name
    old_name="$(_rollback_backup_latest_migration_name || true)"
    if [ -z "$old_name" ]; then
        echo "  ⚠ [rollback-guard] rollback backup metadata is missing, invalid, or does not match the backup digest — treating rollback as unsafe" >&2
        return 0
    fi
    if [ "${AGENTDESK_DEPLOY_FORCE_ROLLBACK:-0}" = "1" ]; then
        echo "  ▸ [rollback-guard] AGENTDESK_DEPLOY_FORCE_ROLLBACK=1 — backup integrity verified; skipping migration-advance comparison" >&2
        return 1
    fi
    new_path="$(_latest_postgres_migration_path 2>/dev/null || true)"
    if [ -z "$new_path" ]; then
        echo "  ⚠ [rollback-guard] cannot resolve the new binary's latest migration ($REPO/migrations/postgres) — treating rollback as unsafe" >&2
        return 0
    fi
    new_name="$(basename "$new_path")"
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
    local asset_txn="${1:-}"
    local rel_binary="${REL_BINARY:-}"
    local rel_backup="${REL_BINARY_BACKUP:-}"
    local plist="${PLIST_REL:-}"
    local rel_port="${REL_PORT:-${AGENTDESK_REL_PORT:-${ADK_DEFAULT_PORT:-8791}}}"
    local domain
    local flag_restore_ok=1

    [ -n "$rel_binary" ] && [ -n "$plist" ] || return 3
    if [ ! -f "$rel_backup" ]; then
        echo "⚠ No rollback backup available (${rel_backup:-unset} missing) — cannot auto-rollback"
        return 3
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
        return 2
    fi

    echo "↩ Rolling back release binary to previous good version..."
    domain="$(_launchd_domain)" || domain="gui/$(id -u 2>/dev/null)"
    # The exact post-bootstrap candidate instance and its listener must both be
    # gone before old bytes or assets can be restored. Otherwise a draining
    # candidate can keep executing while the rollback generation is published.
    if ! _stop_and_drain_release_candidate "$asset_txn"; then
        return 6
    fi
    if ! _set_release_binary_immutable_state "$rel_binary" 0; then
        echo "🛑 Candidate immutable flag could not be cleared; old generation remains untouched" >&2
        return 6
    fi
    # mv is an atomic same-dir rename: the backup replaces the bad binary in one
    # step — at no instant are both copies gone.
    if ! mv -f "$rel_backup" "$rel_binary"; then
        echo "✗ Failed to restore previous binary from $rel_backup — manual intervention required"
        return 3
    fi
    if [ -n "${REL_BINARY_BACKUP_META:-}" ]; then
        rm -f "$REL_BINARY_BACKUP_META" 2>/dev/null || true
    fi
    # Restore the exact pre-deploy immutable state before restart. Failure is
    # reported transactionally, but must not strand a byte-restored service.
    if ! _restore_release_binary_immutable_flag; then
        flag_restore_ok=0
        echo "⚠ Previous binary restored, but its immutable flag could not be verified" >&2
    fi
    # The previous binary is on disk but remains stopped until its matching
    # routine assets are restored. This closes the old-binary/new-assets window.
    if [ -n "$asset_txn" ]; then
        if ! adk_rollback_routine_asset_transaction "$ADK_REL" "$asset_txn" \
          && ! adk_rollback_routine_asset_transaction "$ADK_REL" "$asset_txn"; then
            echo "🛑 Previous binary restored, but matching routine assets could not roll back" >&2
            echo "   Service remains stopped; durable transaction: $asset_txn" >&2
            return 5
        fi
    fi
    echo "↩ Previous binary restored — restarting release..."
    xattr -d com.apple.quarantine "$HOME/Library/LaunchAgents/$plist.plist" 2>/dev/null || true
    if ! launchctl bootstrap "$domain" "$HOME/Library/LaunchAgents/$plist.plist"; then
        echo "⚠ launchd bootstrap failed during rollback — using tmux fallback"
        start_release_tmux_fallback || true
    fi
    if wait_for_http_service_health "$plist" "$rel_port" "$DEPLOY_HEALTH_RETRIES" "$DEPLOY_HEALTH_DELAY_SECS" 1 1 1; then
        echo "✓ Rollback succeeded — release healthy on :${rel_port} with previous binary"
        [ "$flag_restore_ok" = 1 ] || return 7
        return 0
    else
        echo "✗ Rollback restart did not reach healthy state — manual intervention required (logs: ${ADK_REL:-}/logs/)"
        return 4
    fi
}

# The stop happens well before the binary/asset renames.  Keep its recovery
# state separate from the rename state so a failure in that gap cannot strand a
# proven release merely because no live file was swapped yet.
_release_launchd_job_is_loaded() {
    local domain="${LAUNCHD_DOMAIN:-}"

    [ -n "$domain" ] || domain="$(_launchd_domain)" || return 1
    launchctl print "$domain/$PLIST_REL" >/dev/null 2>&1
}

_release_old_process_is_alive() {
    adk_process_instance_alive \
        "${OLD_PID:-}" "${OLD_PID_IDENTITY:-}"
}

_release_service_is_stopped() {
    ! _release_launchd_job_is_loaded \
        && ! _release_old_process_is_alive \
        && _release_candidate_port_refuses_connections
}

_pre_promotion_release_restart_is_safe() {
    # Before the first live rename the current binary/assets are already a
    # matching pair.  The manifest is only trustworthy for that pair when no
    # older .prev is pending from an interrupted deploy.  Refuse an ambiguous
    # restart instead of booting a binary behind a newly-applied migration.
    local old_name new_path new_name

    if [ -e "$ADK_REL/bin/agentdesk.prev" ] \
      || [ -e "$ADK_REL/bin/agentdesk.prev.meta" ]; then
        echo "  ⚠ [rollback-guard] prior rollback material exists; pre-promotion restart is ambiguous" >&2
        return 1
    fi
    old_name="$(_manifest_latest_migration_name 2>/dev/null || true)"
    new_path="$(_latest_postgres_migration_path 2>/dev/null || true)"
    if [ -z "$old_name" ] || [ -z "$new_path" ]; then
        echo "  ⚠ [rollback-guard] cannot prove pre-promotion migration compatibility" >&2
        return 1
    fi
    new_name="$(basename "$new_path")"
    if _migration_advanced "$new_name" "$old_name"; then
        echo "  ▸ [rollback-guard] ${new_name} is ahead of running ${old_name}; old restart is unsafe" >&2
        return 1
    fi
    return 0
}

_restart_pre_promotion_release() {
    local domain rel_port wait_secs=0

    [ "${RELEASE_SERVICE_RECOVERY_ARMED:-0}" = 1 ] || return 1
    if [ "${RELEASE_SERVICE_STOP_CONFIRMED:-0}" != 1 ]; then
        if _release_launchd_job_is_loaded; then
            echo "↩ Previous release never stopped — no recovery start required"
            return 0
        fi
        # launchd can unload before its old PID finishes draining. TERM in that
        # window must wait for the exact old process instance, then restart;
        # treating a still-draining PID as "never stopped" strands the service
        # as soon as that process exits.
        while _release_old_process_is_alive && [ "$wait_secs" -lt 15 ]; do
            sleep 1
            wait_secs=$((wait_secs + 1))
        done
        if ! _release_service_is_stopped; then
            echo "🛑 Previous release did not quiesce during recovery" >&2
            return 1
        fi
        RELEASE_SERVICE_STOP_CONFIRMED=1
    fi
    [ "${RELEASE_SERVICE_RESTART_SAFE:-0}" = 1 ] || {
        echo "🛑 Previous release retained but left stopped: migration-safe restart was not proven" >&2
        return 2
    }
    _release_service_is_stopped || {
        echo "🛑 Refusing recovery start: previous release is not confirmed stopped" >&2
        return 1
    }
    if ! _restore_release_binary_immutable_flag; then
        echo "🛑 Previous release immutable flag could not be restored; leaving it stopped" >&2
        return 1
    fi

    domain="${LAUNCHD_DOMAIN:-$(_launchd_domain)}"
    rel_port="${REL_PORT:-${AGENTDESK_REL_PORT:-${ADK_DEFAULT_PORT:-8791}}}"
    echo "↩ No live promotion completed — restarting previous release pair..."
    xattr -d com.apple.quarantine "$HOME/Library/LaunchAgents/$PLIST_REL.plist" 2>/dev/null || true
    if ! launchctl bootstrap "$domain" "$HOME/Library/LaunchAgents/$PLIST_REL.plist"; then
        echo "⚠ launchd bootstrap failed during pre-promotion recovery — using tmux fallback"
        start_release_tmux_fallback || return 1
    fi
    if wait_for_http_service_health "$PLIST_REL" "$rel_port" "$DEPLOY_HEALTH_RETRIES" "$DEPLOY_HEALTH_DELAY_SECS" 1 1 1; then
        echo "✓ Previous release pair restarted and healthy on :${rel_port}"
        return 0
    fi
    echo "✗ Previous release restart did not reach healthy state — manual intervention required" >&2
    return 1
}

_stop_release_for_promotion() {
    local wait_secs=0

    # This is deliberately armed before bootout, not before the first mv.  A
    # signal or an unguarded failure after launchd unloads but before promotion
    # must still recover the untouched old binary/asset pair.
    ROLLBACK_ARMED=1
    RELEASE_SERVICE_RECOVERY_ARMED=1
    RELEASE_SERVICE_STOP_CONFIRMED=0
    RELEASE_SERVICE_RESTART_SAFE=0
    if _pre_promotion_release_restart_is_safe; then
        RELEASE_SERVICE_RESTART_SAFE=1
    fi

    launchctl bootout "$LAUNCHD_DOMAIN/$PLIST_REL" 2>/dev/null || true
    # A previous launchd bootstrap may have fallen back to this manual server.
    # It must not survive long enough to observe a newly promoted pair.
    tmux kill-session -t "${AGENTDESK_RELEASE_TMUX_SESSION:-AgentDesk-dcserver-release-manual}" 2>/dev/null || true
    while ! _release_service_is_stopped; do
        if [ "$wait_secs" -ge 15 ]; then
            break
        fi
        sleep 1
        wait_secs=$((wait_secs + 1))
    done
    if ! _release_service_is_stopped \
      && [ -n "${OLD_PID:-}" ] && kill -0 "$OLD_PID" 2>/dev/null; then
        echo "  ⚠ PID $OLD_PID did not exit after ${wait_secs}s — sending SIGKILL"
        kill -9 "$OLD_PID" 2>/dev/null || true
        wait_secs=0
        while ! _release_service_is_stopped && [ "$wait_secs" -lt 5 ]; do
            sleep 1
            wait_secs=$((wait_secs + 1))
        done
    fi
    if ! _release_service_is_stopped; then
        echo "✗ Previous release stop was not confirmed; refusing live promotion" >&2
        return 1
    fi
    RELEASE_SERVICE_STOP_CONFIRMED=1
    echo "  ✓ old process terminated"
}

_cleanup_owned_pg_tunnel_preflight() {
    local pid="${PG_TUNNEL_PREFLIGHT_PID:-}" attempts=0
    if [ -n "$pid" ]; then
        if kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" 2>/dev/null || true
            while kill -0 "$pid" 2>/dev/null && [ "$attempts" -lt 25 ]; do
                sleep 0.2
                attempts=$((attempts + 1))
            done
            if kill -0 "$pid" 2>/dev/null; then
                kill -KILL "$pid" 2>/dev/null || true
            fi
        fi
        # Reap the child on every exit path, including an early SSH failure.
        wait "$pid" 2>/dev/null || true
    fi
    PG_TUNNEL_PREFLIGHT_PID=""
    [ -z "${PG_TUNNEL_PREFLIGHT_CONNINFO_DIR:-}" ] || rm -rf "$PG_TUNNEL_PREFLIGHT_CONNINFO_DIR" 2>/dev/null || true
    [ -z "${PG_TUNNEL_PREFLIGHT_PASSWORD_FILE:-}" ] || rm -f "$PG_TUNNEL_PREFLIGHT_PASSWORD_FILE" 2>/dev/null || true
    PG_TUNNEL_PREFLIGHT_CONNINFO_DIR=""
    PG_TUNNEL_PREFLIGHT_PASSWORD_FILE=""
}

_reset_pg_tunnel_rollback_state() {
    PG_TUNNEL_ROLLBACK_ARMED=0
    PG_TUNNEL_ROLLBACK_DIR=""
    PG_TUNNEL_ROLLBACK_JOB_LOADED=0
    PG_TUNNEL_ROLLBACK_MANUAL_KIND="none"
    PG_TUNNEL_ROLLBACK_MANUAL_CONFIG=""
    PG_TUNNEL_ROLLBACK_WRAPPER_SOURCE=""
}

_pg_canonical_listener_absent() {
    command -v lsof >/dev/null 2>&1 || return 1
    ! lsof -nP -a -iTCP@127.0.0.1:15432 -sTCP:LISTEN >/dev/null 2>&1
}

_pg_wait_canonical_listener_absent() {
    local attempt=0
    while [ "$attempt" -lt 25 ]; do
        _pg_canonical_listener_absent && return 0
        sleep 0.2
        attempt=$((attempt + 1))
    done
    return 1
}

_pg_report_rollback_recovery() {
    local backup=${1:-unknown}
    echo "⚠ PG tunnel rollback incomplete; recovery material retained at $backup" >&2
    echo "  Manual recovery: clear the canonical :15432 listener, inspect state, then restore" >&2
    echo "  the saved wrapper/plist and restart the prior launchd or manual tunnel." >&2
}

_rollback_pg_tunnel_migration() {
    local domain="${PG_TUNNEL_LAUNCHD_DOMAIN:-}" bin="${PG_TUNNEL_BIN:-}"
    local plist="${PG_TUNNEL_PLIST_PATH:-}" backup="${PG_TUNNEL_ROLLBACK_DIR:-}"
    local restore_ok=1 readiness_ok=0
    [ "${PG_TUNNEL_ROLLBACK_ARMED:-0}" = 1 ] || return 0
    if [ -z "$domain" ] || [ -z "$bin" ] || [ -z "$plist" ] || [ -z "$backup" ]; then
        echo "✗ PG tunnel rollback state is incomplete" >&2
        _pg_report_rollback_recovery "${backup:-unknown}"
        return 1
    fi

    echo "↩ Restoring previous PG tunnel state..." >&2
    launchctl bootout "$domain/${PG_TUNNEL_LABEL:-com.agentdesk.pg-tunnel}" 2>/dev/null || true
    if ! _pg_wait_canonical_listener_absent; then
        echo "✗ New PG tunnel listener survived rollback bootout; refusing restore bind race" >&2
        _pg_report_rollback_recovery "$backup"
        return 1
    fi
    if [ -e "$backup/wrapper" ]; then
        if ! install -m 0755 "$backup/wrapper" "$bin" 2>/dev/null; then
            echo "✗ Failed to restore PG tunnel wrapper" >&2
            restore_ok=0
        fi
    elif ! rm -f "$bin" 2>/dev/null; then
        echo "✗ Failed to remove newly installed PG tunnel wrapper" >&2
        restore_ok=0
    fi
    if [ -e "$backup/plist" ]; then
        if ! cp -p "$backup/plist" "$plist.tmp" 2>/dev/null \
          || ! mv -f "$plist.tmp" "$plist" 2>/dev/null; then
            echo "✗ Failed to restore PG tunnel launchd plist" >&2
            restore_ok=0
        fi
    elif ! rm -f "$plist" "$plist.tmp" 2>/dev/null; then
        echo "✗ Failed to remove newly installed PG tunnel launchd plist" >&2
        restore_ok=0
    fi

    if [ "$restore_ok" = 1 ]; then
        if [ "${PG_TUNNEL_ROLLBACK_JOB_LOADED:-0}" = 1 ]; then
            if [ ! -f "$plist" ] || ! launchctl bootstrap "$domain" "$plist" 2>/dev/null; then
                echo "✗ Failed to restart previous PG tunnel launchd job" >&2
                restore_ok=0
            fi
        elif [ "${PG_TUNNEL_ROLLBACK_MANUAL_KIND:-none}" != none ]; then
            if [ ! -x "${PG_TUNNEL_ROLLBACK_WRAPPER_SOURCE:-}" ] \
              || [ ! -r "${PG_TUNNEL_ROLLBACK_MANUAL_CONFIG:-}" ] \
              || ! "$PG_TUNNEL_ROLLBACK_WRAPPER_SOURCE" --restore-canonical \
                    "$PG_TUNNEL_ROLLBACK_MANUAL_CONFIG" \
                    "$PG_TUNNEL_ROLLBACK_MANUAL_KIND" >/dev/null 2>&1; then
                echo "✗ Failed to restart previous manual PG tunnel" >&2
                restore_ok=0
            fi
        fi
    fi

    if [ "$restore_ok" = 1 ]; then
        if [ "${PG_TUNNEL_ROLLBACK_JOB_LOADED:-0}" = 1 ]; then
            if _pg_sql_probe 15432 12; then
                readiness_ok=1
            else
                echo "✗ Restored PG tunnel did not become SQL-ready on :15432 after launchd throttle window" >&2
            fi
            _cleanup_owned_pg_tunnel_preflight
        elif [ "${PG_TUNNEL_ROLLBACK_MANUAL_KIND:-none}" != none ]; then
            if _pg_sql_probe 15432; then
                readiness_ok=1
            else
                echo "✗ Restored PG tunnel did not become SQL-ready on :15432" >&2
            fi
            _cleanup_owned_pg_tunnel_preflight
        elif _pg_wait_canonical_listener_absent; then
            readiness_ok=1
        else
            echo "✗ Restored no-tunnel state still has a listener on :15432" >&2
        fi
    fi
    if [ "$restore_ok" = 1 ] && [ "$readiness_ok" = 1 ]; then
        if rm -rf "$backup" 2>/dev/null; then
            _reset_pg_tunnel_rollback_state
            echo "✓ Previous PG tunnel state restored and verified" >&2
            return 0
        fi
        echo "✗ Previous PG tunnel state is verified but rollback backup cleanup failed" >&2
    fi
    _pg_report_rollback_recovery "$backup"
    return 1
}

_cleanup_on_exit() {
    local status=${1:-$?}
    local binary_rollback_code=0
    local asset_action="rollback"
    local active_txn=""
    local active_status=0
    local active_phase=""
    local asset_lock_held=0
    local restart_pre_promotion_release=0
    local forward_recovery_required=0
    local forward_marker=""
    local forward_marker_present=0
    local forward_marker_valid=1
    local forward_migration_state="${FORWARD_MIGRATION_RECOVERY_STATE:-none}"
    local candidate_drain_guard=0

    # Cleanup traps are installed before this invocation acquires the shared
    # deploy lock. A pre-lock signal must not inspect another deploy's durable
    # asset marker or turn the original signal status into a cleanup failure.
    if adk_routine_asset_lock_owned "$DEPLOY_LOCK_FILE"; then
        asset_lock_held=1
    fi
    trap - EXIT
    trap '' INT TERM
    _cleanup_owned_pg_tunnel_preflight
    if [ "$status" -ne 0 ]; then
        _rollback_pg_tunnel_migration || true
        _cleanup_owned_pg_tunnel_preflight
    fi
    # Binary outcome owns the asset disposition. A safe binary rollback means
    # assets must roll back too. Migration refusal or a pre-restore failure
    # leaves the new binary in place, so the already-promoted assets must commit
    # forward with it. The durable marker, not a post-mv caller flag, identifies
    # a transaction even when TERM lands inside its first rename.
    if [ "${DEPLOY_OK:-0}" != 1 ] && [ "$asset_lock_held" = 1 ]; then
        if active_txn="$(_adk_active_txn "$ADK_REL")"; then
            active_status=0
            if ! active_phase="$(
                adk_routine_asset_transaction_phase "$ADK_REL" "$active_txn"
            )"; then
                active_status=2
                active_txn=""
                status=1
                echo "🛑 Routine asset transaction phase is corrupt" >&2
            fi
        else
            active_status=$?
            active_txn=""
            if [ "$active_status" -ne 1 ]; then
                status=1
                echo "🛑 Routine asset transaction marker is corrupt" >&2
            fi
        fi
        if [ "$active_status" -eq 0 ]; then
            forward_marker="$(_forward_migration_marker_path "$active_txn")" \
                || forward_marker=""
            if [ -n "$forward_marker" ] \
              && { [ -e "$forward_marker" ] || [ -L "$forward_marker" ]; }; then
                forward_marker_present=1
                if ! forward_migration_state="$(
                    _forward_migration_recovery_state "$active_txn"
                )"; then
                    forward_marker_valid=0
                    forward_migration_state=unknown/interrupted
                fi
            fi
        fi
        if [ "$active_status" -eq 0 ] \
          && adk_routine_asset_candidate_drain_authority_exists "$active_txn" \
          && [ "${RELEASE_CANDIDATE_CAPTURED:-0}" != 1 ]; then
            candidate_drain_guard=1
            forward_recovery_required=1
            asset_action="none"
            restart_pre_promotion_release=0
            status=1
            echo "🛑 Exact candidate drain is unresolved; retaining binary/assets and rollback material" >&2
        fi
        if [ "$candidate_drain_guard" = 0 ] \
          && [ "$active_status" -eq 0 ] \
          && { [ "$forward_migration_state" = advanced ] \
            || [ "$forward_migration_state" = unknown/interrupted ] \
            || { [ "${FORWARD_MIGRATION_APPLIED:-0}" = 1 ] \
              && [ "$forward_migration_state" = none ]; }; }; then
            forward_recovery_required=1
            asset_action="none"
            restart_pre_promotion_release=0
            if [ "$forward_marker_present" = 1 ] \
              && { [ "$forward_marker_valid" != 1 ] \
                || ! _forward_migration_candidate_sha \
                    "$active_txn" >/dev/null 2>&1; }; then
                status=1
                echo "🛑 Forward-migration recovery marker is invalid" >&2
                echo "   retained candidate/transaction: $active_txn" >&2
            elif _recover_forward_migrated_release "$active_txn"; then
                active_txn=""
                active_phase=""
            else
                status=1
                echo "🛑 Forward-migrated candidate recovery remains incomplete" >&2
                echo "   retained candidate/transaction: $active_txn" >&2
            fi
        fi
        if [ "$forward_recovery_required" = 0 ] && [ "$active_status" -gt 1 ]; then
            asset_action="none"
            status=1
            if [ "${ROLLBACK_ARMED:-0}" = 1 ]; then
                echo "🛑 Refusing binary-only rollback while routine asset state is corrupt" >&2
            fi
        elif [ "$forward_recovery_required" = 0 ] \
          && { [ "$active_phase" = "committing" ] \
            || [ "$active_phase" = "committed" ]; }; then
            # Health passed and commit intent reached disk before the success
            # flag. Preserve the proven binary if TERM lands in that tiny gap.
            asset_action="commit"
        elif [ "$forward_recovery_required" = 0 ] \
          && [ "${ROLLBACK_ARMED:-0}" = 1 ] \
          && [ -n "${STAGED_BINARY:-}" ] && [ -e "$STAGED_BINARY" ]; then
            # The binary rename did not consume its same-directory stage. Keep
            # the old binary, roll assets back, then restart that exact pair
            # only when the pre-stop migration guard proved it safe.
            asset_action="rollback"
            restart_pre_promotion_release=1
            status=1
            echo "🛑 Binary promotion did not complete; restoring the previous release pair" >&2
        elif [ "$forward_recovery_required" = 0 ] \
          && [ "${ROLLBACK_ARMED:-0}" = 1 ]; then
            asset_action="none"
            _rollback_release_binary "$active_txn" || binary_rollback_code=$?
            case "$binary_rollback_code" in
                0)
                    asset_action="none"
                    ;;
                2)
                    asset_action="commit"
                    echo "⚠ Binary rollback refused; committing matching new routine assets" >&2
                    ;;
                3)
                    asset_action="commit"
                    status=1
                    echo "🛑 Binary rollback failed before restore; retaining matching new routine assets" >&2
                    ;;
                4)
                    asset_action="none"
                    status=1
                    echo "🛑 Previous binary was restored but did not become healthy" >&2
                    ;;
                5)
                    asset_action="rollback"
                    status=1
                    echo "🛑 Previous binary restored but asset rollback remains incomplete" >&2
                    ;;
                6)
                    asset_action="none"
                    status=1
                    echo "🛑 Candidate did not drain; retaining the uncommitted generation and rollback material" >&2
                    ;;
                7)
                    asset_action="none"
                    status=1
                    echo "🛑 Previous generation restored, but immutable flag verification failed" >&2
                    ;;
                *)
                    asset_action="commit"
                    status=1
                    echo "🛑 Unknown binary rollback outcome: $binary_rollback_code" >&2
                    ;;
            esac
        fi
        if [ "$forward_recovery_required" = 0 ] \
          && [ -n "$active_txn" ] && [ "$asset_action" != "none" ]; then
            if [ "$asset_action" = "commit" ]; then
                if ! adk_commit_routine_asset_transaction_forward "$ADK_REL" "$active_txn"; then
                    status=1
                    echo "🛑 Routine assets could not commit with the retained new binary" >&2
                fi
            elif ! adk_rollback_routine_asset_transaction "$ADK_REL" "$active_txn"; then
                status=1
                restart_pre_promotion_release=0
                echo "🛑 Routine asset rollback failed; durable state retained at $active_txn" >&2
            fi
        fi
        if [ "$forward_recovery_required" = 0 ] \
          && [ "$restart_pre_promotion_release" = 1 ]; then
            if ! _restart_pre_promotion_release; then
                status=1
            fi
        fi
    fi
    if [ "$forward_recovery_required" = 0 ] \
      && [ -n "${ROUTINE_ASSET_TXN:-}" ]; then
        forward_marker="$(_forward_migration_marker_path "$ROUTINE_ASSET_TXN")" \
            || forward_marker=""
        if [ -n "$forward_marker" ] \
          && { [ -e "$forward_marker" ] || [ -L "$forward_marker" ]; }; then
            forward_recovery_required=1
            echo "⚠ Retaining the durable forward candidate despite unverifiable lock ownership" >&2
        fi
    fi
    if [ "$candidate_drain_guard" = 1 ] \
      && [ -n "${STAGED_BINARY:-}" ] && [ -e "$STAGED_BINARY" ]; then
        rm -f "$STAGED_BINARY" 2>/dev/null || true
    elif [ "$forward_recovery_required" = 0 ] \
      && [ "${FORWARD_MIGRATION_APPLIED:-0}" != 1 ] \
      && [ -n "${STAGED_BINARY:-}" ] && [ -e "$STAGED_BINARY" ]; then
        rm -f "$STAGED_BINARY" 2>/dev/null || true
    elif { [ "$forward_recovery_required" = 1 ] \
          || [ "${FORWARD_MIGRATION_APPLIED:-0}" = 1 ]; } \
      && [ -n "${STAGED_BINARY:-}" ] && [ -e "$STAGED_BINARY" ]; then
        echo "⚠ Retaining forward-compatible staged binary: $STAGED_BINARY" >&2
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
    if [ "${ROUTINE_ASSET_INCOMING_CLAIMED:-0}" = 1 ] \
      && [ -n "${ROUTINE_ASSET_INCOMING:-}" ] \
      && [ -d "$ROUTINE_ASSET_INCOMING" ]; then
        if adk_remove_claimed_routine_asset_incoming \
            "$ADK_REL" "$ROUTINE_ASSET_INCOMING" "$DEPLOY_LOCK_FILE"; then
            ROUTINE_ASSET_INCOMING_CLAIMED=0
        else
            status=1
            echo "🛑 Failed to remove claimed peer routine asset inbox" >&2
        fi
    fi
    if [ "$asset_lock_held" = 1 ] && ! adk_release_routine_asset_lock; then
        status=1
        echo "🛑 Failed to release shared routine asset deploy lock" >&2
    fi
    _finalize_detached_helper "$status"
    return "$status"
}

_handle_cleanup_signal() {
    local status=$1
    _cleanup_on_exit "$status" || status=$?
    exit "$status"
}

trap _cleanup_on_exit EXIT
trap '_handle_cleanup_signal 130' INT
trap '_handle_cleanup_signal 143' TERM

_self_hosted_release_session() {
    [ "$DEPLOY_DETACHED_CHILD" != "1" ] || return 1
    # Peer inbox ownership is synchronous with the SSH caller. Never detach a
    # peer leg before it has claimed and consumed that unique inbox.
    [ "$DEPLOY_PEER_INVOCATION" != "1" ] || return 1
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
        AGENTDESK_DEPLOY_BUNDLE_MANIFEST \
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
    local remote_lock_query
    local peer_adk_rel=""
    local peer_lock_file=""
    local peer_incoming=""
    local peer_incoming_token=""
    local peer_incoming_created=0
    if ! adk_validate_peer_destination "$peer" \
      || ! _adk_validate_peer_timeout "$DEPLOY_SSH_CONNECT_TIMEOUT"; then
        echo "✗ Refusing unsafe peer destination or SSH timeout" >&2
        return 1
    fi
    env_prelude="$(_deploy_peer_env_prelude)"
    if [ "$#" -gt 0 ]; then
        quoted_args=$(printf ' %q' "$@")
    fi
    if [ -n "${AGENTDESK_PEER_REPO_DIR:-}" ]; then
        remote_cd_command="cd $(printf '%q' "$AGENTDESK_PEER_REPO_DIR")"
    else
        remote_cd_command='remote_root="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"; cd "${AGENTDESK_REPO_DIR:-$remote_root/workspaces/agentdesk}"'
    fi
    # Operator-private assets travel only into a unique remote inbox. The peer
    # deploy claims that inbox under its own shared deploy lock, validates it,
    # and merges it into its transaction stage. Pre-sync never mutates live
    # routines, even if transport or remote preflight fails halfway through.
    if [ -d "$ADK_REL/routines" ] || [ -d "$ADK_REL/routine-helpers" ]; then
        if [ ! -d "$ADK_REL/routines" ] || [ ! -d "$ADK_REL/routine-helpers" ]; then
            echo "✗ [peer:$peer] local routine asset surfaces are incomplete"
            return 1
        fi
        if ! peer_adk_rel="$(ssh -o ConnectTimeout="$DEPLOY_SSH_CONNECT_TIMEOUT" "$peer" \
            'bash -lc '"$(printf '%q' 'echo "${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"')"'')"; then
            echo "✗ [peer:$peer] could not resolve remote AGENTDESK_ROOT_DIR"
            return 1
        fi
        peer_adk_rel="$(printf '%s' "$peer_adk_rel" | tr -d '\r')"
        if ! adk_validate_peer_runtime_root "$peer_adk_rel"; then
            echo "✗ [peer:$peer] remote AGENTDESK_ROOT_DIR is not a safe absolute path"
            return 1
        fi
        remote_lock_query="set -e
${env_prelude}
${remote_cd_command}
runtime_root=$(printf '%q' "$peer_adk_rel")
lock_file=\${AGENTDESK_DEPLOY_LOCK_FILE:-\$runtime_root/runtime/deploy-release.lock}
python3 - \"\$lock_file\" <<'PY'
import os
import sys
print(os.path.abspath(sys.argv[1]))
PY"
        if ! peer_lock_file="$(ssh -o ConnectTimeout="$DEPLOY_SSH_CONNECT_TIMEOUT" \
            "$peer" "bash -lc $(printf '%q' "$remote_lock_query")")"; then
            echo "✗ [peer:$peer] could not resolve remote deploy lock path"
            return 1
        fi
        peer_lock_file="$(printf '%s' "$peer_lock_file" | tr -d '\r')"
        if ! adk_validate_peer_runtime_root "$peer_lock_file"; then
            echo "✗ [peer:$peer] remote deploy lock path is not a safe absolute path"
            return 1
        fi
        adk_validate_quickjs_routine_tree "$ADK_REL/routines" || {
            echo "✗ [peer:$peer] local routine surface failed portability validation"
            return 1
        }
        adk_validate_routine_helper_surface "$ADK_REL/routine-helpers" || {
            echo "✗ [peer:$peer] local helper surface failed portability validation"
            return 1
        }

        peer_incoming_token="$(date '+%Y%m%d%H%M%S').$$.$RANDOM"
        if ! peer_incoming="$(adk_prepare_peer_asset_incoming \
            "$peer" "$peer_adk_rel" "$peer_incoming_token" \
            "$DEPLOY_SSH_CONNECT_TIMEOUT")"; then
            echo "✗ [peer:$peer] could not create unique routine asset inbox"
            return 1
        fi
        peer_incoming_created=1

        echo "▸ [peer:$peer] Uploading routine assets into transaction inbox..."
        if ! adk_rsync_peer_asset_surface "$ADK_REL/routines" "$peer" \
            "$peer_incoming" "routines" "$DEPLOY_SSH_CONNECT_TIMEOUT" \
          || ! adk_rsync_peer_asset_surface "$ADK_REL/routine-helpers" "$peer" \
            "$peer_incoming" "routine-helpers" "$DEPLOY_SSH_CONNECT_TIMEOUT"; then
            adk_remove_peer_asset_incoming "$peer" "$peer_adk_rel" \
                "$peer_incoming_token" "$DEPLOY_SSH_CONNECT_TIMEOUT" \
                "$peer_lock_file" \
                >/dev/null 2>&1 || true
            echo "✗ [peer:$peer] routine asset inbox transfer failed"
            return 1
        fi
        env_prelude="$env_prelude AGENTDESK_ROUTINE_ASSET_INCOMING=$(printf '%q' "$peer_incoming")"
    fi

    env_prelude="$env_prelude AGENTDESK_PEER_SYNC_MAIN_UNDER_LOCK=1"
    # The peer checkout itself may predate the under-lock sync protocol. Fetch
    # without mutating the worktree, extract deploy+common from one exact remote
    # commit, and let that bootstrap acquire/recover the durable deploy lock
    # before it fast-forwards the checkout and same-PID re-execs from the result.
    remote_deploy_command="set -e
${remote_cd_command}
peer_repo=\$PWD
git fetch --quiet origin main
bootstrap_head=\$(git rev-parse origin/main)
bootstrap_root=\$(mktemp -d \"\${TMPDIR:-/tmp}/agentdesk-peer-bootstrap.XXXXXXXX\")
cleanup_peer_bootstrap() { rm -rf -- \"\$bootstrap_root\"; }
trap cleanup_peer_bootstrap EXIT INT TERM
mkdir -p \"\$bootstrap_root\"
git archive \"\$bootstrap_head\" scripts/deploy-release.sh scripts/routine-asset-surface.sh \
    | tar -xf - -C \"\$bootstrap_root\"
[ -f \"\$bootstrap_root/scripts/deploy-release.sh\" ]
[ -f \"\$bootstrap_root/scripts/routine-asset-surface.sh\" ]
${env_prelude} \
AGENTDESK_PEER_BOOTSTRAP_HEAD=\"\$bootstrap_head\" \
AGENTDESK_REPO_DIR=\"\$peer_repo\" \
bash \"\$bootstrap_root/scripts/deploy-release.sh\"${quoted_args}"
    echo "▸ [peer:$peer] Running deploy-release.sh..."
    if ! ssh -o ConnectTimeout="$DEPLOY_SSH_CONNECT_TIMEOUT" "$peer" "bash -lc $(printf '%q' "$remote_deploy_command")"; then
        if [ "$peer_incoming_created" = 1 ]; then
            adk_remove_peer_asset_incoming "$peer" "$peer_adk_rel" \
                "$peer_incoming_token" "$DEPLOY_SSH_CONNECT_TIMEOUT" \
                "$peer_lock_file" \
                >/dev/null 2>&1 || true
        fi
        echo "✗ [peer:$peer] deploy-release.sh failed"
        return 1
    fi

    if [ "$peer_incoming_created" = 1 ] \
      && ! adk_remove_peer_asset_incoming "$peer" "$peer_adk_rel" \
          "$peer_incoming_token" "$DEPLOY_SSH_CONNECT_TIMEOUT" \
          "$peer_lock_file"; then
        echo "✗ [peer:$peer] deployed but could not verify routine asset inbox cleanup"
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
    echo "▸ [gate] Waiting for release deploy lock: $DEPLOY_LOCK_FILE"
    adk_acquire_routine_asset_lock "$DEPLOY_LOCK_FILE" "$DEPLOY_LOCK_TIMEOUT_SECS" \
        || exit 1
    echo "▸ [gate] Release deploy lock acquired"
}

_peer_git_blob_sha256() {
    local revision="$1"
    local relative_path="$2"

    if command -v shasum >/dev/null 2>&1; then
        git -C "$REPO" show "$revision:$relative_path" \
            | shasum -a 256 | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        git -C "$REPO" show "$revision:$relative_path" \
            | sha256sum | awk '{print $1}'
    else
        return 1
    fi
}

_validate_peer_bootstrap_generation() {
    local bootstrap_head="${AGENTDESK_PEER_BOOTSTRAP_HEAD:-}"
    local relative_path actual_path expected_sha actual_sha

    [ "${DEPLOY_PEER_INVOCATION:-0}" = 1 ] || return 0
    [ "${AGENTDESK_PEER_SYNC_MAIN_UNDER_LOCK:-0}" = 1 ] || return 0
    [ "${AGENTDESK_PEER_SYNC_REEXEC:-0}" != 1 ] || return 0
    adk_routine_asset_lock_owned "$DEPLOY_LOCK_FILE" || return 1
    case "$bootstrap_head" in
        ''|*[!0-9a-f]*) return 1 ;;
    esac
    [ "${#bootstrap_head}" -ge 40 ] && [ "${#bootstrap_head}" -le 64 ] \
        || return 1
    git -C "$REPO" cat-file -e "$bootstrap_head^{commit}" 2>/dev/null \
        || return 1
    for relative_path in \
        scripts/deploy-release.sh scripts/routine-asset-surface.sh; do
        case "$relative_path" in
            scripts/deploy-release.sh) actual_path="$SCRIPT_DIR/deploy-release.sh" ;;
            scripts/routine-asset-surface.sh)
                actual_path="$SCRIPT_DIR/routine-asset-surface.sh"
                ;;
        esac
        [ -f "$actual_path" ] && [ ! -L "$actual_path" ] || return 1
        expected_sha="$(_peer_git_blob_sha256 \
            "$bootstrap_head" "$relative_path")" || return 1
        actual_sha="$(_sha256_file "$actual_path")" || return 1
        [ "$actual_sha" = "$expected_sha" ] || return 1
    done
}

_peer_post_merge_executable_input_digest() {
    local common="$REPO/scripts/routine-asset-surface.sh"

    [ -f "$common" ] && [ ! -L "$common" ] || return 1
    bash -s -- "$common" "$REPO" <<'SH'
set -euo pipefail
. "$1"
adk_executable_input_digest "$2"
SH
}

_peer_sync_main_and_reexec_under_lock() {
    [ "${DEPLOY_PEER_INVOCATION:-0}" = 1 ] || return 0
    [ "${AGENTDESK_PEER_SYNC_MAIN_UNDER_LOCK:-0}" = 1 ] || return 0
    [ "${AGENTDESK_PEER_SYNC_REEXEC:-0}" != 1 ] || return 0
    adk_routine_asset_lock_owned "$DEPLOY_LOCK_FILE" || return 1

    echo "▸ [peer] Syncing main under the release deploy lock..."
    git -C "$REPO" fetch --quiet origin main || return 1
    git -C "$REPO" checkout --quiet main || return 1
    git -C "$REPO" merge --quiet --ff-only origin/main || return 1
    AGENTDESK_PEER_LOCKED_HEAD="$(git -C "$REPO" rev-parse HEAD)" \
        || return 1
    AGENTDESK_PEER_LOCKED_DEPLOY_SHA="$(_sha256_file \
        "$REPO/scripts/deploy-release.sh")" || return 1
    AGENTDESK_PEER_LOCKED_COMMON_SHA="$(_sha256_file \
        "$REPO/scripts/routine-asset-surface.sh")" || return 1
    # The bootstrap may implement an older digest protocol than the generation
    # it just merged. Load the post-merge common script in a fresh shell so the
    # captured digest and the re-exec validator necessarily use one protocol.
    AGENTDESK_PEER_LOCKED_INPUT_SHA="$(_peer_post_merge_executable_input_digest)" \
        || return 1
    export AGENTDESK_PEER_LOCKED_HEAD AGENTDESK_PEER_LOCKED_INPUT_SHA \
        AGENTDESK_PEER_LOCKED_DEPLOY_SHA AGENTDESK_PEER_LOCKED_COMMON_SHA
    export ADK_ROUTINE_ASSET_LOCK_DIR ADK_ROUTINE_ASSET_LOCK_TOKEN
    export AGENTDESK_PEER_SYNC_MAIN_UNDER_LOCK=0
    export AGENTDESK_PEER_SYNC_REEXEC=1
    exec bash "$REPO/scripts/deploy-release.sh" "$@"
}

_validate_peer_locked_generation() {
    [ "${AGENTDESK_PEER_SYNC_REEXEC:-0}" = 1 ] || return 0
    adk_routine_asset_lock_owned "$DEPLOY_LOCK_FILE" || return 1
    [ -n "${AGENTDESK_PEER_LOCKED_HEAD:-}" ] \
        && [ -n "${AGENTDESK_PEER_LOCKED_INPUT_SHA:-}" ] \
        && [ -n "${AGENTDESK_PEER_LOCKED_DEPLOY_SHA:-}" ] \
        && [ -n "${AGENTDESK_PEER_LOCKED_COMMON_SHA:-}" ] || return 1
    [ "$(git -C "$REPO" rev-parse HEAD)" \
        = "$AGENTDESK_PEER_LOCKED_HEAD" ] || return 1
    [ "$(_sha256_file "$REPO/scripts/deploy-release.sh")" \
        = "$AGENTDESK_PEER_LOCKED_DEPLOY_SHA" ] || return 1
    [ "$(_sha256_file "$REPO/scripts/routine-asset-surface.sh")" \
        = "$AGENTDESK_PEER_LOCKED_COMMON_SHA" ] || return 1
    [ "$(adk_executable_input_digest "$REPO")" \
        = "$AGENTDESK_PEER_LOCKED_INPUT_SHA" ]
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
export AGENTDESK_DEPLOY_BUNDLE_MANIFEST=$(printf '%q' "${AGENTDESK_DEPLOY_BUNDLE_MANIFEST:-}")
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
export AGENTDESK_ROUTINE_ASSET_INCOMING=$(printf '%q' "${AGENTDESK_ROUTINE_ASSET_INCOMING:-}")
export AGENTDESK_BUNDLE_ID=$(printf '%q' "$BUNDLE_ID")
export AGENTDESK_DCSERVER_LABEL=$(printf '%q' "$PLIST_REL")
export AGENTDESK_PLIST_REL=$(printf '%q' "${AGENTDESK_PLIST_REL:-}")
export OBSIDIAN_VAULT_ROOT=$(printf '%q' "${OBSIDIAN_VAULT_ROOT:-}")
export AGENTDESK_OBSIDIAN_AGENTS_SRC=$(printf '%q' "${AGENTDESK_OBSIDIAN_AGENTS_SRC:-}")
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

REL_BINARY="$ADK_REL/bin/agentdesk"
REL_BINARY_BACKUP="$ADK_REL/bin/agentdesk.prev"
REL_BINARY_BACKUP_META="$REL_BINARY_BACKUP.meta"
if ! _recover_durable_forward_migration_before_new_deploy; then
    exit 1
fi
if ! _validate_peer_bootstrap_generation; then
    echo "✗ Peer bootstrap scripts did not match their fetched generation" >&2
    exit 1
fi
if ! _peer_sync_main_and_reexec_under_lock "$@"; then
    echo "✗ Peer repository sync failed under the deploy lock" >&2
    exit 1
fi
if ! _validate_peer_locked_generation; then
    echo "✗ Peer repository generation changed after its locked sync" >&2
    exit 1
fi

if [ -n "$ROUTINE_ASSET_INCOMING" ]; then
    if ! adk_claim_routine_asset_incoming \
        "$ADK_REL" "$ROUTINE_ASSET_INCOMING" "$DEPLOY_LOCK_FILE"; then
        echo "✗ Refusing unsafe or unowned peer routine asset inbox"
        exit 1
    fi
    ROUTINE_ASSET_INCOMING_CLAIMED=1
fi

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

# An external executable is trusted only as one member of a manifest-bound
# generation. Validate this even in DEPLOY_TEST_MODE so CI can exercise the
# rejection gate without reaching service lifecycle operations.
if [ -n "${AGENTDESK_DEPLOY_BINARY:-}" ]; then
    SOURCE_BINARY="$AGENTDESK_DEPLOY_BINARY"
    if ! _validate_external_deploy_bundle; then
        exit 1
    fi
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
if [ -z "${AGENTDESK_DEPLOY_BINARY:-}" ]; then
    SOURCE_BINARY="$(_resolve_default_release_binary "$DEPLOY_BUILD_PROFILE")"
fi
if [ -z "${AGENTDESK_DEPLOY_BINARY:-}" ]; then
    DEPLOY_EXECUTABLE_INPUT_SHA="$(adk_executable_input_digest "$REPO")" || {
        echo "✗ Could not capture executable inputs before cargo build"
        exit 1
    }
    if [ "$DEPLOY_BUILD_PROFILE" = "release" ]; then
        echo "▸ Building release binary..."
        (cd "$REPO" && cargo build --release --bin agentdesk)
    else
        echo "▸ Building ${DEPLOY_BUILD_PROFILE} binary (opt-in fast deploy profile)..."
        (cd "$REPO" && cargo build --profile "$DEPLOY_BUILD_PROFILE" --bin agentdesk)
    fi
    if [ "$(adk_executable_input_digest "$REPO")" \
      != "$DEPLOY_EXECUTABLE_INPUT_SHA" ]; then
        echo "✗ Executable inputs changed while cargo was building"
        exit 1
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
echo "▸ Validating required routine asset payload..."
if ! adk_validate_repo_routine_assets "$REPO"; then
    echo "✗ Routine asset preflight failed; refusing an incomplete deploy"
    exit 1
fi
if [ "$ROUTINE_ASSET_INCOMING_CLAIMED" = 1 ]; then
    if [ ! -d "$ROUTINE_ASSET_INCOMING/routines" ] \
      || [ ! -d "$ROUTINE_ASSET_INCOMING/routine-helpers" ] \
      || ! adk_validate_quickjs_routine_tree \
          "$ROUTINE_ASSET_INCOMING/routines" \
      || ! adk_validate_routine_helper_surface \
          "$ROUTINE_ASSET_INCOMING/routine-helpers"; then
        echo "✗ Claimed peer routine asset inbox is incomplete or invalid"
        exit 1
    fi
fi

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
if ! _validate_peer_locked_generation; then
    echo "✗ Peer repository generation changed before staging" >&2
    exit 1
fi
if [ -z "${DEPLOY_EXECUTABLE_INPUT_SHA:-}" ] \
  || [ "$(adk_executable_input_digest "$REPO")" \
    != "$DEPLOY_EXECUTABLE_INPUT_SHA" ]; then
    echo "✗ Executable inputs changed before generation staging"
    exit 1
fi
if ! ROUTINE_ASSET_TXN="$(
    adk_begin_routine_asset_transaction "$ADK_REL" "$DEPLOY_LOCK_FILE"
)"; then
    echo "✗ Could not begin durable routine asset transaction"
    exit 1
fi
echo "▸ Staging routines..."
if ! adk_stage_routines "$REPO" "$ADK_REL" "$ROUTINE_ASSET_TXN" \
    "${ROUTINE_ASSET_INCOMING:+$ROUTINE_ASSET_INCOMING/routines}" \
    >/dev/null; then
    echo "✗ Routine staging failed; refusing to swap an incomplete asset tree"
    exit 1
fi

# Stage deterministic helper assets outside the QuickJS-only routines root.
# Seed with any operator-owned helpers, then overlay repository assets without
# --delete so unrelated local helpers survive release deployment.
echo "▸ Staging routine helper assets..."
if ! adk_stage_routine_helpers "$REPO" "$ADK_REL" "$ROUTINE_ASSET_TXN" \
    "${ROUTINE_ASSET_INCOMING:+$ROUTINE_ASSET_INCOMING/routine-helpers}" \
    >/dev/null; then
    echo "✗ Routine helper staging failed; refusing to swap an incomplete asset tree"
    exit 1
fi
if ! _strip_legacy_helper_sentinel_from_staged_generation \
    "$ROUTINE_ASSET_TXN/staged/release-root/routine-helpers"; then
    echo "✗ Reserved legacy helper sentinel contaminated the staged generation"
    exit 1
fi

if [ "$ROUTINE_ASSET_INCOMING_CLAIMED" = 1 ]; then
    if ! adk_remove_claimed_routine_asset_incoming \
        "$ADK_REL" "$ROUTINE_ASSET_INCOMING" "$DEPLOY_LOCK_FILE"; then
        echo "✗ Could not retire consumed peer routine asset inbox"
        exit 1
    fi
    ROUTINE_ASSET_INCOMING_CLAIMED=0
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

_doctor_postgres_preflight() {
    local label=$1 doctor_json_tmp doctor_rc
    doctor_json_tmp=$(mktemp "${TMPDIR:-/tmp}/agentdesk-doctor.XXXXXX.json") || return 1
    set +e
    "$SOURCE_BINARY" doctor --json >"$doctor_json_tmp" 2>/dev/null
    doctor_rc=$?
    set -e
    if [ ! -s "$doctor_json_tmp" ]; then
        echo "✗ ${label} did not return JSON output."
        rm -f "$doctor_json_tmp"
        return 1
    fi
    if ! python3 - "$doctor_json_tmp" <<'PY'
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
        rm -f "$doctor_json_tmp"
        return 1
    fi
    if [ "$doctor_rc" -ne 0 ]; then
        echo "⚠ doctor command returned non-zero ($doctor_rc), but postgres preflight check passed."
    fi
    rm -f "$doctor_json_tmp"
}

echo "▸ Preflight PostgreSQL migration integrity via doctor..."
_doctor_postgres_preflight "Doctor preflight"

# Copy and sign the binary before stopping release. This keeps a missing
# certificate or failed codesign from taking down a healthy dcserver.
echo "▸ Staging signed binary from $SOURCE_BINARY..."
STAGED_BINARY="$(_staged_deploy_binary_path)"
cp "$SOURCE_BINARY" "$STAGED_BINARY"
chmod +x "$STAGED_BINARY"
xattr -d com.apple.provenance "$STAGED_BINARY" 2>/dev/null || true
sign_binary_with_fallback "$STAGED_BINARY"
_clean_release_build_cache_after_staging
echo "▸ Exact-validating staged routine generation with candidate runtime..."
if ! adk_validate_staged_routine_asset_transaction \
    "$ADK_REL" "$ROUTINE_ASSET_TXN" "$STAGED_BINARY"; then
    echo "✗ Candidate runtime rejected staged routines before migration/service stop"
    exit 1
fi
echo "▸ Preflighting current release rollback generation..."
if ! _prepare_release_rollback_generation; then
    echo "✗ Current release rollback generation is unsafe; service was not stopped"
    exit 1
fi

# ── Fail-closed PostgreSQL tunnel migration (#4378) ───────────────────────────
# Prove the new remote Unix-socket route on an alternate local port, then replace
# and prove the canonical launchd route before dcserver is stopped. A missing
# machine-local config is a node gate; a present but invalid or unusable config
# aborts without disrupting dcserver.
PG_TUNNEL_LABEL="com.agentdesk.pg-tunnel"
PG_TUNNEL_PLIST_PATH="$HOME/Library/LaunchAgents/$PG_TUNNEL_LABEL.plist"
PG_TUNNEL_BIN="$ADK_REL/bin/pg-tunnel.sh"
PG_TUNNEL_CONFIG="$ADK_REL/config/pg-tunnel.env"
PG_TUNNEL_LAUNCHD_DOMAIN="$(_launchd_domain)"

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
    <string>-L</string><string>127.0.0.1:15432:/tmp/.s.PGSQL.5432</string>
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
    mv -f "$PG_TUNNEL_PLIST_PATH.tmp" "$PG_TUNNEL_PLIST_PATH" || return 1
}

_pg_write_probe_conninfo() {
    local local_port=$1 output_dir=$2 password_output=$3
    local config_path="$ADK_REL/config/agentdesk.yaml"
    command -v ruby >/dev/null 2>&1 || return 1
    if [ -n "${DATABASE_URL:-}" ]; then
        DATABASE_URL="$DATABASE_URL" ruby -ruri - "$local_port" "$output_dir" "$password_output" \
            2>/dev/null <<'RUBY'
port, output_dir, password_output = ARGV
uri = URI.parse(ENV.fetch("DATABASE_URL"))
raise "unsupported database URL scheme" unless %w[postgres postgresql].include?(uri.scheme)
decode = ->(value) { URI::DEFAULT_PARSER.unescape(value.to_s) }
host = uri.hostname.to_s
user = decode.call(uri.user)
name = decode.call(uri.path.to_s.sub(%r{\A/}, ""))
password = decode.call(uri.password) if uri.password
option_env = {
  "application_name" => "PGAPPNAME",
  "channel_binding" => "PGCHANNELBINDING",
  "client_encoding" => "PGCLIENTENCODING",
  "connect_timeout" => "PGCONNECT_TIMEOUT",
  "fallback_application_name" => "PGAPPNAME",
  "gssencmode" => "PGGSSENCMODE",
  "krbsrvname" => "PGKRBSRVNAME",
  "options" => "PGOPTIONS",
  "require_auth" => "PGREQUIREAUTH",
  "sslcert" => "PGSSLCERT",
  "sslcrl" => "PGSSLCRL",
  "sslcrldir" => "PGSSLCRLDIR",
  "sslkey" => "PGSSLKEY",
  "sslmode" => "PGSSLMODE",
  "sslrootcert" => "PGSSLROOTCERT",
  "ssl_max_protocol_version" => "PGSSLMAXPROTOCOLVERSION",
  "ssl_min_protocol_version" => "PGSSLMINPROTOCOLVERSION",
  "target_session_attrs" => "PGTARGETSESSIONATTRS"
}
fields = {}
uri.query.to_s.split("&", -1).each do |pair|
  next if pair.empty?
  encoded_key, encoded_value = pair.split("=", 2)
  key = decode.call(encoded_key)
  value = decode.call(encoded_value)
  case key
  when "password"
    password = value
  when "user"
    user = value
  when "dbname"
    name = value
  when "host", "hostaddr", "port"
    next
  else
    env_name = option_env[key]
    fields[env_name] = value if env_name
  end
end
raise "database host, user, or name missing" if host.empty? || user.empty? || name.empty?
fields.merge!("PGHOST" => host, "PGHOSTADDR" => "127.0.0.1",
              "PGPORT" => Integer(port, 10).to_s,
              "PGUSER" => user, "PGDATABASE" => name)
raise "NUL is not allowed in PostgreSQL settings" if fields.values.any? { |value| value.include?("\0") }
fields.each do |env_name, value|
  File.open(File.join(output_dir, env_name), File::WRONLY | File::CREAT | File::TRUNC, 0o600) do |file|
    file.write(value)
  end
end
if password
  raise "NUL is not allowed in PostgreSQL password" if password.include?("\0")
  escape = lambda do |value|
    value.to_s.gsub("\\") { "\\\\" }.gsub(":") { "\\:" }
  end
  File.open(password_output, File::WRONLY | File::CREAT | File::TRUNC, 0o600) do |file|
    file.write([host, port, name, user, password].map { |value| escape.call(value) }.join(":") + "\n")
  end
end
RUBY
        return $?
    fi
    [ -r "$config_path" ] || return 1
    ruby -ryaml - "$config_path" "$local_port" "$output_dir" "$password_output" \
        2>/dev/null <<'RUBY'
config_path, port, output_dir, password_output = ARGV
config = YAML.safe_load(File.read(config_path), aliases: true) || {}
db = config.fetch("database", {})
raise "database disabled" unless db["enabled"] == true
host = db.fetch("host").to_s
user = db.fetch("user").to_s
name = db.fetch("dbname").to_s
raise "database host, user, or name missing" if host.empty? || user.empty? || name.empty?
fields = { "PGHOST" => host, "PGHOSTADDR" => "127.0.0.1",
           "PGPORT" => Integer(port, 10).to_s,
           "PGUSER" => user, "PGDATABASE" => name }
raise "NUL is not allowed in PostgreSQL settings" if fields.values.any? { |value| value.include?("\0") }
fields.each do |env_name, value|
  File.open(File.join(output_dir, env_name), File::WRONLY | File::CREAT | File::TRUNC, 0o600) do |file|
    file.write(value)
  end
end
if db.key?("password") && !db["password"].nil?
  password = db["password"].to_s
  raise "NUL is not allowed in PostgreSQL password" if password.include?("\0")
  escape = lambda do |value|
    value.to_s.gsub("\\") { "\\\\" }.gsub(":") { "\\:" }
  end
  File.open(password_output, File::WRONLY | File::CREAT | File::TRUNC, 0o600) do |file|
    file.write([host, port, name, user, password].map { |value| escape.call(value) }.join(":") + "\n")
  end
end
RUBY
}

_pg_sql_probe() {
    local local_port=$1 minimum_wait_secs=${2:-5} conninfo_dir password_file
    local attempt=0 max_attempts name value connect_timeout_seen=0
    local -a psql_env=(env -u DATABASE_URL)
    local -a clear_names=(
        PGAPPNAME PGCHANNELBINDING PGCLIENTENCODING PGCONNECT_TIMEOUT PGDATABASE
        PGGSSENCMODE PGHOST PGHOSTADDR PGKEEPALIVES PGKEEPALIVESCOUNT
        PGKEEPALIVESIDLE PGKEEPALIVESINTERVAL PGKRBSRVNAME PGLOADBALANCEHOSTS
        PGOPTIONS PGPASSFILE PGPASSWORD PGPORT PGREQUIREAUTH PGSERVICE
        PGSERVICEFILE PGSSLCERT PGSSLCRL PGSSLCRLDIR PGSSLKEY
        PGSSLMAXPROTOCOLVERSION PGSSLMINPROTOCOLVERSION PGSSLMODE
        PGSSLNEGOTIATION PGSSLROOTCERT PGTARGETSESSIONATTRS PGTCPUSER_TIMEOUT
        PGUSER
    )
    local -a conninfo_names=(
        PGAPPNAME PGCHANNELBINDING PGCLIENTENCODING PGCONNECT_TIMEOUT
        PGGSSENCMODE PGHOST PGHOSTADDR PGKRBSRVNAME PGOPTIONS PGPORT
        PGREQUIREAUTH PGSSLCERT PGSSLCRL PGSSLCRLDIR PGSSLKEY
        PGSSLMAXPROTOCOLVERSION PGSSLMINPROTOCOLVERSION PGSSLMODE
        PGSSLROOTCERT PGTARGETSESSIONATTRS PGUSER PGDATABASE
    )
    command -v psql >/dev/null 2>&1 || return 1
    for name in "${clear_names[@]}"; do
        psql_env+=(-u "$name")
    done
    conninfo_dir=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-pg-probe.XXXXXX") || return 1
    PG_TUNNEL_PREFLIGHT_CONNINFO_DIR="$conninfo_dir"
    if ! password_file=$(mktemp "${TMPDIR:-/tmp}/agentdesk-pgpass.XXXXXX"); then
        rm -rf "$conninfo_dir" 2>/dev/null || true
        PG_TUNNEL_PREFLIGHT_CONNINFO_DIR=""
        return 1
    fi
    PG_TUNNEL_PREFLIGHT_PASSWORD_FILE="$password_file"
    chmod 700 "$conninfo_dir" || return 1
    chmod 600 "$password_file" || return 1
    _pg_write_probe_conninfo "$local_port" "$conninfo_dir" "$password_file" || return 1
    for name in "${conninfo_names[@]}"; do
        if [ -f "$conninfo_dir/$name" ]; then
            value=$(<"$conninfo_dir/$name")
            psql_env+=("$name=$value")
            [ "$name" != PGCONNECT_TIMEOUT ] || connect_timeout_seen=1
        fi
    done
    if [ "$connect_timeout_seen" = 0 ]; then
        psql_env+=("PGCONNECT_TIMEOUT=5")
    fi
    if [ -s "$password_file" ]; then
        psql_env+=("PGPASSFILE=$password_file")
    else
        psql_env+=("PGPASSFILE=/dev/null")
    fi
    max_attempts=$((minimum_wait_secs * 4))
    [ "$max_attempts" -ge 20 ] || max_attempts=20
    while [ "$attempt" -lt "$max_attempts" ]; do
        if "${psql_env[@]}" psql --no-psqlrc \
          -v ON_ERROR_STOP=1 -Atqc 'SELECT 1' >/dev/null 2>&1; then
            rm -rf "$conninfo_dir"
            rm -f "$password_file"
            PG_TUNNEL_PREFLIGHT_CONNINFO_DIR=""
            PG_TUNNEL_PREFLIGHT_PASSWORD_FILE=""
            return 0
        fi
        sleep 0.25
        attempt=$((attempt + 1))
    done
    return 1
}

_migrate_pg_tunnel_before_release_stop() {
    local probe_port wrapper_source="$REPO/scripts/pg_tunnel.sh"
    [ -f "$PG_TUNNEL_CONFIG" ] || {
        echo "▸ PG tunnel config absent: $PG_TUNNEL_CONFIG"
        echo "  Supervisor NOT armed on this node (machine-local node gate)."
        return 0
    }
    [ -x "$wrapper_source" ] || { echo "✗ PG tunnel wrapper missing: $wrapper_source"; return 1; }
    "$wrapper_source" --check-config "$PG_TUNNEL_CONFIG" || {
        echo "✗ PG tunnel config invalid: $PG_TUNNEL_CONFIG"
        return 1
    }

    probe_port=$((20000 + ($$ % 20000)))
    echo "▸ Proving remote PostgreSQL Unix-socket route on alternate port..."
    "$wrapper_source" --probe-remote "$PG_TUNNEL_CONFIG" "$probe_port" \
        >/dev/null 2>&1 &
    PG_TUNNEL_PREFLIGHT_PID=$!
    if ! _pg_sql_probe "$probe_port"; then
        echo "✗ Remote PostgreSQL Unix-socket SQL probe failed"
        _cleanup_owned_pg_tunnel_preflight
        return 1
    fi
    _cleanup_owned_pg_tunnel_preflight

    PG_TUNNEL_ROLLBACK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-pg-rollback.XXXXXX") || return 1
    if [ -e "$PG_TUNNEL_BIN" ] \
      && ! cp -p "$PG_TUNNEL_BIN" "$PG_TUNNEL_ROLLBACK_DIR/wrapper"; then
        echo "✗ Failed to snapshot existing PG tunnel wrapper"
        rm -rf "$PG_TUNNEL_ROLLBACK_DIR" 2>/dev/null || true
        _reset_pg_tunnel_rollback_state
        return 1
    fi
    if [ -e "$PG_TUNNEL_PLIST_PATH" ] \
      && ! cp -p "$PG_TUNNEL_PLIST_PATH" "$PG_TUNNEL_ROLLBACK_DIR/plist"; then
        echo "✗ Failed to snapshot existing PG tunnel launchd plist"
        rm -rf "$PG_TUNNEL_ROLLBACK_DIR" 2>/dev/null || true
        _reset_pg_tunnel_rollback_state
        return 1
    fi
    if ! cp -p "$PG_TUNNEL_CONFIG" "$PG_TUNNEL_ROLLBACK_DIR/config"; then
        echo "✗ Failed to snapshot PG tunnel machine config"
        rm -rf "$PG_TUNNEL_ROLLBACK_DIR" 2>/dev/null || true
        _reset_pg_tunnel_rollback_state
        return 1
    fi
    PG_TUNNEL_ROLLBACK_WRAPPER_SOURCE="$wrapper_source"
    PG_TUNNEL_ROLLBACK_MANUAL_CONFIG="$PG_TUNNEL_ROLLBACK_DIR/config"
    PG_TUNNEL_ROLLBACK_JOB_LOADED=0
    PG_TUNNEL_ROLLBACK_MANUAL_KIND="none"
    if launchctl print "$PG_TUNNEL_LAUNCHD_DOMAIN/$PG_TUNNEL_LABEL" >/dev/null 2>&1; then
        if [ ! -f "$PG_TUNNEL_ROLLBACK_DIR/wrapper" ] \
          || [ ! -f "$PG_TUNNEL_ROLLBACK_DIR/plist" ]; then
            echo "✗ Loaded PG tunnel job lacks restorable wrapper/plist snapshots"
            rm -rf "$PG_TUNNEL_ROLLBACK_DIR" 2>/dev/null || true
            _reset_pg_tunnel_rollback_state
            return 1
        fi
        PG_TUNNEL_ROLLBACK_JOB_LOADED=1
    else
        if ! PG_TUNNEL_ROLLBACK_MANUAL_KIND=$(
            "$wrapper_source" --canonical-kind 2>/dev/null
        ); then
            echo "✗ Failed to snapshot existing manual PG tunnel state"
            rm -rf "$PG_TUNNEL_ROLLBACK_DIR" 2>/dev/null || true
            _reset_pg_tunnel_rollback_state
            return 1
        fi
        case "$PG_TUNNEL_ROLLBACK_MANUAL_KIND" in
            none|tcp|unix) ;;
            *)
                echo "✗ Refusing unknown PG tunnel rollback kind"
                rm -rf "$PG_TUNNEL_ROLLBACK_DIR" 2>/dev/null || true
                _reset_pg_tunnel_rollback_state
                return 1
                ;;
        esac
    fi
    PG_TUNNEL_ROLLBACK_ARMED=1
    echo "▸ PG tunnel rollback armed; recovery material: $PG_TUNNEL_ROLLBACK_DIR"

    install -m 0755 "$wrapper_source" "$PG_TUNNEL_BIN" || return 1
    _install_pg_tunnel_plist || return 1
    xattr -d com.apple.quarantine "$PG_TUNNEL_PLIST_PATH" 2>/dev/null || true
    launchctl bootout "$PG_TUNNEL_LAUNCHD_DOMAIN/$PG_TUNNEL_LABEL" 2>/dev/null || true
    if ! "$wrapper_source" --take-over-canonical; then
        echo "✗ Failed to synchronously take over the canonical PG tunnel"
        return 1
    fi
    if ! _pg_wait_canonical_listener_absent; then
        echo "✗ Canonical PG tunnel listener survived synchronous takeover"
        return 1
    fi
    if ! launchctl bootstrap "$PG_TUNNEL_LAUNCHD_DOMAIN" "$PG_TUNNEL_PLIST_PATH"; then
        echo "✗ PG tunnel bootstrap failed"
        return 1
    fi
    echo "▸ Proving canonical PostgreSQL tunnel readiness on :15432..."
    if ! _pg_sql_probe 15432; then
        echo "✗ Canonical PostgreSQL tunnel SQL readiness failed"
        return 1
    fi

    rm -rf "$PG_TUNNEL_ROLLBACK_DIR" || return 1
    _reset_pg_tunnel_rollback_state
    echo "✓ PG tunnel migrated and SQL-ready before release stop"
}

_migrate_pg_tunnel_before_release_stop

LOCK_FILE="$ADK_REL/runtime/dcserver.lock"
if ! _capture_release_old_process; then
    echo "✗ Could not capture the exact current release process before migration" >&2
    exit 1
fi

# Apply the forward-only database boundary before requesting restart_pending.
# The runtime may consume a persisted restart request and exit on its own, so no
# drain marker or self-exit trigger may exist when candidate migration runs. The
# tunnel migration above is a fail-closed, SQL-ready prerequisite; its EXIT trap
# restores the previous tunnel state if that prerequisite itself fails.
echo "▸ Applying release PostgreSQL migrations before restart drain..."
if ! _apply_release_postgres_migration_with_forward_barrier; then
    exit 1
fi

# Migration 0100 is now a forward-only binary floor: once it commits, a pre-0100
# binary cannot restart because SQLx rejects a database migration newer than its
# embedded manifest. From this point onward failures must fail forward with the
# staged 0100-aware binary; do not claim that the old runtime can be preserved.
# Fence new relay admissions and let dcserver atomically persist each in-flight
# delivery frontier before launchd is allowed to stop it. The runtime consumes
# restart_pending only after queue/checkpoint state and DrainRestart markers are
# durable; the replacement watcher then resumes from those committed offsets.
AGENTDESK_RESTART_ALLOW_FOREIGN_TURNS=1
export AGENTDESK_RESTART_ALLOW_FOREIGN_TURNS
if ! request_restart_drain_mode_or_fail \
    "release" "$PLIST_REL" "$REL_PORT" "$ADK_REL/runtime" "deploy-release"; then
    exit 1
fi
RESTART_REQUEST_NONCE="${AGENTDESK_RESTART_REQUEST_NONCE:-}"
if [ "${AGENTDESK_RESTART_PERSISTENCE_NOT_REQUIRED:-0}" != "1" ]; then
    if [ -z "$RESTART_REQUEST_NONCE" ]; then
        echo "✗ [gate] release restart request nonce missing" >&2
        clear_restart_drain_mode "$ADK_REL/runtime" || true
        exit 1
    fi
    if ! wait_for_restart_persistence_or_fail \
        "release" "$ADK_REL/runtime" "$RESTART_REQUEST_NONCE" 30; then
        exit 1
    fi
fi

# A planned restart no longer suppresses transcript gaps: the watchdog's durable
# pre-restart authority must remain observable until Discord delivery catches up.
# Remove a marker left by an older deploy so its quiet window cannot mask this
# restart boundary after the runtime has proved its replay frontier durable.
rm -f "$ADK_REL/logs/relay-watchdog.deploy-marker" 2>/dev/null || true
rm -f "$ADK_REL/runtime/restart_persisted" 2>/dev/null || true

# Stop release only after migration and the durable persistence acknowledgement.
echo "▸ Stopping release..."
LAUNCHD_DOMAIN="$(_launchd_domain)"
if ! _stop_release_for_promotion; then
    exit 1
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
if [ -e "$REL_BINARY" ] \
  && ! _set_release_binary_immutable_state "$REL_BINARY" 0; then
    echo "✗ Could not clear and verify the live release binary immutable flag" >&2
    exit 1
fi
case "$REL_ROLLBACK_MATERIAL_MODE" in
preserve)
    # #3858 (re-entrancy / finding 2): treat .prev as last-KNOWN-GOOD. A leftover
    # .prev means a PRIOR deploy failed before its success-path cleanup, so it
    # holds that deploy's last good binary (captured when the then-live binary was
    # still healthy). The CURRENT live binary may be the unverified/bad binary the
    # prior deploy promoted — do NOT overwrite a good .prev with it. Preserve the
    # existing last-known-good as the rollback target so a re-run can still recover.
    # The branch was selected and validated before release stop; repeat the exact
    # digest check here only as a JIT tamper guard.
    if ! _rollback_backup_latest_migration_name allow-prior >/dev/null; then
        echo "✗ Existing rollback generation changed after preflight"
        echo "  refusing to trust stale $REL_BINARY_BACKUP"
        exit 1
    fi
    # The preserved bytes and current live assets are an exact generation. Bind
    # that pair to THIS transaction before it can become this deploy's rollback
    # target; a later rollback refuses metadata from any unrelated transaction.
    if ! _write_rollback_backup_metadata \
        "$REL_BINARY_BACKUP" "$REL_BINARY_BACKUP_META.tmp" \
        "$ROUTINE_ASSET_TXN" \
      || ! mv -f "$REL_BINARY_BACKUP_META.tmp" "$REL_BINARY_BACKUP_META"; then
        rm -f "$REL_BINARY_BACKUP_META.tmp" 2>/dev/null || true
        echo "✗ Could not rebind preserved rollback generation to current asset transaction"
        exit 1
    fi
    echo "▸ Preserving generation-verified last-known-good rollback bundle..."
    # Ensure it is mutable so the rollback's `mv -f` can consume it.
    chflags nouchg "$REL_BINARY_BACKUP" 2>/dev/null || true
    ;;
capture)
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
    _write_rollback_backup_metadata \
        "$REL_BINARY_BACKUP.tmp" "$REL_BINARY_BACKUP_META.tmp" \
        "$ROUTINE_ASSET_TXN"
    mv -f "$REL_BINARY_BACKUP.tmp" "$REL_BINARY_BACKUP"
    if ! mv -f "$REL_BINARY_BACKUP_META.tmp" "$REL_BINARY_BACKUP_META"; then
        rm -f "$REL_BINARY_BACKUP" 2>/dev/null || true
        echo "✗ Could not atomically bind rollback metadata to the backup"
        exit 1
    fi
    ;;
none)
    # Fresh install: preflight proved that no live or rollback binary exists.
    ;;
*)
    echo "✗ Rollback generation mode was not decided before release stop" >&2
    exit 1
    ;;
esac

# Promote the fully validated asset pair before the binary. The durable marker
# was armed inside the shared state machine before its first rename, while the
# binary rollback flag is still clear. Therefore a signal in either asset mv
# rolls assets back without ever exposing a new binary with a partial payload.
echo "▸ Promoting routine asset transaction..."
if ! adk_promote_routine_asset_transaction \
    "$ADK_REL" "$ROUTINE_ASSET_TXN" "$STAGED_BINARY"; then
    echo "✗ Routine asset transaction promotion failed"
    exit 1
fi

echo "▸ Promoting staged binary..."
# Arm the coordinated EXIT path before the atomic rename. A TERM between mv and
# the next shell statement must still restore the binary and its matching assets.
ROLLBACK_ARMED=1
mv -f "$STAGED_BINARY" "$REL_BINARY"
STAGED_BINARY=""
# #3858: ANY non-zero exit before DEPLOY_OK (set on the success path) restores
# the last-known-good backup and matching assets — see _rollback_release_binary.
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
REL_PORT="$(_resolve_release_server_port)"
if ! _persist_release_candidate_drain_authority "$ROUTINE_ASSET_TXN"; then
    echo "✗ Could not arm durable release candidate drain authority" >&2
    exit 1
fi
if ! launchctl bootstrap "$LAUNCHD_DOMAIN" "$HOME/Library/LaunchAgents/$PLIST_REL.plist"; then
    echo "⚠ launchd bootstrap failed for $LAUNCHD_DOMAIN/$PLIST_REL — using tmux fallback"
    start_release_tmux_fallback
fi
if ! _capture_release_candidate_process; then
    echo "✗ Release started without a provable candidate PID/identity; refusing commit" >&2
    exit 1
fi
if ! _persist_release_candidate_drain_authority "$ROUTINE_ASSET_TXN" \
    "$RELEASE_CANDIDATE_PID" "$RELEASE_CANDIDATE_IDENTITY"; then
    echo "✗ Could not persist the exact release candidate drain authority" >&2
    exit 1
fi

# Health check (server health + dashboard availability)
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

# Immutable protection is part of the generation transaction: apply and verify
# it while rollback is still armed, before backup retirement, commit intent, or
# DEPLOY_OK can make the candidate authoritative.
if ! _set_release_binary_immutable_state "$REL_BINARY" 1; then
    echo "✗ Healthy release could not be protected with a verified immutable flag" >&2
    exit 1
fi

# #4902: retire the old rollback binary BEFORE committing its matching .old
# asset trees. Forward-only recovery uses the same gate; neither path may
# publish a committed new generation while a usable incompatible .prev remains.
if ! _retire_release_rollback_material; then
    echo "✗ Healthy binary is serving, but rollback backup cleanup failed" >&2
    echo "  asset commit was withheld; durable forward recovery retains generation identity" >&2
    exit 1
fi

# Health passed and no prior rollback target survived. Persist commit intent
# only now, then disarm binary rollback. A signal after the durable intent keeps
# the proven binary and finishes the matching asset generation forward.
if ! adk_mark_routine_asset_transaction_committing "$ADK_REL" "$ROUTINE_ASSET_TXN"; then
    echo "✗ Could not persist healthy routine asset commit intent" >&2
    exit 1
fi
DEPLOY_OK=1
if ! adk_commit_routine_asset_transaction "$ADK_REL" "$ROUTINE_ASSET_TXN"; then
    echo "⚠ Healthy release retained a committing routine asset transaction"
fi

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
WATCHDOG_SCRIPT_CHANGED=1
if [ -f "$WATCHDOG_BIN" ] && cmp -s "$REPO/scripts/relay_watchdog.py" "$WATCHDOG_BIN"; then
    WATCHDOG_SCRIPT_CHANGED=0
fi
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
        else
            WATCHDOG_PLIST_BEFORE="$WATCHDOG_PLIST_PATH.deploy-prev.$$"
            rm -f "$WATCHDOG_PLIST_BEFORE" 2>/dev/null || true
            if [ -f "$WATCHDOG_PLIST_PATH" ]; then
                cp -p "$WATCHDOG_PLIST_PATH" "$WATCHDOG_PLIST_BEFORE" 2>/dev/null || true
            fi
            if _install_relay_watchdog_plist; then
                WATCHDOG_PLIST_CHANGED=1
                if [ -f "$WATCHDOG_PLIST_BEFORE" ] \
                  && cmp -s "$WATCHDOG_PLIST_BEFORE" "$WATCHDOG_PLIST_PATH"; then
                    WATCHDOG_PLIST_CHANGED=0
                fi
                rm -f "$WATCHDOG_PLIST_BEFORE" 2>/dev/null || true
                xattr -d com.apple.quarantine "$WATCHDOG_PLIST_PATH" 2>/dev/null || true
                _wd_loaded=0
                if launchctl print "$LAUNCHD_DOMAIN/$WATCHDOG_LABEL" >/dev/null 2>&1; then
                    _wd_loaded=1
                fi
                if [ "$WATCHDOG_SCRIPT_CHANGED" = "0" ] \
                  && [ "$WATCHDOG_PLIST_CHANGED" = "0" ] \
                  && [ "$_wd_loaded" = "1" ]; then
                    echo "✓ Relay watchdog retained ($WATCHDOG_LABEL; durable authority uninterrupted)"
                else
                    # Restart only when deployment material changed or the job is absent.
                    # The watermark lives in the atomic state file, so replacement loads
                    # the same pre-restart transcript authority before its first tick.
                    launchctl bootout "$LAUNCHD_DOMAIN/$WATCHDOG_LABEL" 2>/dev/null || true
                    _wd_bootout_polls=0
                    while launchctl print "$LAUNCHD_DOMAIN/$WATCHDOG_LABEL" >/dev/null 2>&1; do
                        if [ "$_wd_bootout_polls" -ge 12 ]; then
                            echo "⚠ Relay watchdog still unloading ~6s after bootout — bootstrapping anyway"
                            break
                        fi
                        sleep 0.5
                        _wd_bootout_polls=$((_wd_bootout_polls + 1))
                    done
                    _wd_armed=0
                    for _wd_attempt in 1 2 3; do
                        if launchctl bootstrap "$LAUNCHD_DOMAIN" "$WATCHDOG_PLIST_PATH"; then
                            _wd_armed=1
                            break
                        fi
                        if [ "$_wd_attempt" -lt 3 ]; then
                            echo "⚠ Relay watchdog bootstrap attempt $_wd_attempt failed — retrying in 2s"
                            sleep 2
                        fi
                    done
                    if [ "$_wd_armed" = "1" ]; then
                        echo "✓ Relay watchdog armed ($WATCHDOG_LABEL)"
                    else
                        echo "⚠ Relay watchdog bootstrap FAILED after 3 attempts — relay gaps will go unwatched"
                    fi
                fi
            else
                rm -f "$WATCHDOG_PLIST_PATH.tmp" "$WATCHDOG_PLIST_BEFORE" 2>/dev/null || true
                echo "⚠ Relay watchdog plist write FAILED ($WATCHDOG_PLIST_PATH) — not armed"
                echo "  Deploy continues (fail-open): fix permissions/disk space and redeploy."
            fi
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

_write_release_source_manifest

echo "═══ Deploy Complete ═══"

if [ "$DEPLOY_ALL_NODES" = "1" ]; then
    _deploy_to_all_peers "$@"
fi
