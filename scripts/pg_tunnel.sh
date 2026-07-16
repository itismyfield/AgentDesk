#!/usr/bin/env bash
set -euo pipefail

# Consumer-owned PostgreSQL tunnel supervisor entrypoint (#4378).
#
# launchd owns this foreground process.  The consumer owns the -L forwarding
# direction so a sleeping/moving mac-book cannot leave a reverse -R process
# alive-but-useless on mac-mini.  The forwarding endpoints are intentionally
# fixed: this P1 path must not become coupled to auxiliary tunnels.
#
# Machine-local config (deploy passes $ADK_REL/config/pg-tunnel.env):
#   PG_TUNNEL_SSH_TARGET=mac-mini
# PG_TUNNEL_HOST is accepted as a backwards-friendly alias.  No config file is
# shipped in the repository; deploy-release.sh only arms launchd when the local
# file exists, passes --check-config, and proves the remote Unix-socket path with
# a dedicated SQL probe.  --check-config validates local configuration only.

# A migration deploy must take over both the new socket target and the old TCP
# target on the exact loopback listener. Other bind addresses and ports remain
# outside this ownership boundary. The endpoint boundary is load-bearing: a
# superstring such as port 54321 or socket .5432.bak must never be signaled.
TAKEOVER_PATTERN='^([^[:space:]]*/)?ssh[[:space:]].*-L[[:space:]]+127[.]0[.]0[.]1:15432:(/tmp/[.]s[.]PGSQL[.]5432|127[.]0[.]0[.]1:5432)([[:space:]]|$)'
UNIX_FORWARD_PATTERN='^([^[:space:]]*/)?ssh[[:space:]].*-L[[:space:]]+127[.]0[.]0[.]1:15432:/tmp/[.]s[.]PGSQL[.]5432([[:space:]]|$)'
TCP_FORWARD_PATTERN='^([^[:space:]]*/)?ssh[[:space:]].*-L[[:space:]]+127[.]0[.]0[.]1:15432:127[.]0[.]0[.]1:5432([[:space:]]|$)'
SSH_BASE_ARGS=(
    -N
    -T
    -o BatchMode=yes
    -o ConnectTimeout=10
    -o ServerAliveInterval=15
    -o ServerAliveCountMax=3
    -o ExitOnForwardFailure=yes
)
EXPECTED_SSH_ARGS=(
    -N
    -T
    -o BatchMode=yes
    -o ConnectTimeout=10
    -o ServerAliveInterval=15
    -o ServerAliveCountMax=3
    -o ExitOnForwardFailure=yes
    -L 127.0.0.1:15432:/tmp/.s.PGSQL.5432
)

die() {
    echo "pg-tunnel: $*" >&2
    exit 1
}

load_config() {
    local config_path=$1
    [ -f "$config_path" ] || die "config not found: $config_path"
    # Operator-owned shell assignments, kept outside the repository.
    # shellcheck disable=SC1090
    . "$config_path"
    PG_TUNNEL_SSH_TARGET="${PG_TUNNEL_SSH_TARGET:-${PG_TUNNEL_HOST:-}}"
    case "$PG_TUNNEL_SSH_TARGET" in
        "") die "PG_TUNNEL_SSH_TARGET is required in $config_path" ;;
        -*) die "PG_TUNNEL_SSH_TARGET must not start with '-'" ;;
        *[!A-Za-z0-9_.:@-]*)
            die "PG_TUNNEL_SSH_TARGET contains unsafe characters"
            ;;
    esac
}

still_matching_manual_tunnel() {
    local pid=$1 command
    command=$(/bin/ps -p "$pid" -o command= 2>/dev/null || true)
    [ -n "$command" ] && printf '%s\n' "$command" | /usr/bin/grep -Eq "$TAKEOVER_PATTERN"
}

take_over_manual_tunnel() {
    local pids pid attempts
    pids=$(/usr/bin/pgrep -f "$TAKEOVER_PATTERN" 2>/dev/null || true)
    [ -n "$pids" ] || return 0

    # pgrep returns one numeric pid per line; intentional whitespace splitting.
    # shellcheck disable=SC2086
    for pid in $pids; do
        case "$pid" in *[!0-9]*|"") continue ;; esac
        # Re-read the full command immediately before signaling.  This narrows
        # the PID-reuse window and makes the strict -L endpoint predicate the
        # final authority; a loose ssh/15432 match must never kill a bystander.
        still_matching_manual_tunnel "$pid" || continue
        echo "pg-tunnel: taking over residual manual tunnel pid=$pid" >&2
        /bin/kill -TERM "$pid" 2>/dev/null || true
        attempts=0
        while /bin/kill -0 "$pid" 2>/dev/null && [ "$attempts" -lt 25 ]; do
            /bin/sleep 0.2
            attempts=$((attempts + 1))
        done
        if /bin/kill -0 "$pid" 2>/dev/null && still_matching_manual_tunnel "$pid"; then
            echo "pg-tunnel: residual pid=$pid ignored TERM; sending KILL" >&2
            /bin/kill -KILL "$pid" 2>/dev/null || true
            attempts=0
            while /bin/kill -0 "$pid" 2>/dev/null && [ "$attempts" -lt 10 ]; do
                /bin/sleep 0.1
                attempts=$((attempts + 1))
            done
            if /bin/kill -0 "$pid" 2>/dev/null && still_matching_manual_tunnel "$pid"; then
                die "residual manual tunnel pid=$pid did not exit; refusing bind race"
            fi
        fi
    done
}

validate_ssh_args() {
    local -a actual=("$@")
    local i
    [ "${#actual[@]}" -eq "${#EXPECTED_SSH_ARGS[@]}" ] ||
        die "refusing non-canonical ssh arguments"
    for ((i = 0; i < ${#EXPECTED_SSH_ARGS[@]}; i++)); do
        [ "${actual[$i]}" = "${EXPECTED_SSH_ARGS[$i]}" ] ||
            die "refusing non-canonical ssh arguments"
    done
}

validate_probe_port() {
    local port=$1
    case "$port" in
        ""|*[!0-9]*) die "probe port must be a decimal integer" ;;
    esac
    [ "$port" -ge 1024 ] && [ "$port" -le 65535 ] ||
        die "probe port must be between 1024 and 65535"
    [ "$port" -ne 15432 ] || die "probe port must not be the canonical port"
}

canonical_forward_kind() {
    local pids pid command kind=""
    pids=$(/usr/bin/pgrep -f "$TAKEOVER_PATTERN" 2>/dev/null || true)
    # pgrep returns one numeric pid per line; intentional whitespace splitting.
    # shellcheck disable=SC2086
    for pid in $pids; do
        case "$pid" in *[!0-9]*|"") continue ;; esac
        command=$(/bin/ps -p "$pid" -o command= 2>/dev/null || true)
        if printf '%s\n' "$command" | /usr/bin/grep -Eq "$UNIX_FORWARD_PATTERN"; then
            [ -z "$kind" ] || [ "$kind" = "unix" ] || die "ambiguous canonical tunnel state"
            kind="unix"
        elif printf '%s\n' "$command" | /usr/bin/grep -Eq "$TCP_FORWARD_PATTERN"; then
            [ -z "$kind" ] || [ "$kind" = "tcp" ] || die "ambiguous canonical tunnel state"
            kind="tcp"
        fi
    done
    printf '%s\n' "${kind:-none}"
}

restore_canonical_forward() {
    local kind=$1 endpoint
    case "$kind" in
        unix) endpoint="127.0.0.1:15432:/tmp/.s.PGSQL.5432" ;;
        tcp) endpoint="127.0.0.1:15432:127.0.0.1:5432" ;;
        *) die "restore kind must be 'unix' or 'tcp'" ;;
    esac
    exec /usr/bin/ssh -f "${SSH_BASE_ARGS[@]}" -L "$endpoint" "$PG_TUNNEL_SSH_TARGET"
}

case "${1:-}" in
    --check-config)
        [ "$#" -eq 2 ] || die "usage: $0 --check-config CONFIG"
        load_config "$2"
        exit 0
        ;;
    --probe-remote)
        [ "$#" -eq 3 ] || die "usage: $0 --probe-remote CONFIG PORT"
        load_config "$2"
        validate_probe_port "$3"
        exec /usr/bin/ssh "${SSH_BASE_ARGS[@]}" \
            -L "127.0.0.1:$3:/tmp/.s.PGSQL.5432" "$PG_TUNNEL_SSH_TARGET"
        ;;
    --canonical-kind)
        [ "$#" -eq 1 ] || die "usage: $0 --canonical-kind"
        canonical_forward_kind
        exit 0
        ;;
    --take-over-canonical)
        [ "$#" -eq 1 ] || die "usage: $0 --take-over-canonical"
        take_over_manual_tunnel
        exit 0
        ;;
    --restore-canonical)
        [ "$#" -eq 3 ] || die "usage: $0 --restore-canonical CONFIG KIND"
        load_config "$2"
        restore_canonical_forward "$3"
        ;;
esac

[ "$#" -ge 1 ] || die "usage: $0 CONFIG SSH_ARGS..."
CONFIG_PATH=$1
shift
load_config "$CONFIG_PATH"
validate_ssh_args "$@"
take_over_manual_tunnel

exec /usr/bin/ssh "$@" "$PG_TUNNEL_SSH_TARGET"
