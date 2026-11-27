#!/usr/bin/env bash

_post_deploy_smoke_waits_for_peers() {
    [ "$DEPLOY_ALL_NODES" = 1 ] && [ "$DEPLOY_PEER_INVOCATION" != 1 ] &&
        [ -z "$SCHEMA_PEERS_FIRST_STATE" ]
}

_finish_cluster_stage_with_smoke() {
    local peer_failed=0
    _finish_cluster_stage "$@" || peer_failed=1
    # Smoke coverage and deploy status are reported independently.
    if _post_deploy_smoke_waits_for_peers; then
        _run_post_deploy_smoke_stage "$peer_failed" || true
    fi
    return "$peer_failed"
}
