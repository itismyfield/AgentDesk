#!/usr/bin/env bash
# The peer env prelude must never forward AGENTDESK_DEPLOY_BINARY: the value is a
# host-local path, and on a peer it also disables the source freshness/identity gates.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Overridable so a mutation run can point the same assertions at a patched copy.
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-peer-binary-test.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

extract_function() {
    local function_name="$1"
    awk -v start="^${function_name}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$DEPLOY_SH"
}

prelude_body="$(extract_function _deploy_peer_env_prelude)"
if [ -z "$prelude_body" ]; then
    printf 'FAIL: could not extract _deploy_peer_env_prelude from %s\n' "$DEPLOY_SH" >&2
    exit 1
fi
eval "$prelude_body"

failures=0
fail_test() {
    printf 'FAIL: %s\n' "$1" >&2
    failures=$((failures + 1))
}

assert_peer_prelude() {
    local label="$1" prelude
    prelude="$(_deploy_peer_env_prelude)"
    case "$prelude" in
        *AGENTDESK_DEPLOY_BINARY=*) fail_test "$label: prelude forwards AGENTDESK_DEPLOY_BINARY; got '$prelude'" ;;
    esac
    # Other operator settings must still reach the peer, so an empty prelude cannot pass.
    case "$prelude" in
        *"AGENTDESK_DEPLOY_FAST=1"*) : ;;
        *) fail_test "$label: prelude dropped a forwarded setting; got '$prelude'" ;;
    esac
}

export AGENTDESK_DEPLOY_FAST=1

# Path that exists only on the deploying host (the observed failure).
export AGENTDESK_DEPLOY_BINARY="$TMP_ROOT/missing-on-peer/target/release/agentdesk"
assert_peer_prelude "local-only artifact path"

# A path that exists locally proves nothing about the peer's file at that path.
mkdir -p "$TMP_ROOT/exists-locally"
printf '#!/bin/sh\n' >"$TMP_ROOT/exists-locally/agentdesk"
chmod +x "$TMP_ROOT/exists-locally/agentdesk"
export AGENTDESK_DEPLOY_BINARY="$TMP_ROOT/exists-locally/agentdesk"
assert_peer_prelude "locally present artifact path"

if [ "$failures" -ne 0 ]; then
    printf '%s\n' "test_cluster_deploy_peer_binary_5918: $failures assertion(s) failed" >&2
    exit 1
fi

printf '%s\n' "test_cluster_deploy_peer_binary_5918: all assertions passed"
