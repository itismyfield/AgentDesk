#!/usr/bin/env bash

_init_deploy_service_paths() {
    ADK_REL="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"
    PLIST_REL="${AGENTDESK_DCSERVER_LABEL:-${AGENTDESK_PLIST_REL:-com.agentdesk.release}}"
    RELEASE_PLIST_HOME="$HOME"
    [[ "$RELEASE_PLIST_HOME" = /* ]] || RELEASE_PLIST_HOME="$PWD/$RELEASE_PLIST_HOME"
    readonly RELEASE_PLIST_HOME
    readonly RELEASE_PLIST_PATH="$RELEASE_PLIST_HOME/Library/LaunchAgents/$PLIST_REL.plist"
    REL_LAUNCHD_ENV_FILE="$ADK_REL/config/launchd.env"
}

_deploy_service_contexts() {
    local dir
    printf '%s\n' planned
    for dir in "$RELEASE_PLIST_HOME" "$RELEASE_PLIST_HOME/Library" "${RELEASE_PLIST_PATH%/*}"; do
        if [ -d "$dir" ] && [ ! -x "$dir" ]; then
            printf '✗ [current] Cannot inspect the release plist at %s\n' "$RELEASE_PLIST_PATH" >&2
            printf '%s\n' tmux
            return 1
        fi
    done
    if [ -e "$RELEASE_PLIST_PATH" ] || [ -L "$RELEASE_PLIST_PATH" ]; then
        printf '%s\n' current
    fi
    printf '%s\n' tmux
}

_run_deploy_service_context() {
    local context="$1"
    shift
    local env_file="$REL_LAUNCHD_ENV_FILE"
    local -a shell_args
    local role_program
    local -a role_functions=(_check_deploy_role _check_deploy_role_file _read_deploy_role_file _record_deploy_candidate
        _can_inspect_deploy_path _check_tmux_env_literals _apply_launchd_env_file_to_shell
        _parse_launchd_env_line _trim_whitespace)
    if [ "$1" = _verify_peers_only_current ]; then
        role_functions+=(_verify_peers_only_current _doctor_migration_snapshot _peer_binary_signature)
    fi
    [ "$context" != current ] || env_file=""
    case "$env_file" in
        ''|/*) ;;
        *) env_file="$PWD/$env_file" ;;
    esac
    if [ "$context" = current ] || { [ "$context" = planned ] && [ "$PLIST_REL" != com.agentdesk.release ]; }; then
        shell_args=(python3 -I "$SCRIPT_DIR/refresh_release_launchd_plist.py" --role-context
            "$RELEASE_PLIST_PATH" "$PLIST_REL" "$BASH" --noprofile --norc -p -s --)
    else
        # Match the generated service environment before applying its overrides.
        local release_path="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:$HOME/.cargo/bin:$HOME/.local/bin:$HOME/bin"
        shell_args=(env -i HOME="$HOME" PATH="$release_path" AGENTDESK_ROOT_DIR="$ADK_REL"
            AGENTDESK_DCSERVER_LABEL="$PLIST_REL" "$BASH" --noprofile --norc -s --)
    fi
    role_program="set -euo pipefail
$(declare -f "${role_functions[@]}")
SCRIPT_DIR=\$1
REL_LAUNCHD_ENV_FILE=\$2
context=\$3
if [ \"\$context\" = tmux ] || { [ \"\$context\" = planned ] && [ \"\$4\" = com.agentdesk.release ]; }; then
    if [ -e \"\$AGENTDESK_ROOT_DIR\" ] || [ -L \"\$AGENTDESK_ROOT_DIR\" ]; then
        CDPATH='' cd -- \"\$AGENTDESK_ROOT_DIR\" || exit 1
        service_cwd=\$(pwd -P) || exit 1
    elif [[ \"\$AGENTDESK_ROOT_DIR\" = /* ]]; then
        service_cwd=\$AGENTDESK_ROOT_DIR
    else
        service_cwd=\$(pwd -P)/\$AGENTDESK_ROOT_DIR
    fi
else
    service_cwd=\$(pwd -P) || exit 1
fi
shift 4
if [ -e \"\$REL_LAUNCHD_ENV_FILE\" ] || [ -L \"\$REL_LAUNCHD_ENV_FILE\" ]; then
    if [ ! -f \"\$REL_LAUNCHD_ENV_FILE\" ] || [ ! -r \"\$REL_LAUNCHD_ENV_FILE\" ] ||
        { [ \"\$context\" = tmux ] && ! _check_tmux_env_literals \"\$REL_LAUNCHD_ENV_FILE\"; }; then
        printf '✗ [%s] %s · Cannot verify %s release environment\n' \"\$context\" \"\$REL_LAUNCHD_ENV_FILE\" \"\$context\" >&2
        _record_deploy_candidate \"\$context\" \"\$REL_LAUNCHD_ENV_FILE\" read-failure
        exit 1
    fi
    _apply_launchd_env_file_to_shell \"\$REL_LAUNCHD_ENV_FILE\" || exit 1
fi
if [ \"\$context\" = tmux ] && [ \"\${AGENTDESK_ROOT_DIR:-}\" != \"\$service_cwd\" ]; then
    export AGENTDESK_ROOT_DIR=\"\$service_cwd\"
fi
unset BASH_ENV ENV
if [ -z \"\${HOME:-}\" ] || [ -z \"\${PATH:-}\" ]; then
    echo '✗ Cannot verify the release service environment: HOME and PATH are required.' >&2
    exit 1
fi
action=\$1
shift
\"\$action\" \"\$service_cwd\" \"\$@\"
"
    # A pipe keeps the role probe read-only even on shells that write heredocs to disk.
    printf '%s' "$role_program" | "${shell_args[@]}" "$SCRIPT_DIR" "$env_file" "$context" "$PLIST_REL" "$@"
}

_check_tmux_env_literals() {
    # The tmux launcher sources the file. Reject shell syntax the plist loader cannot model.
    local line key value literal='^[[:alnum:]_./:@%+,?#=*-]*$' single="^'[^']*'$"
    local double='^"[^"\\$`]*"$'
    while IFS= read -r line || [ -n "$line" ]; do
        [[ "$line" != *$'\r'* ]] || return 1
        line="$(_trim_whitespace "$line")"
        case "$line" in
            ''|\#*) continue ;;
            'export '*) line="$(_trim_whitespace "${line#export }")" ;;
        esac
        key="${line%%=*}"
        value="${line#*=}"
        [[ "$line" == *=* && "$key" =~ ^[A-Za-z_][A-Za-z_0-9]*$ ]] || return 1
        [[ "$value" =~ $literal || "$value" =~ $single || "$value" =~ $double ]] || return 1
    done < "$1"
}

_check_deploy_role() {
    local service_cwd="$1" context="$2" default_root runtime_root root config_path sibling="" rc=0
    default_root="$HOME/.adk/release"
    runtime_root="$(_trim_whitespace "${AGENTDESK_ROOT_DIR:-}")"
    runtime_root="${runtime_root:-$default_root}"
    [[ "$runtime_root" = /* ]] || runtime_root="$service_cwd/$runtime_root"
    if [ -n "${AGENTDESK_CONFIG:-}" ]; then
        config_path="$AGENTDESK_CONFIG"
        [[ "$config_path" = /* ]] || config_path="$service_cwd/$config_path"
    else
        config_path=""
        for root in "$runtime_root" "$service_cwd" "$default_root"; do
            [[ "$root" = /* ]] || root="$service_cwd/$root"
            if ! _can_inspect_deploy_path "$root/config/agentdesk.yaml"; then
                _check_deploy_role_file "$context" "$root/config/agentdesk.yaml"
                return 1
            fi
            if [ -e "$root/config/agentdesk.yaml" ] || [ -L "$root/config/agentdesk.yaml" ]; then
                config_path="$root/config/agentdesk.yaml"
                break
            fi
            if [ -e "$root/agentdesk.yaml" ] || [ -L "$root/agentdesk.yaml" ]; then
                config_path="$root/agentdesk.yaml"
                break
            fi
        done
        if [ -z "$config_path" ]; then
            _record_deploy_candidate "$context" "$runtime_root/config/agentdesk.yaml" standalone
            return 0
        fi
    fi
    case "$config_path" in
        */config/agentdesk.yaml) sibling="${config_path%/config/agentdesk.yaml}/agentdesk.yaml" ;;
        */agentdesk.yaml) sibling="${config_path%/agentdesk.yaml}/config/agentdesk.yaml" ;;
    esac
    if [ -n "$sibling" ] && ! _can_inspect_deploy_path "$sibling"; then
        _check_deploy_role_file "$context" "$sibling" || rc=1
    fi
    if [ -n "$sibling" ] && { [ -e "$sibling" ] || [ -L "$sibling" ]; }; then
        _check_deploy_role_file "$context" "$sibling" || rc=1
        if [ ! -e "$config_path" ] && [ ! -L "$config_path" ]; then
            return "$rc"
        fi
    fi
    if [ -z "$sibling" ] && { [ -e "$runtime_root/agentdesk.yaml" ] || [ -L "$runtime_root/agentdesk.yaml" ]; } \
        && { [ -e "$runtime_root/config/agentdesk.yaml" ] || [ -L "$runtime_root/config/agentdesk.yaml" ] \
            || ! _can_inspect_deploy_path "$runtime_root/config/agentdesk.yaml"; }; then
        _check_deploy_role_file "$context" "$runtime_root/agentdesk.yaml" || rc=1
        _check_deploy_role_file "$context" "$runtime_root/config/agentdesk.yaml" || rc=1
    fi
    _check_deploy_role_file "$context" "$config_path" || rc=1
    return "$rc"
}

_can_inspect_deploy_path() {
    local dir="${1%/*}"
    while [[ "$dir" = /* ]]; do
        if [ -d "$dir" ] && [ ! -x "$dir" ]; then
            return 1
        fi
        dir="${dir%/*}"
    done
}

_record_deploy_candidate() {
    local file="$2"
    file="${file//%/%25}"
    file="${file//,/%2C}"
    file="${file//:/%3A}"
    file="${file//$'\n'/%0A}"
    printf '%s:%s:%s\n' "$1" "$file" "$3"
}

_check_deploy_role_file() {
    local context="$1" config_path="$2" role cause
    if [ ! -f "$config_path" ] || [ ! -r "$config_path" ]; then
        cause='file missing or unreadable'
    elif ! role="$(_read_deploy_role_file "$config_path")"; then
        cause='invalid configuration or role'
    else
        cause=""
    fi
    if [ -n "$cause" ]; then
        printf '✗ [%s] Cannot determine the cluster role from %s · read-failure (%s)\n' "$context" "$config_path" "$cause" >&2
        _record_deploy_candidate "$context" "$config_path" read-failure
        return 1
    fi
    _record_deploy_candidate "$context" "$config_path" "$role"
    case "$role" in
        hub|standalone) return 0 ;;
        *) printf '✗ [%s] %s role in %s. Deploy from the primary or provide AGENTDESK_DEPLOY_BINARY.\n' \
            "$context" "$role" "$config_path" >&2 ;;
    esac
    return 1
}

_read_deploy_role_file() {
    python3 -B -c '
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
             "puts JSON.generate(config.nil? ? {} : config.fetch(\"cluster\", {}))", sys.argv[1]],
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
        print({"hub": "hub", "leader": "hub", "runner": "runner",
               "worker": "runner", "auto": "auto"}[role])
except Exception:
    # Configuration can contain credentials. Emit only the derived role.
    sys.exit(1)
' "$1"
}

_check_deploy_source_roles() {
    local context contexts records candidates="" verdict=allow path
    if ! contexts="$(_deploy_service_contexts)"; then
        verdict=refuse
        candidates="$(_record_deploy_candidate current "$RELEASE_PLIST_PATH" read-failure)"
    fi
    while IFS= read -r context; do
        [ -n "$context" ] || continue
        if ! records="$(_run_deploy_service_context "$context" _check_deploy_role "$context")"; then
            verdict=refuse
            if [ -z "$records" ]; then
                path="$RELEASE_PLIST_PATH"
                [ "$context" != tmux ] || path="$REL_LAUNCHD_ENV_FILE"
                [[ "$path" = /* ]] || path="$PWD/$path"
                printf '✗ [%s] %s · read-failure (cannot verify service inputs)\n' "$context" "$path" >&2
                records="$(_record_deploy_candidate "$context" "$path" read-failure)"
            fi
        fi
        candidates="${candidates:+$candidates,}${records//$'\n'/,}"
    done < <(printf '%s\n' "$contexts")
    if [ "${1:-}" = --report ]; then
        printf 'DEPLOY_ROLE_CHECK verdict=%s candidates=%s\n' "$verdict" "$candidates"
    fi
    [ "$verdict" = allow ]
}

_require_deploy_source_builder() {
    [ "${AGENTDESK_DEPLOY_PEER_BUILD_FALLBACK:-0}" != 1 ] || return 0
    if [ "$DEPLOY_PEER_INVOCATION" = 1 ]; then
        echo '✗ DEPLOY_FAILED_PEER: no signed binary received. Deploy from the primary or provide AGENTDESK_DEPLOY_BINARY.' >&2
        return 1
    fi
    _check_deploy_source_roles quiet
}

_verify_peers_only_current() {
    local service_cwd="$1" expected_root="$2" target="$3" platform="$4"
    local root binary deployed source_identity snapshot rc=0
    _check_deploy_role "$service_cwd" current >/dev/null || return 1
    root="$(_trim_whitespace "${AGENTDESK_ROOT_DIR:-}")"
    root="$(CDPATH='' cd -- "${root:-$HOME/.adk/release}" && pwd -P)" || return 1
    if [ "$root" != "$expected_root" ]; then
        printf '✗ [peers-only] service root does not match the deployment root (%s).\n' "$root" >&2
        return 1
    fi

    if ! deployed="$(python3 - "$root/runtime/release-source.json" "$target" <<'PY'
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

    binary="$root/bin/agentdesk"
    if [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
        echo '✗ [peers-only] installed primary binary is missing or not executable.' >&2
        return 1
    fi
    if ! _peer_binary_signature "$binary" >/dev/null; then
        echo '✗ [peers-only] primary binary signature verification failed.' >&2
        return 1
    fi
    if ! source_identity="$(python3 "$SCRIPT_DIR/release_source.py" inspect \
        "$binary.source.json" "$binary")" || [ "${source_identity%% *}" != "$target" ]; then
        echo '✗ [peers-only] primary binary source or digest does not match the target.' >&2
        return 1
    fi
    if ! grep -Fxq "host: ${source_identity#* }" <<<"$platform"; then
        echo '✗ [peers-only] primary binary target does not match the local toolchain host.' >&2
        return 1
    fi

    snapshot="$(_doctor_migration_snapshot "$binary" require-current)" || rc=$?
    case "$rc" in
        0) printf '%s\n%s\n' "$deployed" "$snapshot" ;;
        2) echo "✗ [peers-only] migrations are not current ($snapshot)" >&2; return 1 ;;
        *) echo '✗ [peers-only] could not verify applied migrations.' >&2; return 1 ;;
    esac
}

_run_peers_only() {
    local root verified deployed snapshot
    if [ "$DEPLOY_PEER_INVOCATION" = 1 ] || ! _run_deploy_service_context tmux _check_deploy_role tmux >/dev/null; then
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
    if ! DEPLOY_BINARY_PLATFORM="$(_peer_binary_platform)"; then
        echo '✗ [peers-only] primary binary platform probe failed.' >&2
        return 1
    fi
    root="$(CDPATH='' cd -- "$ADK_REL" && pwd -P)" || return 1
    if ! verified="$(_run_deploy_service_context current _verify_peers_only_current \
        "$root" "$DEPLOY_TARGET_SHA" "$DEPLOY_BINARY_PLATFORM")"; then
        echo '✗ [peers-only] run on the primary with a known cluster role and verified release.' >&2
        return 1
    fi
    deployed="${verified%%$'\n'*}"
    snapshot="${verified#*$'\n'}"
    echo "✓ [peers-only] migrations already applied ($snapshot)"
    AGENTDESK_DEPLOY_TARGET_SHA="$DEPLOY_TARGET_SHA"
    export AGENTDESK_DEPLOY_TARGET_SHA
    AGENTDESK_DEPLOY_FAST=0
    [ "$deployed" != release-fast ] || AGENTDESK_DEPLOY_FAST=1
    export AGENTDESK_DEPLOY_FAST
    DEPLOY_PEER_BINARY_SOURCE="$root/bin/agentdesk" DEPLOY_BUILT_SOURCE_SHA="$DEPLOY_TARGET_SHA" \
        DEPLOY_BUILD_PROFILE="$deployed" _deploy_to_all_peers "$@"
}
