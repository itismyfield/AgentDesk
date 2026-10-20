use super::{
    VALID_DISPATCH_STATUSES, is_valid_dispatch_status, should_skip_auto_queue_terminal_sync,
};

#[test]
fn canonical_dispatch_status_set_rejects_typos() {
    for status in VALID_DISPATCH_STATUSES {
        assert!(is_valid_dispatch_status(status), "{status} should be valid");
    }
    for status in ["dispatchd", "complete", "canceled", "", " pending "] {
        assert!(
            !is_valid_dispatch_status(status),
            "{status:?} should be rejected"
        );
    }
}

#[test]
fn task_dispatches_status_check_migration_quarantines_unknown_statuses() {
    let sql = include_str!("../../../migrations/postgres/0067_task_dispatches_status_check.sql");
    assert!(
        sql.contains("ELSE 'failed'"),
        "dirty historical task_dispatches.status values must converge before CHECK install"
    );
    assert!(sql.contains("task_dispatches_status_known_check"));
}

#[test]
fn core_status_check_migration_normalizes_before_constraining() {
    let sql = include_str!("../../../migrations/postgres/0068_core_status_constraints.sql");
    let final_sql = include_str!("../../../migrations/postgres/0089_drop_agent_archive.sql");
    assert!(
        sql.contains("agents_status_known_check")
            && sql.contains("status IN ('idle', 'working', 'archived')"),
        "the immutable core migration must retain its original closed CHECK"
    );
    assert!(
        final_sql.contains("WHERE status = 'archived'")
            && final_sql.contains("status IN ('idle', 'working')"),
        "the archive removal migration must normalize archived agents before narrowing the CHECK"
    );
    assert!(
        sql.contains("WHEN lower(btrim(status)) IN ('active') THEN 'idle'"),
        "legacy onboarding agents.status='active' must normalize to canonical idle before CHECK install"
    );
    assert!(
        sql.contains("kanban_cards_status_slug_check")
            && sql.contains("regexp_replace(lower(btrim(status))")
            && sql.contains("qa_test"),
        "kanban_cards.status must normalize fixture/runtime states before slug CHECK install"
    );
}

#[test]
fn review_enabled_work_dispatches_hold_auto_queue_entry_until_card_terminal() {
    let result = serde_json::json!({"summary": "implemented"});

    assert!(should_skip_auto_queue_terminal_sync(
        Some("implementation"),
        "completed",
        Some(&result),
        true,
        false
    ));
    assert!(should_skip_auto_queue_terminal_sync(
        Some("rework"),
        "completed",
        Some(&result),
        true,
        false
    ));
    assert!(!should_skip_auto_queue_terminal_sync(
        Some("implementation"),
        "completed",
        Some(&result),
        true,
        true
    ));
}

#[test]
fn non_work_or_non_completed_dispatches_keep_existing_terminal_sync_behavior() {
    let result = serde_json::json!({"summary": "reviewed"});

    assert!(!should_skip_auto_queue_terminal_sync(
        Some("review"),
        "completed",
        Some(&result),
        true,
        false
    ));
    assert!(!should_skip_auto_queue_terminal_sync(
        Some("implementation"),
        "failed",
        Some(&result),
        true,
        false
    ));
    assert!(should_skip_auto_queue_terminal_sync(
        Some("consultation"),
        "completed",
        Some(&result),
        true,
        false
    ));
}

#[test]
fn scope_assessment_terminal_completion_never_finalizes_auto_queue_entry() {
    // #3605 (T2): a scope-assessment is an inert side-path. If an auto-queue
    // entry was bound to it (because it transiently became the card's
    // latest_dispatch_id), its completion must NOT finalize that entry as
    // `done` — that would close the card with no implementation dispatch.
    // Mirror of the consultation guard above; review_disabled must not matter.
    let result = serde_json::json!({"scope_depth": "full"});
    for review_disabled in [false, true] {
        assert!(
            should_skip_auto_queue_terminal_sync(
                Some("scope-assessment"),
                "completed",
                Some(&result),
                true,
                review_disabled,
            ),
            "scope-assessment completion must skip terminal sync (review_disabled={review_disabled})"
        );
    }
}

#[test]
fn plan_and_plan_review_completion_skips_terminal_sync_but_is_not_a_side_path() {
    // #3594 (T3): plan / plan-review are multi-stage WORK dispatches. Their
    // completion is consumed by the kanban-rules JS fan-out, which re-dispatches
    // the bound auto-queue entry to the next stage. So their completion must
    // SKIP the terminal sync (entry stays alive for the JS follow-up) — but they
    // are NOT side-paths (they kick the card to in_progress and must not trigger
    // a review-transition). review_disabled must not matter.
    let result = serde_json::json!({"plan": "design + steps"});
    for dispatch_type in ["plan", "plan-review"] {
        for review_disabled in [false, true] {
            assert!(
                should_skip_auto_queue_terminal_sync(
                    Some(dispatch_type),
                    "completed",
                    Some(&result),
                    true,
                    review_disabled,
                ),
                "{dispatch_type} completion must skip terminal sync (review_disabled={review_disabled})"
            );
        }
        // Sanity: plan/plan-review are NOT in the side-path set (no review-transition).
        assert!(
            !crate::dispatch::dispatch_is_side_path(Some(dispatch_type)),
            "{dispatch_type} must not be classified as a side-path"
        );
        // A non-completed (failed) plan must NOT skip — failure finalization
        // (sync to failed) still applies on the failure terminal status.
        assert!(
            !should_skip_auto_queue_terminal_sync(
                Some(dispatch_type),
                "failed",
                Some(&result),
                true,
                false,
            ),
            "{dispatch_type} failure must not be skipped"
        );
    }
}

#[test]
fn side_path_dispatch_types_are_all_skipped_on_completion() {
    // Guard the centralized SIDE_PATH_DISPATCH_TYPES set: every member must
    // skip the auto-queue terminal sync on completion so future additions to
    // the set automatically inherit the protection.
    let result = serde_json::json!({"summary": "side-path"});
    for dispatch_type in crate::dispatch::SIDE_PATH_DISPATCH_TYPES {
        assert!(
            should_skip_auto_queue_terminal_sync(
                Some(dispatch_type),
                "completed",
                Some(&result),
                true,
                false,
            ),
            "{dispatch_type} is a side-path and must skip terminal sync"
        );
    }
}
