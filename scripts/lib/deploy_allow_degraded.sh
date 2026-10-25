#!/usr/bin/env bash

# shellcheck disable=SC2120 # The deploy entry also passes CLI flags.
_deploy_allow_degraded_validate() {
    local arg pattern="${AGENTDESK_DEPLOY_ALLOW_DEGRADED:-}" rc=0
    for arg in "$@"; do
        case "$arg" in --skip-health|--skip-review)
            echo "✗ $arg is unsupported; use AGENTDESK_DEPLOY_ALLOW_DEGRADED for named degraded causes" >&2
            return 2 ;;
        esac
    done
    [ "${AGENTDESK_DEPLOY_ALLOW_DEGRADED+x}" = x ] || return 0
    case "$pattern" in ''|*,*|*$'\n'*|*$'\r'*)
        echo '✗ AGENTDESK_DEPLOY_ALLOW_DEGRADED must be a nonempty ERE without commas or line breaks' >&2
        return 2 ;;
    esac
    # shellcheck disable=SC2319 # Status 2 from =~ means an invalid ERE.
    [[ '' =~ $pattern ]] || rc=$?
    if [ "$rc" -gt 1 ]; then
        echo '✗ AGENTDESK_DEPLOY_ALLOW_DEGRADED is not a valid ERE' >&2
        return 2
    fi
}

_deploy_allow_degraded_waived() {
    local health_json="$1" baseline="$2" reasons reason out=""
    local pattern="${AGENTDESK_DEPLOY_ALLOW_DEGRADED:-}"
    [ "$(_health_json_status "$health_json")" = degraded ] || return 0
    reasons=$(_health_json_reasons "$health_json" || true)
    _health_json_reasons_csv_is_well_formed "$reasons" || return 0
    local IFS=','
    for reason in $reasons; do
        # Match independently: a valid ERE can contain a literal unmatched ')'.
        # BASH_REMATCH proves full membership without inserting it into a group.
        if [[ "$reason" =~ $pattern ]] && [ "${BASH_REMATCH[0]}" = "$reason" ] \
            && ! [[ "$reason" =~ $baseline ]]; then
            out="${out:+$out,}$reason"
        fi
    done
    printf '%s' "$out"
}

_deploy_allow_degraded_ere() {
    local deploy="$1" baseline="$2" health_json="$3" waived reason literal ere
    if [ "$deploy" != 1 ] || [ "${AGENTDESK_DEPLOY_ALLOW_DEGRADED+x}" != x ]; then
        printf '%s' "$baseline"
        return 0
    fi
    _deploy_allow_degraded_validate || return 2
    waived=$(_deploy_allow_degraded_waived "$health_json" "$baseline")
    ere="${baseline%')$'}"
    local IFS=','
    for reason in $waived; do
        literal=$(printf '%s' "$reason" | sed 's/[][\\.^$*+?(){}|]/\\&/g')
        ere="$ere|$literal"
    done
    printf '%s)$' "$ere"
}

_deploy_wait_with_allow_degraded() {
    local rc=0 baseline waived health_json
    _deploy_allow_degraded_validate || return 2
    wait_for_http_service_health "$@" || rc=$?
    if [ "${AGENTDESK_DEPLOY_ALLOW_DEGRADED+x}" = x ]; then
        health_json="${WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON:-}"
        baseline=$(unset AGENTDESK_DEPLOY_ALLOW_DEGRADED; \
            _health_json_deploy_nonblocking_ere_for_body "$health_json" "${6:-1}" "${8:-0}")
        waived=$(_deploy_allow_degraded_waived "$health_json" "$baseline")
        printf '▸ [allow-degraded] pattern=<%s> waived=<%s>\n' "$AGENTDESK_DEPLOY_ALLOW_DEGRADED" "${waived:-none}"
    fi
    return "$rc"
}
