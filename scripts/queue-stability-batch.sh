#!/usr/bin/env bash
set -euo pipefail

# Auto-queue batch runner — variable phases with optional deploy gates.
# Usage: edit PHASES array below, then run manually or via launchd.
# Idempotent — skips if active run exists.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "$SCRIPT_DIR/_defaults.sh"

REL_PORT="${AGENTDESK_REL_PORT:-$ADK_DEFAULT_PORT}"
API="http://${ADK_DEFAULT_LOOPBACK}:${REL_PORT}"
DB="$HOME/.adk/release/data/agentdesk.sqlite"

log() { echo "[$(date '+%H:%M:%S')] $*"; }

# ── Configuration ──────────────────────────────────────────
# Each PHASES entry: "issue1 issue2 ..."
# Phase index = array position (0-based)
PHASES=(
    ""  # Phase 0:
    ""  # Phase 1:
    ""  # Phase 2:
    ""  # Phase 3:
)

AGENT_ID="project-agentdesk"
RATIONALE="Batch auto-queue run"

# ── Pre-checks ─────────────────────────────────────────────
if ! curl -sf "$API/api/health" >/dev/null 2>&1; then
    log "✗ Release server not healthy — aborting"
    exit 1
fi

ACTIVE_RUN=$(/usr/bin/sqlite3 -readonly "$DB" \
    "SELECT id FROM auto_queue_runs WHERE status IN ('active','pending','paused') LIMIT 1;" 2>/dev/null || true)
if [ -n "$ACTIVE_RUN" ]; then
    log "▸ Active run exists ($ACTIVE_RUN) — skipping"
    exit 0
fi

# ── Build issue list ───────────────────────────────────────
ALL_ISSUES=()
for phase_issues in "${PHASES[@]}"; do
    for iss in $phase_issues; do
        ALL_ISSUES+=("$iss")
    done
done

if [ ${#ALL_ISSUES[@]} -eq 0 ]; then
    log "✗ No issues configured — edit PHASES array"
    exit 1
fi

# ── Ensure cards are ready ─────────────────────────────────
for issue in "${ALL_ISSUES[@]}"; do
    STATUS=$(/usr/bin/sqlite3 -readonly "$DB" \
        "SELECT status FROM kanban_cards WHERE github_issue_number = $issue;" 2>/dev/null || true)
    if [ -z "$STATUS" ]; then
        log "▸ #$issue: card not found — syncing"
        curl -sf "$API/api/github/repos/itismyfield/AgentDesk/sync" -X POST >/dev/null 2>&1 || true
        sleep 2
        STATUS=$(/usr/bin/sqlite3 -readonly "$DB" \
            "SELECT status FROM kanban_cards WHERE github_issue_number = $issue;" 2>/dev/null || true)
    fi
    if [ "$STATUS" = "done" ]; then
        log "▸ #$issue: already done — will be skipped by preflight"
        continue
    fi
    if [ "$STATUS" = "backlog" ]; then
        CARD_ID=$(/usr/bin/sqlite3 -readonly "$DB" \
            "SELECT id FROM kanban_cards WHERE github_issue_number = $issue;")
        curl -sf "$API/api/kanban-cards/$CARD_ID" -X PATCH \
            -H "Content-Type: application/json" -d '{"status":"ready"}' >/dev/null 2>&1 || true
        log "▸ #$issue: backlog → ready"
    fi
    /usr/bin/sqlite3 "$DB" \
        "UPDATE kanban_cards SET assigned_agent_id = '$AGENT_ID' WHERE github_issue_number = $issue AND (assigned_agent_id IS NULL OR assigned_agent_id = '');" 2>/dev/null || true
done

# ── Generate run ───────────────────────────────────────────
ISSUE_JSON=$(printf '%s\n' "${ALL_ISSUES[@]}" | jq -s '.')
GENERATE_RESULT=$(curl -sf "$API/api/auto-queue/generate" -X POST \
    -H "Content-Type: application/json" \
    -d "{\"issue_numbers\": $ISSUE_JSON}" 2>/dev/null)

RUN_ID=$(echo "$GENERATE_RESULT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('run',{}).get('id',''))" 2>/dev/null || true)
if [ -z "$RUN_ID" ]; then
    log "✗ Generate failed"
    exit 1
fi
log "▸ Generated run: $RUN_ID"

# ── Set phases ─────────────────────────────────────────────
set_phase() {
    local phase=$1; shift
    for issue in "$@"; do
        /usr/bin/sqlite3 "$DB" "
            UPDATE auto_queue_entries SET batch_phase = $phase
            WHERE run_id = '$RUN_ID'
            AND kanban_card_id = (SELECT id FROM kanban_cards WHERE github_issue_number = $issue);
        " 2>/dev/null || true
    done
}

for idx in "${!PHASES[@]}"; do
    issues_str="${PHASES[$idx]}"
    if [ -n "$issues_str" ]; then
        # shellcheck disable=SC2086
        set_phase "$idx" $issues_str
    fi
done

# ── Add missing entries ────────────────────────────────────
for idx in "${!PHASES[@]}"; do
    for issue in ${PHASES[$idx]}; do
        EXISTS=$(/usr/bin/sqlite3 -readonly "$DB" "
            SELECT COUNT(*) FROM auto_queue_entries e
            JOIN kanban_cards k ON k.id = e.kanban_card_id
            WHERE e.run_id = '$RUN_ID' AND k.github_issue_number = $issue;
        " 2>/dev/null || echo "0")
        if [ "$EXISTS" = "0" ]; then
            CARD_ID=$(/usr/bin/sqlite3 -readonly "$DB" \
                "SELECT id FROM kanban_cards WHERE github_issue_number = $issue;" 2>/dev/null || true)
            if [ -n "$CARD_ID" ]; then
                /usr/bin/sqlite3 "$DB" "
                    INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank, status, batch_phase)
                    VALUES (lower(hex(randomblob(16))), '$RUN_ID', '$CARD_ID', '$AGENT_ID', 0, 'pending', $idx);
                " 2>/dev/null || true
                log "▸ #$issue: added missing entry (phase $idx)"
            fi
        fi
    done
done

# ── Submit order + activate ────────────────────────────────
/usr/bin/sqlite3 "$DB" "UPDATE auto_queue_runs SET status = 'pending' WHERE id = '$RUN_ID';"

ORDER_JSON=$(printf '%s\n' "${ALL_ISSUES[@]}" | jq -s '.')
curl -sf "$API/api/auto-queue/runs/$RUN_ID/order" -X POST \
    -H "Content-Type: application/json" \
    -d "{\"order\": $ORDER_JSON, \"rationale\": \"$RATIONALE\"}" >/dev/null 2>&1

ACTIVATE_RESULT=$(curl -sf "$API/api/auto-queue/dispatch-next" -X POST \
    -H "Content-Type: application/json" \
    -d "{\"run_id\": \"$RUN_ID\"}" 2>/dev/null)

DISPATCHED=$(echo "$ACTIVATE_RESULT" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('dispatched',[])))" 2>/dev/null || echo "?")
log "✓ Activated run $RUN_ID — $DISPATCHED dispatched"

# ── Verify ─────────────────────────────────────────────────
/usr/bin/sqlite3 -readonly "$DB" "
    SELECT k.github_issue_number, e.batch_phase, e.status
    FROM auto_queue_entries e
    JOIN kanban_cards k ON k.id = e.kanban_card_id
    WHERE e.run_id = '$RUN_ID'
    ORDER BY e.batch_phase, k.github_issue_number;
"
