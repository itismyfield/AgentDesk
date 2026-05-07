use super::*;
use crate::dispatch::create_dispatch;
use crate::dispatch::test_support::*;
use serde_json::json;

#[test]
fn complete_dispatch_updates_status() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-2", "ready");

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-2",
        "agent-1",
        "implementation",
        "title",
        &json!({}),
    )
    .unwrap();
    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();
    seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented");

    let completed =
        complete_dispatch(&db, &engine, &dispatch_id, &json!({"output": "done"})).unwrap();

    assert_eq!(completed["status"], "completed");
}

#[test]
fn complete_dispatch_records_completed_at() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-2-ts", "ready");

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-2-ts",
        "agent-1",
        "implementation",
        "title",
        &json!({}),
    )
    .unwrap();
    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();
    seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented");

    let completed =
        complete_dispatch(&db, &engine, &dispatch_id, &json!({"output": "done"})).unwrap();

    assert!(
        completed["completed_at"].as_str().is_some(),
        "completion result must expose completed_at"
    );

    let conn = db.separate_conn().unwrap();
    let stored_completed_at: Option<String> = conn
        .query_row(
            "SELECT completed_at FROM task_dispatches WHERE id = ?1",
            [&dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        stored_completed_at.is_some(),
        "task_dispatches.completed_at must be stored for completed rows"
    );
}

#[test]
fn complete_dispatch_nonexistent_fails() {
    let db = test_db();
    let engine = test_engine(&db);

    let result = complete_dispatch(&db, &engine, "nonexistent", &json!({}));
    assert!(result.is_err());
}

#[test]
fn complete_dispatch_rejects_work_without_execution_evidence() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-no-evidence", "ready");

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-no-evidence",
        "agent-1",
        "implementation",
        "title",
        &json!({}),
    )
    .unwrap();
    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

    let result = complete_dispatch(
        &db,
        &engine,
        &dispatch_id,
        &json!({"completion_source": "test_harness"}),
    );
    let err = format!("{}", result.unwrap_err());
    assert!(err.contains("no agent execution evidence"));
    assert_eq!(dispatch["status"], "pending");

    let conn = db.separate_conn().unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [&dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "pending");
}

#[test]
fn complete_dispatch_skips_cancelled() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-cancel", "review");

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-cancel",
        "agent-1",
        "review-decision",
        "Decision",
        &json!({}),
    )
    .unwrap();
    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

    // Simulate dismiss: cancel the dispatch
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "UPDATE task_dispatches SET status = 'cancelled' WHERE id = ?1",
            [&dispatch_id],
        )
        .unwrap();
    }

    // Delayed completion attempt should NOT re-complete the cancelled dispatch
    let result = complete_dispatch(&db, &engine, &dispatch_id, &json!({"verdict": "pass"}));
    // Should return Ok (dispatch found) but status should remain cancelled
    assert!(result.is_ok());
    let returned = result.unwrap();
    assert_eq!(
        returned["status"], "cancelled",
        "cancelled dispatch must not be re-completed"
    );
}

#[test]
fn terminal_dispatch_status_clears_linked_session_active_dispatch() {
    let db = test_db();
    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at) \
         VALUES ('card-terminal-session', 'Terminal Session Card', 'in_progress', 'agent-1', datetime('now'), datetime('now'))",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
         VALUES ('dispatch-terminal-session', 'card-terminal-session', 'agent-1', 'implementation', 'dispatched', 'Terminal Session', datetime('now'), datetime('now'))",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (session_key, agent_id, provider, status, active_dispatch_id, session_info, created_at) \
         VALUES ('session-terminal-session', 'agent-1', 'codex', 'turn_active', 'dispatch-terminal-session', 'live dispatch', datetime('now'))",
        [],
    )
    .unwrap();

    let changed = set_dispatch_status_on_conn(
        &conn,
        "dispatch-terminal-session",
        "completed",
        Some(&serde_json::json!({"completed_commit": "abc123"})),
        "api",
        Some(&["dispatched"]),
        true,
    )
    .unwrap();
    assert_eq!(changed, 1);

    let (session_status, active_dispatch_id, session_info): (
        String,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, active_dispatch_id, session_info FROM sessions WHERE session_key = 'session-terminal-session'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(session_status, "idle");
    assert!(active_dispatch_id.is_none());
    assert_eq!(session_info.as_deref(), Some("Dispatch completed"));
}

#[test]
fn ensure_dispatch_notify_outbox_skips_completed_dispatch() {
    let db = test_db();
    seed_card(&db, "card-completed-notify", "done");
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at, completed_at)
             VALUES ('dispatch-completed-notify', 'card-completed-notify', 'agent-1', 'review', 'completed', 'Completed review', datetime('now'), datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    let conn = db.separate_conn().unwrap();
    let inserted = ensure_dispatch_notify_outbox_on_conn(
        &conn,
        "dispatch-completed-notify",
        "agent-1",
        "card-completed-notify",
        "Completed review",
    )
    .unwrap();

    assert!(
        !inserted,
        "completed dispatches must not enqueue new notify outbox rows"
    );
    assert_eq!(
        count_notify_outbox(&conn, "dispatch-completed-notify"),
        0,
        "completed dispatches must not retain notify outbox rows"
    );
}

#[test]
fn finalize_dispatch_sets_completion_source() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-fin", "ready");

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-fin",
        "agent-1",
        "implementation",
        "Finalize test",
        &json!({}),
    )
    .unwrap();
    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();
    seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented");

    let completed =
        finalize_dispatch(&db, &engine, &dispatch_id, "turn_bridge_explicit", None).unwrap();

    assert_eq!(completed["status"], "completed");
    // result is parsed JSON (query_dispatch_row parses it)
    assert_eq!(
        completed["result"]["completion_source"],
        "turn_bridge_explicit"
    );
}

#[test]
fn finalize_dispatch_merges_context() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-ctx", "ready");

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-ctx",
        "agent-1",
        "implementation",
        "Context test",
        &json!({}),
    )
    .unwrap();
    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

    let completed = finalize_dispatch(
        &db,
        &engine,
        &dispatch_id,
        "session_idle",
        Some(&json!({ "auto_completed": true, "agent_response_present": true })),
    )
    .unwrap();

    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["result"]["completion_source"], "session_idle");
    assert_eq!(completed["result"]["auto_completed"], true);
}

// #699 — phase-gate completion with all checks passing but no explicit
// `verdict` must inject `verdict = context.phase_gate.pass_verdict` into
// the persisted result so auto-queue does not pause the run.

#[test]
fn finalize_phase_gate_injects_verdict_when_all_checks_pass() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-pg-pass", "in_progress");

    let context = json!({
        "auto_queue": true,
        "sidecar_dispatch": true,
        "phase_gate": {
            "run_id": "run-699",
            "batch_phase": 1,
            "next_phase": 2,
            "final_phase": false,
            "pass_verdict": "phase_gate_passed",
            "checks": ["merge_verified", "issue_closed", "build_passed"],
        }
    });
    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-pg-pass",
        "agent-1",
        "phase-gate",
        "Phase gate test",
        &context,
    )
    .unwrap();
    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

    // Simulate a caller that produced all-pass checks + summary but
    // omitted the explicit verdict field entirely.
    let result = json!({
        "summary": "Phase gate passed",
        "checks": {
            "merge_verified": { "status": "pass" },
            "issue_closed": { "status": "pass" },
            "build_passed": { "status": "pass" },
        }
    });
    let completed = finalize_dispatch(&db, &engine, &dispatch_id, "api", Some(&result)).unwrap();

    assert_eq!(completed["status"], "completed");
    assert_eq!(
        completed["result"]["verdict"], "phase_gate_passed",
        "server must inject phase_gate_passed when verdict absent and checks all pass",
    );
    assert_eq!(completed["result"]["verdict_inferred"], true);
}

// #699 — never infer pass when any check fails. The verdict must remain
// absent so auto-queue can classify the gate as failed.

#[test]
fn finalize_phase_gate_preserves_absent_verdict_when_check_fails() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-pg-fail", "in_progress");

    let context = json!({
        "auto_queue": true,
        "sidecar_dispatch": true,
        "phase_gate": {
            "run_id": "run-699b",
            "batch_phase": 1,
            "pass_verdict": "phase_gate_passed",
            "checks": ["merge_verified", "issue_closed"],
        }
    });
    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-pg-fail",
        "agent-1",
        "phase-gate",
        "Phase gate test (fail)",
        &context,
    )
    .unwrap();
    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

    let result = json!({
        "checks": {
            "merge_verified": { "status": "pass" },
            "issue_closed": { "status": "fail" },
        }
    });
    let completed = finalize_dispatch(&db, &engine, &dispatch_id, "api", Some(&result)).unwrap();

    assert_eq!(completed["status"], "completed");
    assert!(
        completed["result"].get("verdict").is_none() || completed["result"]["verdict"].is_null(),
        "verdict must not be inferred when any check is fail"
    );
    assert!(
        completed["result"].get("verdict_inferred").is_none()
            || completed["result"]["verdict_inferred"].is_null(),
        "verdict_inferred flag must not be set on failed checks"
    );
}

// #699 (round 2) — non-default phase-gate dispatch types (e.g. "qa-gate")
// must also receive verdict injection. Detection goes by
// context.phase_gate presence, not dispatch_type string.

#[test]
fn finalize_phase_gate_injects_verdict_for_custom_dispatch_type() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-pg-qa", "in_progress");

    let context = json!({
        "auto_queue": true,
        "sidecar_dispatch": true,
        "phase_gate": {
            "run_id": "run-699qa",
            "batch_phase": 1,
            "pass_verdict": "qa_passed",
            "dispatch_type": "qa-gate",
            "checks": ["merge_verified", "qa_passed"],
        }
    });
    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-pg-qa",
        "agent-1",
        "qa-gate", // non-default dispatch type
        "QA gate test",
        &context,
    )
    .unwrap();
    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

    let result = json!({
        "summary": "QA gate passed",
        "checks": {
            "merge_verified": { "status": "pass" },
            "qa_passed": { "status": "pass" },
        }
    });
    let completed = finalize_dispatch(&db, &engine, &dispatch_id, "api", Some(&result)).unwrap();

    assert_eq!(
        completed["result"]["verdict"], "qa_passed",
        "server must inject the configured pass_verdict (qa_passed) for non-default dispatch type",
    );
    assert_eq!(completed["result"]["verdict_inferred"], true);
}

// #699 (round 2) — when `result.checks` is missing a required check key
// declared in `context.phase_gate.checks`, verdict MUST NOT be inferred.
// A partial payload cannot advance the gate.

#[test]
fn finalize_phase_gate_rejects_inference_when_required_check_absent() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-pg-partial", "in_progress");

    let context = json!({
        "auto_queue": true,
        "sidecar_dispatch": true,
        "phase_gate": {
            "run_id": "run-699partial",
            "batch_phase": 1,
            "pass_verdict": "phase_gate_passed",
            "checks": ["merge_verified", "issue_closed", "build_passed"],
        }
    });
    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-pg-partial",
        "agent-1",
        "phase-gate",
        "Partial checks test",
        &context,
    )
    .unwrap();
    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

    // Only 2 of 3 declared checks are reported; the third is silently missing.
    let result = json!({
        "checks": {
            "merge_verified": { "status": "pass" },
            "issue_closed": { "status": "pass" },
        }
    });
    let completed = finalize_dispatch(&db, &engine, &dispatch_id, "api", Some(&result)).unwrap();

    assert!(
        completed["result"].get("verdict").is_none() || completed["result"]["verdict"].is_null(),
        "verdict must not be inferred when a declared required check key is absent",
    );
}

// #699 — explicit verdict="fail" must survive verbatim even when every
// check status happens to be "pass" in the same payload.

#[test]
fn finalize_phase_gate_preserves_explicit_verdict_fail() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-pg-explicit", "in_progress");

    let context = json!({
        "auto_queue": true,
        "sidecar_dispatch": true,
        "phase_gate": {
            "run_id": "run-699c",
            "batch_phase": 1,
            "pass_verdict": "phase_gate_passed",
        }
    });
    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-pg-explicit",
        "agent-1",
        "phase-gate",
        "Phase gate test (explicit fail)",
        &context,
    )
    .unwrap();
    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

    let result = json!({
        "verdict": "fail",
        "summary": "Operator-forced fail",
        "checks": {
            "merge_verified": { "status": "pass" },
        }
    });
    let completed = finalize_dispatch(&db, &engine, &dispatch_id, "api", Some(&result)).unwrap();

    assert_eq!(completed["result"]["verdict"], "fail");
    assert!(
        completed["result"].get("verdict_inferred").is_none()
            || completed["result"]["verdict_inferred"].is_null(),
        "explicit verdict must not be flagged as inferred"
    );
}

#[test]
fn dispatch_events_capture_dispatched_and_completed_transitions() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-events", "ready");

    let dispatch = create_dispatch(
        &db,
        &engine,
        "card-events",
        "agent-1",
        "implementation",
        "Event trail",
        &json!({}),
    )
    .unwrap();
    let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

    {
        let conn = db.separate_conn().unwrap();
        set_dispatch_status_on_conn(
            &conn,
            &dispatch_id,
            "dispatched",
            None,
            "test_dispatch_outbox",
            Some(&["pending"]),
            false,
        )
        .unwrap();
    }
    seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented");

    finalize_dispatch(&db, &engine, &dispatch_id, "test_complete", None).unwrap();

    let conn = db.separate_conn().unwrap();
    assert_eq!(
        load_dispatch_events(&conn, &dispatch_id),
        vec![
            (None, "pending".to_string(), "create_dispatch".to_string()),
            (
                Some("pending".to_string()),
                "dispatched".to_string(),
                "test_dispatch_outbox".to_string()
            ),
            (
                Some("dispatched".to_string()),
                "completed".to_string(),
                "test_complete".to_string()
            ),
        ],
        "dispatch event log must preserve ordered status transitions"
    );
}

/// #750: narrowed enqueue policy.
/// - pending → dispatched: no enqueue (command bot's ⏳ is the source).
/// - dispatched → completed from live command-bot paths
///   (`transition_source` starts with "turn_bridge" or "watcher"): no
///   enqueue. Command bot already added ✅ on response delivery.
/// - dispatched → completed from non-live paths (api, recovery,
///   supervisor, test_*): enqueue. Announce bot's ✅ is the only
///   terminal success signal on the original message.
/// - any → failed / cancelled: enqueue. Announce bot must clean
///   command bot's stale ✅ and add ❌ to avoid false green checks.

#[test]
fn dispatch_status_transitions_enqueue_narrowed_on_non_live_paths() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card(&db, "card-outbox-turn-bridge", "ready");

    let live = create_dispatch(
        &db,
        &engine,
        "card-outbox-turn-bridge",
        "agent-1",
        "implementation",
        "Live trail",
        &json!({}),
    )
    .unwrap();
    let live_id = live["id"].as_str().unwrap().to_string();

    let conn = db.separate_conn().unwrap();
    set_dispatch_status_on_conn(
        &conn,
        &live_id,
        "dispatched",
        None,
        "turn_bridge_notify",
        Some(&["pending"]),
        false,
    )
    .unwrap();
    assert_eq!(
        count_status_reaction_outbox(&conn, &live_id),
        0,
        "#750: pending→dispatched must never enqueue (command bot owns ⏳)"
    );

    set_dispatch_status_on_conn(
        &conn,
        &live_id,
        "completed",
        Some(&json!({"completion_source":"turn_bridge_explicit"})),
        "turn_bridge_explicit",
        Some(&["dispatched"]),
        true,
    )
    .unwrap();
    assert_eq!(
        count_status_reaction_outbox(&conn, &live_id),
        0,
        "#750: completed via turn_bridge must not enqueue (command bot already added ✅)"
    );

    seed_card(&db, "card-outbox-api", "ready");
    let api = create_dispatch(
        &db,
        &engine,
        "card-outbox-api",
        "agent-1",
        "implementation",
        "API trail",
        &json!({}),
    )
    .unwrap();
    let api_id = api["id"].as_str().unwrap().to_string();
    set_dispatch_status_on_conn(
        &conn,
        &api_id,
        "dispatched",
        None,
        "turn_bridge_notify",
        Some(&["pending"]),
        false,
    )
    .unwrap();
    set_dispatch_status_on_conn(
        &conn,
        &api_id,
        "completed",
        Some(&json!({"completion_source":"api"})),
        "api",
        Some(&["dispatched"]),
        true,
    )
    .unwrap();
    assert_eq!(
        count_status_reaction_outbox(&conn, &api_id),
        1,
        "#750: completed via api/recovery/etc. must enqueue (no command-bot ✅ on message)"
    );

    seed_card(&db, "card-outbox-failed", "ready");
    let failed = create_dispatch(
        &db,
        &engine,
        "card-outbox-failed",
        "agent-1",
        "implementation",
        "Fail trail",
        &json!({}),
    )
    .unwrap();
    let failed_id = failed["id"].as_str().unwrap().to_string();
    set_dispatch_status_on_conn(
        &conn,
        &failed_id,
        "dispatched",
        None,
        "turn_bridge_notify",
        Some(&["pending"]),
        false,
    )
    .unwrap();
    set_dispatch_status_on_conn(
        &conn,
        &failed_id,
        "failed",
        Some(&json!({"completion_source":"turn_bridge_explicit"})),
        "turn_bridge_explicit",
        Some(&["dispatched"]),
        true,
    )
    .unwrap();
    assert_eq!(
        count_status_reaction_outbox(&conn, &failed_id),
        1,
        "#750: failed ALWAYS enqueues regardless of source (announce bot cleans ✅ and adds ❌)"
    );

    seed_card(&db, "card-outbox-cancelled", "ready");
    let cancelled = create_dispatch(
        &db,
        &engine,
        "card-outbox-cancelled",
        "agent-1",
        "implementation",
        "Cancel trail",
        &json!({}),
    )
    .unwrap();
    let cancelled_id = cancelled["id"].as_str().unwrap().to_string();
    set_dispatch_status_on_conn(
        &conn,
        &cancelled_id,
        "cancelled",
        Some(&json!({"completion_source":"cli"})),
        "cli",
        Some(&["pending"]),
        true,
    )
    .unwrap();
    assert_eq!(
        count_status_reaction_outbox(&conn, &cancelled_id),
        1,
        "#750: cancelled ALWAYS enqueues regardless of source"
    );
}
