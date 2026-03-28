# AgentDesk Feature Specification

Detailed behavior specification for the kanban pipeline, auto-queue, review automation, dispatch system, and policy engine.

## Kanban System

### Card State Flow

```
backlog → ready → requested → in_progress → review → done
                      ↓              ↓           ↓
                    failed        blocked    pending_decision
                                               ↓
                                         rework (→ in_progress)
```

| Status | Meaning | Next States |
|--------|---------|-------------|
| `backlog` | Waiting | ready (manual or auto-queue) |
| `ready` | Ready for dispatch | requested (auto-queue or manual) |
| `requested` | Work requested from agent | in_progress (agent accepts) / failed (45min timeout) |
| `in_progress` | Agent working | review (work complete) / blocked (2hr inactivity) |
| `review` | Awaiting review | done (pass) / pending_decision (fail) / in_progress (rework) |
| `pending_decision` | PM decision needed | done / in_progress (manual decision) |
| `done` | Completed | — |
| `failed` | Failed | ready (retry) |
| `blocked` | Stalled | in_progress (manual unblock) |

### PM Decision Gate

Runs automatically on dispatch completion via `policies/kanban-rules.js` `onDispatchCompleted`.

**Checks:**
1. **DoD completion** — If `deferred_dod_json` has a checklist, verifies all items are checked
2. **Minimum work time** — Agent must have worked at least 2 minutes (120s)

**On pass:** → `review` (if DoD exists) or `done` (no DoD)
**On fail:** → `pending_decision` + notification to PMD channel

### XP Rewards

Agents earn XP on card completion:

| Priority | XP | Chain Bonus |
|----------|-----|------------|
| low | 5 | +2 per depth (max +6) |
| medium | 10 | +2 per depth |
| high | 18 | +2 per depth |
| urgent | 30 | +2 per depth |

---

## Auto-Queue System

`policies/auto-queue.js` — runs via tiered tick hooks (primarily `onTick30s` for fast dispatch, `onTick5min` for reconciliation).

### Queue Storage

Two tables manage the auto-queue:

- **`auto_queue_runs`** — Tracks queue generation runs (id, repo, agent_id, status, unified_thread settings, timestamps)
- **`auto_queue_entries`** — Individual queue entries per run (id, run_id, kanban_card_id, agent_id, priority_rank, status, dispatch_id, timestamps)

### Two-Phase Operation

**Phase 1: Queue Generation (auto-generate)**
- Scans `backlog` or `ready` cards not yet in queue
- Agent selection:
  - Card has `assigned_agent_id` → use that agent
  - Otherwise → use `github_repos.default_agent_id`
  - Neither → skip
- Priority scoring: critical=100, high=75, medium=50, low=25
- Inserts into `auto_queue_entries` with `status='pending'`

**Phase 2: Queue Activation (auto-activate)**
- Processes `pending` queue entries by priority order
- **One activation per agent per tick** (prevents overload)
- Checks agent is idle (no requested/in_progress cards)
- Card → `requested` + dispatch created + Discord notification
- Queue entry → `status='dispatched'`

### API

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/auto-queue/generate` | POST | Manual generation trigger |
| `/api/auto-queue/activate` | POST | Manual activation trigger |
| `/api/auto-queue/status` | GET | Current queue status |
| `/api/auto-queue/enqueue` | POST | Manual card enqueue |
| `/api/auto-queue/entries/{id}/skip` | PATCH | Skip queue entry |
| `/api/auto-queue/runs/{id}` | PATCH | Update run |
| `/api/auto-queue/reorder` | PATCH | Reorder queue |
| `/api/auto-queue/reset` | POST | Reset queue |
| `/api/auto-queue/pause` | POST | Pause queue |
| `/api/auto-queue/resume` | POST | Resume queue |
| `/api/auto-queue/cancel` | POST | Cancel queue |

---

## Review Automation

`policies/review-automation.js` — activates when a card enters `review` status.

### Review Entry (onReviewEnter)

1. If reviews disabled (`review_enabled = false`) → skip to `done`
2. Increment `review_round`
3. If round limit exceeded (default 3) → `pending_decision` + PM notification
4. Create counter-model review dispatch:
   - `review` type dispatch to same agent's alternate channel
   - Message: `[Counter Review R{round}] {title}`

### Verdict Processing

**Pass (pass/accept/approved):**
- If pipeline has next stage → move to that stage
- Otherwise → `done` + set `completed_at`

**Improve (improve/reject/rework):**
- Save review feedback to `review_notes`
- Create `rework` dispatch → send back to original agent
- Card → `in_progress` + `review_status = 'rework_pending'`

---

## Timeout Detection

`policies/timeouts.js` — runs via tiered tick hooks (`onTick1min` for non-critical timeouts, `onTick30s` for recovery-critical checks).

| Target | Condition | Result |
|--------|-----------|--------|
| requested card | 45min with no acceptance | → `failed` + dispatch failed |
| in_progress card | 2hr inactivity | → `blocked` + reason logged |
| review card | 30min after dispatch with no verdict | → `pending_decision` |
| awaiting_dod card | 15min exceeded | → `pending_decision` |
| suggestion_pending card | 15min exceeded | Auto-accept → rework_pending |
| queue entry | 100min exceeded | Deleted |
| pending/dispatched dispatch | 24hr exceeded | → `failed` |

---

## Pipeline

`policies/pipeline.js` — multi-stage workflows for cards.

### How It Works

`pipeline_stages` table defines per-repo stages:
```
stage_order=1: "Implementation"   trigger_after="ready"
stage_order=2: "Testing"          trigger_after="review_pass"
stage_order=3: "Deployment"       trigger_after="review_pass"
```

- Card → `ready`: first stage (`trigger_after='ready'`) assigned
- After review pass: advance to next `stage_order`
- Final stage complete: clear `pipeline_stage_id`, card → `done`

---

## Auto-Triage

`policies/triage-rules.js` — label-based card classification.

On `onTick`, scans `backlog` cards without `assigned_agent_id`:

- `agent:xxx` label → auto-assign to that agent
- `priority:urgent` / `critical` label → urgent priority
- `priority:high` label → high priority
- `priority:low` label → low priority

---

## Dispatch System

`src/dispatch/mod.rs` — core mechanism for sending work to agents.

### Dispatch Types

| Type | Purpose |
|------|---------|
| `implementation` | Main work (coding, implementation) |
| `review` | Counter-model review |
| `review-decision` | PM decision request |
| `rework` | Post-review rework |

### Creation Flow

1. Activated from `auto_queue_entries` or manual API call
2. Record created in `task_dispatches` (status=`pending`)
3. Card's `latest_dispatch_id` updated
4. Discord notification sent (announce bot → agent channel)
5. `OnCardTransition` hook fired

### Completion Flow

1. Agent session ends → `OnSessionStatusChange` hook
2. Dispatch status → `completed` + result_summary saved
3. `OnDispatchCompleted` hook → PM gate, review automation, XP rewards

---

## GitHub Integration

`src/github/` — uses `gh` CLI for GitHub operations.

### Issue Sync (sync.rs)
- `gh issue list --json` to fetch open issues
- Match to kanban cards by `github_issue_number`
- Auto-transition cards when issues are closed on GitHub

### DoD Mirroring (dod.rs)
- Sync card's `deferred_dod_json` checklist to GitHub issue comments

### Auto-Triage (triage.rs)
- New issues → auto-create kanban cards
- Label-based agent/priority assignment

---

## Meeting System

`src/services/discord/meeting.rs` — round-table meeting orchestration.

### Meeting Flow

1. `/meeting start <agenda> [provider]` to begin
2. Participant selection (auto or manual per settings)
3. Round progression:
   - Primary provider works one round
   - Reviewer provider reviews + provides feedback
   - Repeat (up to max_rounds)
4. Summary agent generates final summary
5. Transcript + summary saved to DB

### Meeting States
`SelectingParticipants → InProgress → Concluding → Completed`

---

## Policy Engine

`src/engine/` — QuickJS-based JavaScript execution environment.

### Hot-Reload

With `policies.hot_reload: true` in `agentdesk.yaml`:
- File watcher on `policies/` directory (notify crate)
- Changed `.js` files auto-reloaded without server restart

### Writing Policies

Each policy file exports a default object with hook handlers:

```javascript
export default {
  priority: 50,  // Lower runs first

  // 3-tier tick hooks (#127) — register specific tiers for precise scheduling
  onTick30s(ctx) {
    // Fast: retry, notification recovery, deadlock detection (every 30s)
  },

  onTick1min(ctx) {
    // Normal: non-critical timeouts (every 1 min)
  },

  onTick5min(ctx) {
    // Slow: reconciliation, context check (every 5 min)
  },

  onTick(ctx) {
    // Legacy: fires at 5min interval for backward compat
    const cards = agentdesk.db.query("SELECT * FROM kanban_cards WHERE status = ?", ["backlog"]);
    // ... process cards
  },

  onCardTransition(ctx) {
    const { card_id, from_status, to_status } = ctx;
    // ... handle transition
  }
};
```

---

## Discord Bot Roles

| Bot | Purpose |
|-----|---------|
| Command (Claude/Codex) | Runs AI agent sessions, receives slash commands |
| Announce | Sends work start/review notifications, `!` text command trigger |
| Notify | PM decision needed, failure/blocked alerts |

### Message Processing Flow

```
User message
  → router.rs: intake_message()
    → dedup check (dispatch_id or hash-based)
    → bot message filtering (allowed_bot_ids check)
    → mention filtering (only process self-mentions)
    → provider routing (-cc → Claude, -cdx → Codex)
  → turn_bridge.rs: dispatch_turn()
    → cancel_token created
    → "..." placeholder message in Discord
    → claude.rs: execute_streaming — provider CLI spawn
    → placeholder replaced with final response
```
