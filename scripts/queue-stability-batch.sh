#!/usr/bin/env bash
set -euo pipefail

# Auto-queue batch runner - variable phases with optional deploy gates.
# Usage: edit PHASES array below, then run manually or via launchd.
# Idempotent - skips if active/pending/paused run exists.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/_defaults.sh
. "$SCRIPT_DIR/_defaults.sh"

REL_PORT="${AGENTDESK_REL_PORT:-$ADK_DEFAULT_PORT}"
API="http://${ADK_DEFAULT_LOOPBACK}:${REL_PORT}"
REPO="${AQ_REPO:-itismyfield/AgentDesk}"

log() { echo "[$(date '+%H:%M:%S')] $*"; }

# -- Configuration ----------------------------------------------------------
# Each PHASES entry: "issue1 issue2 ..."
# Phase index = array position (0-based)
PHASES=(
    ""  # Phase 0:
    ""  # Phase 1:
    ""  # Phase 2:
    ""  # Phase 3:
)

AGENT_ID="project-agentdesk"

api_get() {
    curl -sf "$API$1"
}

api_post_json() {
    local path="$1"
    local body="$2"
    curl -sf "$API$path" -X POST -H "Content-Type: application/json" -d "$body"
}

api_patch_json() {
    local path="$1"
    local body="$2"
    curl -sf "$API$path" -X PATCH -H "Content-Type: application/json" -d "$body"
}

card_for_issue() {
    local issue="$1"
    api_get "/api/kanban-cards" \
        | jq -c --argjson issue "$issue" \
            'first(.cards[]? | select((.github_issue_number // empty | tonumber) == $issue)) // empty'
}

sync_repo_cards() {
    local owner="${REPO%%/*}"
    local repo="${REPO#*/}"
    api_post_json "/api/github/repos/$owner/$repo/sync" '{}' >/dev/null || true
}

ensure_card_ready_and_assigned() {
    local issue="$1"
    local card status card_id assigned

    card=$(card_for_issue "$issue" || true)
    if [ -z "$card" ]; then
        log "> #$issue: card not found - syncing"
        sync_repo_cards
        sleep 2
        card=$(card_for_issue "$issue" || true)
    fi
    if [ -z "$card" ]; then
        log "x #$issue: card not found after sync"
        exit 1
    fi

    card_id=$(printf '%s' "$card" | jq -r '.id')
    status=$(printf '%s' "$card" | jq -r '.status // ""')
    assigned=$(printf '%s' "$card" | jq -r '.assigned_agent_id // ""')

    if [ "$status" = "done" ]; then
        log "> #$issue: already done - will be skipped by preflight"
        return
    fi

    if [ "$status" = "backlog" ]; then
        api_patch_json "/api/kanban-cards/$card_id" '{"status":"ready"}' >/dev/null
        log "> #$issue: backlog -> ready"
    fi

    if [ -z "$assigned" ]; then
        api_patch_json "/api/kanban-cards/$card_id" \
            "$(jq -n --arg agent "$AGENT_ID" '{assignee_agent_id:$agent}')" >/dev/null
        log "> #$issue: assigned to $AGENT_ID"
    fi
}

build_entries_json() {
    local lines=()
    local idx issue

    for idx in "${!PHASES[@]}"; do
        for issue in ${PHASES[$idx]}; do
            lines+=("$issue:$idx")
        done
    done

    printf '%s\n' "${lines[@]}" \
        | jq -R -s '
            split("\n")[:-1]
            | map(select(length > 0) | split(":") | {
                issue_number: (.[0] | tonumber),
                batch_phase: (.[1] | tonumber)
            })'
}

# -- Pre-checks -------------------------------------------------------------
if ! api_get "/api/health" >/dev/null 2>&1; then
    log "x Release server not healthy - aborting"
    exit 1
fi

ACTIVE_RUN=$(api_get "/api/auto-queue/history?limit=20" \
    | jq -r 'first(.runs[]? | select(.status == "active" or .status == "pending" or .status == "paused") | "\(.id) \(.status)") // ""')
if [ -n "$ACTIVE_RUN" ]; then
    log "> Active run exists ($ACTIVE_RUN) - skipping"
    exit 0
fi

# -- Build issue list -------------------------------------------------------
ALL_ISSUES=()
for phase_issues in "${PHASES[@]}"; do
    for iss in $phase_issues; do
        ALL_ISSUES+=("$iss")
    done
done

if [ ${#ALL_ISSUES[@]} -eq 0 ]; then
    log "x No issues configured - edit PHASES array"
    exit 1
fi

# -- Ensure cards are ready -------------------------------------------------
for issue in "${ALL_ISSUES[@]}"; do
    ensure_card_ready_and_assigned "$issue"
done

# -- Generate run with phase metadata --------------------------------------
ENTRIES_JSON=$(build_entries_json)
GENERATE_BODY=$(jq -n \
    --arg repo "$REPO" \
    --argjson entries "$ENTRIES_JSON" \
    '{repo:$repo, entries:$entries}')

GENERATE_RESULT=$(api_post_json "/api/auto-queue/generate" "$GENERATE_BODY")
RUN_ID=$(printf '%s' "$GENERATE_RESULT" | jq -r '.run.id // ""')
if [ -z "$RUN_ID" ]; then
    log "x Generate failed"
    printf '%s\n' "$GENERATE_RESULT" | jq -c '{message, skipped_due_to_active_dispatch, skipped_due_to_dependency, skipped_due_to_filter}' 2>/dev/null || true
    exit 1
fi
log "> Generated run: $RUN_ID"

GENERATED_COUNT=$(printf '%s' "$GENERATE_RESULT" | jq -r '.entries | length')
if [ "$GENERATED_COUNT" -lt "${#ALL_ISSUES[@]}" ]; then
    log "> Generated $GENERATED_COUNT/${#ALL_ISSUES[@]} entries; skipped issues:"
    printf '%s\n' "$GENERATE_RESULT" \
        | jq -r '
            [
              (.skipped_due_to_active_dispatch[]? | "active_dispatch #" + (.issue_number|tostring)),
              (.skipped_due_to_dependency[]? | "dependency #" + (.issue_number|tostring) + " " + ((.unresolved_deps // [])|join(","))),
              (.skipped_due_to_filter[]? | "filter #" + (.issue_number|tostring) + " " + (.reason // ""))
            ] | .[]' \
        || true
fi

# -- Activate ---------------------------------------------------------------
ACTIVATE_BODY=$(jq -n --arg run_id "$RUN_ID" '{run_id:$run_id}')
ACTIVATE_RESULT=$(api_post_json "/api/auto-queue/dispatch-next" "$ACTIVATE_BODY")
DISPATCHED=$(printf '%s' "$ACTIVATE_RESULT" | jq -r '.dispatched | length')
log "OK Activated run $RUN_ID - $DISPATCHED dispatched"

# -- Verify -----------------------------------------------------------------
api_get "/api/auto-queue/status" \
    | jq -r --arg run "$RUN_ID" '
        if .run.id != $run then
          "warning: latest status is for run " + (.run.id // "none") + ", expected " + $run
        else
          (.entries
            | sort_by(.batch_phase, .github_issue_number)
            | .[]
            | [(.github_issue_number|tostring), (.batch_phase|tostring), .status]
            | @tsv)
        end'
