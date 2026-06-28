use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// #1443 — Long-form guide pages.
//
// The endpoint catalogue answers "what does this endpoint do?", but the
// 2026-04-30 #1435 incident showed that callers also need a higher-level
// "which of these overlapping endpoints do I call right now?" decision tree.
// `guide_index` powers the `guides` field on the `/api/docs` root, and each
// guide has a dedicated handler returning the full body.
// ---------------------------------------------------------------------------

/// #1443 — header marker exposing the most recent main commit this guide was
/// authored against. Update alongside the prose any time the underlying
/// lifecycle endpoints change shape, per the
/// `docs/agent-maintenance/index.md` freshness convention (#1432).
pub(crate) const CARD_LIFECYCLE_OPS_LAST_REFRESHED: &str =
    "Last refreshed: 2026-04-30 against main @ f74cad35 (post #1442/#1444/#1446/#1448)";

/// #1549 — marker contract guide for structured API-friction reporting.
pub(crate) const API_FRICTION_MARKERS_LAST_REFRESHED: &str =
    "Last refreshed: 2026-05-03 against main @ 91bc116a (#1549)";

/// #1443 list of long-form guide pages exposed under `/api/docs/...`.
pub(super) fn guide_index() -> Vec<Value> {
    vec![
        json!({
            "name": "card-lifecycle-ops",
            "title": "Card Lifecycle Ops Guide",
            "path": "/api/docs/card-lifecycle-ops",
            "summary": "Decision tree + endpoint reference for /redispatch, /retry, /transition, /queue/generate, /dispatch-next. Read this BEFORE chaining card-lifecycle calls.",
        }),
        json!({
            "name": "api-friction-markers",
            "title": "API Friction Marker Guide",
            "path": "/api/docs/api-friction-markers",
            "summary": "Marker schema and collection path for API_FRICTION reports emitted when /api docs are missing or misleading.",
        }),
    ]
}

pub(super) fn card_lifecycle_ops_body() -> Value {
    json!({
        "title": "Card Lifecycle Ops Guide",
        "path": "/api/docs/card-lifecycle-ops",
        "last_refreshed": CARD_LIFECYCLE_OPS_LAST_REFRESHED,
        "purpose": "Single source of truth for choosing among /redispatch, /retry, /transition, /queue/generate, and /dispatch-next. The 2026-04-30 #1435 incident chained three of these and created duplicate dispatches. Read the decision tree FIRST, then the anti-pattern, then the endpoint table.",
        "sections": {
            "1_decision_tree": {
                "heading": "Section 1: Decision Tree",
                "intro": "Pick the row that matches the symptom. Each answer is a single call — do NOT chain.",
                "scenarios": [
                    {
                        "scenario": "Card stuck in review/dilemma_pending, want to restart",
                        "single_call": "POST /api/kanban-cards/{id}/redispatch",
                        "notes": "Cancels the live dispatch and creates a brand-new dispatch_id. Inspect new_dispatch_id, cancelled_dispatch_id, and next_action in the response. Do NOT follow with /transition or /queue/generate."
                    },
                    {
                        "scenario": "Card done, want to retry the same failed step",
                        "single_call": "POST /api/kanban-cards/{id}/retry",
                        "notes": "Re-executes the same failed step with the same intent (optional assignee swap via assignee_agent_id). Inspect new_dispatch_id, cancelled_dispatch_id, and next_action. Do NOT follow with /transition or /queue/generate."
                    },
                    {
                        "scenario": "Force card to a specific status",
                        "single_call": "POST /api/kanban-cards/{id}/transition with {\"status\": \"<target>\"}",
                        "notes": "If the card has an active dispatch and target=ready, the call returns 409 Conflict (#1444 guard). Pass {\"force\": true} (or legacy cancel_dispatches=true) to cancel + re-transition in one call. Inspect cancelled_dispatch_ids, created_dispatch_id, and next_action_hint."
                    },
                    {
                        "scenario": "Bulk push N issues into the auto-queue",
                        "single_call": "POST /api/queue/generate with {\"issue_numbers\": [...]}",
                        "notes": "Bulk only — never use to restart a single card that already has an active dispatch (it will silent-skip and surface skipped_due_to_active_dispatch). Inspect skipped_due_to_active_dispatch / skipped_due_to_dependency / skipped_due_to_filter."
                    },
                    {
                        "scenario": "Trigger the next dispatch from an existing run",
                        "single_call": "POST /api/queue/dispatch-next",
                        "notes": "Use only after /generate has produced pending entries. Returns dispatched[], count, active_groups, pending_groups."
                    }
                ]
            },
            "2_endpoint_reference_table": {
                "heading": "Section 2: Endpoint Reference Table",
                "columns": ["endpoint", "when_to_use", "single_call_complete", "common_pitfall"],
                "rows": [
                    {
                        "endpoint": "POST /api/kanban-cards/{id}/redispatch",
                        "when_to_use": "Card has a live (pending/dispatched) dispatch and you want to restart with a brand-new dispatch_id.",
                        "single_call_complete": "Y",
                        "common_pitfall": "Chaining /transition or /queue/generate after it creates duplicate dispatches (#1442 incident)."
                    },
                    {
                        "endpoint": "POST /api/kanban-cards/{id}/retry",
                        "when_to_use": "Card landed in a failed terminal state and you want to re-run the SAME step.",
                        "single_call_complete": "Y",
                        "common_pitfall": "Calling on a card with no failed dispatch returns 409. Do NOT chain /transition or /generate."
                    },
                    {
                        "endpoint": "POST /api/kanban-cards/{id}/transition",
                        "when_to_use": "Administrative move to a specific target status (the canonical /force-transition path).",
                        "single_call_complete": "Y",
                        "common_pitfall": "target=ready while a dispatch is live returns 409 unless force=true (#1444 guard). Without force, callers used to chain /redispatch + /transition + /generate — that is the exact #1435 anti-pattern."
                    },
                    {
                        "endpoint": "POST /api/queue/generate",
                        "when_to_use": "Bulk push of multiple issue numbers into a queue run.",
                        "single_call_complete": "Y for the bulk intent",
                        "common_pitfall": "Not a single-card restart tool. Cards that already have a live dispatch are silently skipped and reported in skipped_due_to_active_dispatch — do not retry by chaining /redispatch first."
                    },
                    {
                        "endpoint": "POST /api/queue/dispatch-next",
                        "when_to_use": "Move the next pending entry of an existing run to dispatched.",
                        "single_call_complete": "Y",
                        "common_pitfall": "No-op if there are no pending entries; check the dispatched[] length before assuming progress."
                    }
                ]
            },
            "3_anti_pattern": {
                "heading": "Section 3: Anti-pattern (today's #1435 incident)",
                "wrong_pattern": [
                    "POST /api/kanban-cards/{id}/redispatch              # creates dispatch A",
                    "POST /api/kanban-cards/{id}/transition status:ready # cancels A, creates dispatch B  <- WRONG: this cancel+create is implicit; caller did not realize a fresh dispatch was made",
                    "POST /api/queue/generate                       # adds the card to a queue run; a subsequent /dispatch-next (or activate=true) then creates dispatch C  <- WRONG: silent-skip exists for cards with an active dispatch but is easy to miss in the response"
                ],
                "why_it_broke": "Each of /redispatch, /transition status:ready, and the /queue/generate -> /dispatch-next chain is single-call complete for its intent. Chaining them produced multiple live dispatch rows for one card (dispatch A from /redispatch, dispatch B from /transition's force-transition cleanup, plus the queue-run path from /generate that the activate hook then turned into dispatch C). The runtime started executing the duplicates, causing the outage on 2026-04-30. Note: /generate by itself creates queue entries — dispatch rows are produced by /dispatch-next or the activate=true shortcut.",
                "how_it_is_prevented_now": [
                    "#1442 added new_dispatch_id and cancelled_dispatch_id(s) to /redispatch, /retry, and /transition responses, plus a per-endpoint follow-up signal: /redispatch and /retry return `next_action` (a fixed marker such as 'none_required' or 'assign_agent_then_call_redispatch'); /transition returns `next_action_hint` (a free-form sentence naming the exact follow-up). On the success path both are 'none_required' / point at no further action — if a caller sees that and still chains another mutation, it is a caller bug, not a missing signal.",
                    "#1444 added a 409 Conflict guard on /transition status:ready when an active dispatch exists. Callers must explicitly opt in via force=true (or legacy cancel_dispatches=true) to override.",
                    "#1444 also made /queue/generate surface structured skips (skipped_due_to_active_dispatch / skipped_due_to_dependency / skipped_due_to_filter) instead of silently dropping the entry, so even a misuse is observable from the response. Note: /dispatch-next does NOT return these arrays — it only reports `dispatched`, `count`, `active_groups`, and `pending_groups`."
                ],
                "right_pattern": "Pick ONE row from Section 1 and call it ONCE. Inspect new_dispatch_id, cancelled_dispatch_id(s), next_action / next_action_hint, and (for /generate) skipped_due_to_*. Do NOT call a second mutation unless next_action / next_action_hint says so."
            },
            "4_new_response_fields": {
                "heading": "Section 4: New Response Fields (from #1442 / #1444)",
                "fields": [
                    {
                        "field": "new_dispatch_id",
                        "source": "/redispatch, /retry",
                        "notes": "String. Confirms that a new dispatch row was inserted; absence means the call was a no-op. /transition uses a different name (`created_dispatch_id`) — see below."
                    },
                    {
                        "field": "created_dispatch_id",
                        "source": "/transition (force-transition path)",
                        "notes": "String or null. Populated when the force-transition cleanup created a fresh dispatch as part of the move. Distinct from /redispatch's and /retry's `new_dispatch_id` field."
                    },
                    {
                        "field": "cancelled_dispatch_id (singular)",
                        "source": "/redispatch, /retry",
                        "notes": "String or null. Only populated when the cancel helper actually transitioned a pending/dispatched row to cancelled."
                    },
                    {
                        "field": "cancelled_dispatch_ids (plural)",
                        "source": "/transition (force-transition path)",
                        "notes": "Array of dispatch IDs cancelled by the cleanup pass; pairs with cancelled_dispatches count."
                    },
                    {
                        "field": "next_action",
                        "source": "/redispatch, /retry",
                        "notes": "Concrete next-action string returned by the per-card endpoints. 'none_required' on the success path; otherwise a fixed marker such as 'assign_agent_then_call_retry', 'assign_agent_then_call_redispatch', or 'duplicate_active_dispatch_detected_inspect_card'. If it says 'none_required', do NOT chain another mutation."
                    },
                    {
                        "field": "next_action_hint",
                        "source": "/transition (force-transition path; also returned in the 409 body)",
                        "notes": "Free-form sentence naming the exact follow-up — for example 'call /api/queue/generate to dispatch newly-ready card', or guidance on the 409 override. Distinct from /redispatch and /retry's `next_action` field."
                    },
                    {
                        "field": "skipped_due_to_active_dispatch",
                        "source": "/queue/generate (NOT /dispatch-next)",
                        "notes": "Array of {issue_number, existing_dispatch_id} entries that were silently skipped because the card already had a live dispatch."
                    },
                    {
                        "field": "skipped_due_to_dependency",
                        "source": "/queue/generate (NOT /dispatch-next)",
                        "notes": "Array of {issue_number, unresolved_deps[]} entries skipped because dependency cards were not yet done."
                    },
                    {
                        "field": "skipped_due_to_filter",
                        "source": "/queue/generate (NOT /dispatch-next)",
                        "notes": "Array of entries skipped by repo/agent_id filters."
                    },
                    {
                        "field": "409 Conflict response",
                        "source": "/transition with status=ready and an active dispatch",
                        "notes": "Body shape: {error, active_dispatch_id, active_dispatch_ids, next_action_hint}. Override with {\"force\": true} (or legacy cancel_dispatches=true)."
                    }
                ]
            },
            "5_cross_references": {
                "heading": "Section 5: Cross-references",
                "links": [
                    {"label": "Issue #1442 — response schema (new_dispatch_id, next_action_hint)", "url": "https://github.com/itismyfield/AgentDesk/issues/1442"},
                    {"label": "Issue #1444 — 409 idempotency guard + structured silent-skip", "url": "https://github.com/itismyfield/AgentDesk/issues/1444"},
                    {"label": "Issue #1446 — stall watchdog and THREAD-GUARD stale cleanup", "url": "https://github.com/itismyfield/AgentDesk/issues/1446"},
                    {"label": "Issue #1448 — announce-bot turn-leak fix (issue-card template block-list)", "url": "https://github.com/itismyfield/AgentDesk/issues/1448"},
                    {"label": "docs/agent-maintenance/index.md — freshness convention", "path": "docs/agent-maintenance/index.md"},
                    {"label": "docs/source-of-truth.md — canonical edit paths index", "path": "docs/source-of-truth.md"}
                ]
            }
        }
    })
}

pub(super) fn api_friction_markers_body() -> Value {
    json!({
        "title": "API Friction Marker Guide",
        "path": "/api/docs/api-friction-markers",
        "last_refreshed": API_FRICTION_MARKERS_LAST_REFRESHED,
        "purpose": "Capture structured reports when an agent had to infer, trial-and-error, or bypass a missing/misleading /api docs contract. The marker is for docs/API friction only; it is not a replacement for normal task output.",
        "marker_prefix": "API_FRICTION:",
        "schema": {
            "required": {
                "endpoint": "HTTP endpoint or API surface, for example PATCH /api/dispatches/{id}",
                "friction_type": "Short category such as missing-docs, wrong-schema, or docs-bypass",
                "summary": "One-sentence friction summary"
            },
            "optional": {
                "workaround": "What the agent had to do instead",
                "suggested_fix": "Concrete docs/API improvement",
                "docs_category": "Fine-grained docs category such as dispatches or queue",
                "keywords": ["Extra grouping/search terms"]
            },
            "aliases": {
                "surface": "endpoint",
                "type": "friction_type",
                "frictionType": "friction_type",
                "workaround_method": "workaround",
                "suggestedFix": "suggested_fix",
                "docsCategory": "docs_category"
            }
        },
        "example": "API_FRICTION: {\"endpoint\":\"PATCH /api/dispatches/{id}\",\"friction_type\":\"missing-docs\",\"summary\":\"dispatch completion docs omitted PATCH semantics\",\"workaround\":\"read source and called PATCH manually\",\"suggested_fix\":\"document status/result response fields in /api/docs/dispatches\",\"docs_category\":\"dispatches\"}",
        "collection_flow": [
            "Turn bridge scans final and late turn output for lines beginning with API_FRICTION:.",
            "Valid JSON markers are stripped from the delivered response, normalized, fingerprinted by endpoint + friction_type, and inserted into api_friction_events.",
            "When Memento is configured, the same event is stored under topic api-friction with workspace-scoped context.",
            "The policy tick aggregates repeated fingerprints and creates one GitHub issue per unreported pattern."
        ],
        "operator_queries": [
            "SELECT endpoint, friction_type, COUNT(*) FROM api_friction_events GROUP BY 1,2 ORDER BY COUNT(*) DESC;",
            "SELECT fingerprint, issue_url, last_error FROM api_friction_issues ORDER BY updated_at DESC;"
        ],
        "constraints": [
            "Emit at most one marker per distinct docs/API gap in a turn.",
            "Do not include secrets, tokens, private prompt content, or full transcripts.",
            "Do not use DB direct writes as the workaround unless the task explicitly required DB repair; prefer canonical /api endpoints first."
        ],
        "source_of_truth": "docs/source-of-truth.md#api_friction-markers"
    })
}
