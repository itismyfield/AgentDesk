#!/usr/bin/env bash

_deploy_cluster_role() {
    local env_file="$REL_LAUNCHD_ENV_FILE"
    local -a shell_args
    [ "${1:-}" != existing ] || env_file=""
    case "$env_file" in
        ''|/*) ;;
        *) env_file="$PWD/$env_file" ;;
    esac
    if [ "${1:-}" = existing ] || [ "$PLIST_REL" != com.agentdesk.release ]; then
        shell_args=(python3 -I "$SCRIPT_DIR/refresh_release_launchd_plist.py" --role-context
            "$HOME/Library/LaunchAgents/$PLIST_REL.plist" "$PLIST_REL" "$BASH"
            --noprofile --norc -p -s -- "$SCRIPT_DIR" "$env_file" existing)
    else
        # Match the generated service environment before applying its overrides.
        local release_path="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:$HOME/.cargo/bin:$HOME/.local/bin:$HOME/bin"
        shell_args=(env -i HOME="$HOME" PATH="$release_path" AGENTDESK_ROOT_DIR="$ADK_REL"
            AGENTDESK_DCSERVER_LABEL="$PLIST_REL" "$BASH" --noprofile --norc -s -- "$SCRIPT_DIR" "$env_file")
    fi
    "${shell_args[@]}" <<EOF
set -euo pipefail
$(declare -f _read_deploy_cluster_role _release_config_path _apply_launchd_env_file_to_shell _parse_launchd_env_line _trim_whitespace)
SCRIPT_DIR=\$1
REL_LAUNCHD_ENV_FILE=\$2
if [ "\${3:-}" = existing ]; then
    ADK_REL=\$(pwd -P) || exit 1
else
    ADK_REL=\$AGENTDESK_ROOT_DIR
fi
_read_deploy_cluster_role
EOF
}

_read_deploy_cluster_role() {
    CDPATH='' cd -- "$ADK_REL" || return 1
    if [ -f "$REL_LAUNCHD_ENV_FILE" ]; then
        _apply_launchd_env_file_to_shell "$REL_LAUNCHD_ENV_FILE" || return 1
    fi
    unset BASH_ENV ENV
    if [ -z "${HOME:-}" ] || [ -z "${PATH:-}" ]; then
        echo '✗ Cannot verify the release service environment: HOME and PATH are required.' >&2
        return 1
    fi
    local config_path="" default_root release_cwd runtime_root root previous_root=""
    release_cwd="$(pwd -P)" || return 1
    default_root="$HOME/.adk/release"
    runtime_root="$(_trim_whitespace "${AGENTDESK_ROOT_DIR:-}")"
    # Reuse the resolver for each runtime search root while keeping the service cwd.
    for root in "${runtime_root:-$default_root}" "$release_cwd" "$default_root"; do
        [ "$root" != "$previous_root" ] || continue
        previous_root="$root"
        [ -n "${AGENTDESK_CONFIG:-}" ] || [ -d "$root" ] || continue
        config_path="$(ADK_REL="$root" _release_config_path)" && [ -n "$config_path" ] || return 1
        if [ -n "${AGENTDESK_CONFIG:-}" ] || [ -e "$config_path" ] || [ -L "$config_path" ]; then
            break
        fi
    done
    [ -n "$config_path" ] || return 1
    if [ ! -e "$config_path" ] && [ ! -L "$config_path" ]; then
        [ -z "${AGENTDESK_CONFIG:-}" ] || return 1
        echo standalone
        return 0
    fi
    python3 - "$config_path" <<'PY'
import json
import subprocess
import sys

try:
    try:
        import yaml
    except ImportError:
        data = json.loads(subprocess.check_output(
            ["ruby", "-ryaml", "-rjson", "-e",
             "config = YAML.safe_load(File.read(ARGV[0]), aliases: true); "
             "puts JSON.generate(config.nil? ? {} : config.fetch('cluster', {}))", sys.argv[1]],
            stderr=subprocess.DEVNULL))
    else:
        with open(sys.argv[1], encoding="utf-8") as handle:
            config = yaml.safe_load(handle)
        data = config.get("cluster", {}) if config is not None else {}
    enabled = data.get("enabled", False)
    if not isinstance(enabled, bool):
        raise ValueError("invalid cluster.enabled")
    if not enabled:
        print("standalone")
    else:
        role = data.get("role", "auto").strip().lower()
        print({"hub": "primary", "leader": "primary", "runner": "peer",
               "worker": "peer", "auto": "auto"}[role])
except Exception:
    # Configuration can contain credentials. Emit only the derived role.
    sys.exit(1)
PY
}

_require_deploy_source_builder() {
    [ "${AGENTDESK_DEPLOY_PEER_BUILD_FALLBACK:-0}" != 1 ] || return 0
    local role
    if [ "$DEPLOY_PEER_INVOCATION" = 1 ]; then
        echo '✗ DEPLOY_FAILED_PEER: no signed binary received. Deploy from the primary or provide AGENTDESK_DEPLOY_BINARY.' >&2
        return 1
    fi
    if ! role="$(_deploy_cluster_role)"; then
        echo '✗ Cannot determine the cluster role for a source build.' >&2
        return 1
    fi
    case "$role" in
        primary|standalone) return 0 ;;
        peer) echo '✗ Deploy from the primary or provide AGENTDESK_DEPLOY_BINARY.' >&2 ;;
        *) echo '✗ A source build requires a designated primary. Deploy from the primary or provide AGENTDESK_DEPLOY_BINARY.' >&2 ;;
    esac
    return 1
}

_run_peers_only() {
    local role deployed source_identity snapshot rc=0
    if [ "$DEPLOY_PEER_INVOCATION" = 1 ] || ! role="$(_deploy_cluster_role existing)" \
        || [[ "$role" != primary && "$role" != standalone ]]; then
        echo '✗ [peers-only] run on the primary with a known cluster role.' >&2
        return 1
    fi
    if [ -n "${AGENTDESK_DEPLOY_BINARY:-}" ]; then
        echo '✗ [peers-only] uses the installed primary binary. Remove AGENTDESK_DEPLOY_BINARY.' >&2
        return 1
    fi
    if [ -z "$(_resolve_deploy_peers)" ]; then
        echo '✗ [peers-only] no peers configured.' >&2
        return 1
    fi

    DEPLOY_TARGET_SHA="${DEPLOY_TARGET_SHA:-$(git -C "$REPO" rev-parse HEAD)}"
    _verify_deploy_target_sha
    if ! deployed="$(python3 - "$ADK_REL/runtime/release-source.json" "$DEPLOY_TARGET_SHA" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        manifest = json.load(handle)
    profile = manifest["build_profile"]
    if manifest["repo_head"] != sys.argv[2] or profile not in ("release", "release-fast"):
        sys.exit(1)
    print(profile)
except (OSError, ValueError, KeyError, TypeError):
    sys.exit(1)
PY
)"; then
        echo '✗ [peers-only] deployed head does not match the target or the deployed source manifest is incomplete.' >&2
        return 1
    fi

    DEPLOY_PEER_BINARY_SOURCE="$ADK_REL/bin/agentdesk"
    if [ ! -f "$DEPLOY_PEER_BINARY_SOURCE" ] || [ ! -x "$DEPLOY_PEER_BINARY_SOURCE" ]; then
        echo '✗ [peers-only] installed primary binary is missing or not executable.' >&2
        return 1
    fi
    if ! _peer_binary_signature "$DEPLOY_PEER_BINARY_SOURCE" >/dev/null; then
        echo '✗ [peers-only] primary binary signature verification failed.' >&2
        return 1
    fi
    if ! source_identity="$(python3 "$SCRIPT_DIR/release_source.py" inspect \
        "$DEPLOY_PEER_BINARY_SOURCE.source.json" "$DEPLOY_PEER_BINARY_SOURCE")" \
        || [ "${source_identity%% *}" != "$DEPLOY_TARGET_SHA" ]; then
        echo '✗ [peers-only] primary binary source or digest does not match the target.' >&2
        return 1
    fi
    if ! DEPLOY_BINARY_PLATFORM="$(_peer_binary_platform)"; then
        echo '✗ [peers-only] primary binary platform probe failed.' >&2
        return 1
    fi
    if ! grep -Fxq "host: ${source_identity#* }" <<<"$DEPLOY_BINARY_PLATFORM"; then
        echo '✗ [peers-only] primary binary target does not match the local toolchain host.' >&2
        return 1
    fi

    snapshot="$(
        _apply_launchd_env_file_to_shell "$REL_LAUNCHD_ENV_FILE" || exit 1
        AGENTDESK_ROOT_DIR="$ADK_REL" _doctor_migration_snapshot "$DEPLOY_PEER_BINARY_SOURCE" require-current
    )" || rc=$?
    case "$rc" in
        0) echo "✓ [peers-only] migrations already applied ($snapshot)" ;;
        2) echo "✗ [peers-only] migrations are not current ($snapshot)" >&2; return 1 ;;
        *) echo '✗ [peers-only] could not verify applied migrations.' >&2; return 1 ;;
    esac

    AGENTDESK_DEPLOY_TARGET_SHA="$DEPLOY_TARGET_SHA"
    export AGENTDESK_DEPLOY_TARGET_SHA
    AGENTDESK_DEPLOY_FAST=0
    [ "$deployed" != release-fast ] || AGENTDESK_DEPLOY_FAST=1
    export AGENTDESK_DEPLOY_FAST
    DEPLOY_BUILT_SOURCE_SHA="$DEPLOY_TARGET_SHA" DEPLOY_BUILD_PROFILE="$deployed" _deploy_to_all_peers "$@"
}
