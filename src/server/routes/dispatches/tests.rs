use super::outbox::{
    extract_review_verdict, format_dispatch_message, handle_completed_dispatch_followups,
    prefix_dispatch_message, use_counter_model_channel,
};
use crate::db::Db;
use crate::engine::PolicyEngine;
use crate::server::routes::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;

fn test_db() -> Db {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    crate::db::schema::migrate(&conn).unwrap();
    crate::db::wrap_conn(conn)
}

fn test_engine(db: &Db) -> PolicyEngine {
    let config = crate::config::Config::default();
    PolicyEngine::new(&config, db.clone()).unwrap()
}

#[test]
fn review_dispatch_uses_counter_model_channel() {
    assert!(use_counter_model_channel(Some("review")));
    assert!(use_counter_model_channel(Some("e2e-test")));
    // #256: consultation dispatches go to counter-model channel
    assert!(use_counter_model_channel(Some("consultation")));
    // review-decision goes to the original agent's primary channel,
    // not the counter-model channel, to reuse the implementation thread
    assert!(!use_counter_model_channel(Some("review-decision")));
    assert!(!use_counter_model_channel(Some("implementation")));
    assert!(!use_counter_model_channel(Some("rework")));
    assert!(!use_counter_model_channel(None));
}

#[test]
fn review_dispatch_message_includes_review_only_banner() {
    let message = format_dispatch_message(
        "dispatch-1",
        "[Review R1] card-1",
        Some("https://github.com/itismyfield/AgentDesk/issues/19"),
        Some(19),
        true,
        Some("abc123"),
        Some("codex"),
        None,
        Some("review"),
        None,
    );

    assert!(message.starts_with("DISPATCH:dispatch-1 [🔍 리뷰] - [Review R1] card-1"));
    assert!(message.contains("⚠️ 검토 전용"));
    assert!(message.contains("코드 리뷰만 수행하고 GitHub 이슈에 코멘트로 피드백해주세요."));
    assert!(
        message.contains(
            "[Review R1] card-1 #19](<https://github.com/itismyfield/AgentDesk/issues/19>)"
        )
    );
    // Verdict API instructions must be present for counter-model reviewers
    assert!(message.contains("review-verdict"));
    assert!(message.contains("VERDICT: pass|improve|reject|rework"));
    assert!(message.contains("dispatch-1"));
    assert!(message.contains("abc123"));
    // Provider must be included in the curl example
    assert!(message.contains(r#""provider":"codex""#));
}

#[test]
fn review_dispatch_message_with_branch() {
    let message = format_dispatch_message(
        "dispatch-br",
        "[Review R1] card-1",
        Some("https://github.com/itismyfield/AgentDesk/issues/19"),
        Some(19),
        true,
        Some("abc12345deadbeef"),
        Some("codex"),
        Some("wt/feature-branch"),
        Some("review"),
        None,
    );

    assert!(message.contains("리뷰 대상 브랜치: `wt/feature-branch`"));
    assert!(message.contains("commit: `abc12345`"));
    assert!(message.contains("main 브랜치가 아닙니다"));
}

#[test]
fn review_dispatch_message_with_merge_base_diff_instructions() {
    let message = format_dispatch_message(
        "dispatch-merge-base",
        "[Review R1] card-1",
        Some("https://github.com/itismyfield/AgentDesk/issues/19"),
        Some(19),
        true,
        Some("abc12345deadbeef0011223344556677"),
        Some("codex"),
        Some("wt/feature-branch"),
        Some("review"),
        Some(r#"{"merge_base":"11223344556677889900aabbccddeeff00112233"}"#),
    );

    assert!(message.contains("merge-base(main, `wt/feature-branch`)"));
    assert!(message.contains("11223344556677889900aabbccddeeff00112233"));
    assert!(message.contains(
        "git diff 11223344556677889900aabbccddeeff00112233..abc12345deadbeef0011223344556677"
    ));
}

#[test]
fn review_dispatch_message_without_commit() {
    let message = format_dispatch_message(
        "dispatch-no-commit",
        "[Review R1] card-1",
        None,
        None,
        true,
        None,
        None,
        None,
        Some("review"),
        None,
    );

    assert!(message.contains("review-verdict"));
    assert!(message.contains("dispatch-no-commit"));
    // No commit arg in the curl command
    assert!(!message.contains(r#""commit""#));
}

#[test]
fn implementation_dispatch_message_stays_compact() {
    let message = format_dispatch_message(
        "dispatch-2",
        "Implement feature",
        Some("https://github.com/itismyfield/AgentDesk/issues/24"),
        Some(24),
        false,
        None,
        None,
        None,
        Some("implementation"),
        None,
    );

    assert!(message.contains("[📋 구현]"));
    assert!(
        message.contains(
            "[Implement feature #24](<https://github.com/itismyfield/AgentDesk/issues/24>)"
        )
    );
    assert!(message.contains("`OUTCOME: noop`"));
    assert!(!message.contains("검토 전용"));
    // Implementation dispatches should NOT include verdict instructions
    assert!(!message.contains("review-verdict"));
}

#[test]
fn e2e_test_dispatch_message_uses_general_completion_contract() {
    let message = format_dispatch_message(
        "dispatch-e2e",
        "Run regression",
        Some("https://github.com/itismyfield/AgentDesk/issues/340"),
        Some(340),
        true,
        None,
        Some("codex"),
        None,
        Some("e2e-test"),
        None,
    );

    assert!(message.contains("[🧪 E2E 테스트]"));
    assert!(message.contains("PATCH"));
    assert!(message.contains("/api/dispatches/dispatch-e2e"));
    assert!(!message.contains("검토 전용"));
    assert!(!message.contains("review-verdict"));
    assert!(!message.contains("VERDICT: pass|improve|reject|rework"));
}

#[test]
fn consultation_dispatch_message_uses_general_completion_contract() {
    let message = format_dispatch_message(
        "dispatch-consult",
        "Need investigation",
        Some("https://github.com/itismyfield/AgentDesk/issues/256"),
        Some(256),
        true,
        None,
        Some("codex"),
        None,
        Some("consultation"),
        None,
    );

    assert!(message.contains("PATCH"));
    assert!(message.contains("/api/dispatches/dispatch-consult"));
    assert!(!message.contains("검토 전용"));
    assert!(!message.contains("review-verdict"));
    assert!(!message.contains("VERDICT: pass|improve|reject|rework"));
}

#[test]
fn review_decision_primary_message_includes_action_instructions() {
    let message = format_dispatch_message(
        "dispatch-rd",
        "[리뷰 검토] Test Card",
        Some("https://github.com/itismyfield/AgentDesk/issues/249"),
        Some(249),
        false,
        None,
        None,
        None,
        Some("review-decision"),
        Some(r#"{"verdict":"rework"}"#),
    );

    assert!(message.contains("[⚖️ 리뷰 검토]"));
    assert!(message.contains("카운터모델 리뷰 결과: **rework**"));
    assert!(message.contains("review-decision API에 `accept` 호출"));
    assert!(message.contains("review-decision API에 `dispute` 호출"));
    assert!(message.contains("review-decision API에 `dismiss` 호출"));
    assert!(message.contains("#249](<https://github.com/itismyfield/AgentDesk/issues/249>)"));
    assert!(!message.contains("review-verdict"));
}

#[test]
fn prefix_dispatch_message_merges_separator_and_body() {
    let message = prefix_dispatch_message("review-decision", "DISPATCH:d-1 - Example");
    assert_eq!(
        message,
        "── review-decision dispatch ──\nDISPATCH:d-1 - Example"
    );
}

#[test]
fn review_verdict_extraction_defaults_to_unknown() {
    // Missing verdict must NOT default to "pass" — that caused false review passes
    assert_eq!(extract_review_verdict(None), "unknown");
    assert_eq!(
        extract_review_verdict(Some(r#"{"auto_completed":true}"#)),
        "unknown"
    );
    assert_eq!(
        extract_review_verdict(Some(r#"{"decision":"dismiss"}"#)),
        "dismiss"
    );
    assert_eq!(
        extract_review_verdict(Some(r#"{"verdict":"improve"}"#)),
        "improve"
    );
    assert_eq!(
        extract_review_verdict(Some(r#"{"verdict":"pass"}"#)),
        "pass"
    );
}

#[tokio::test]
async fn completed_review_dispatch_with_explicit_verdict_creates_followup() {
    // When a review dispatch has an explicit verdict (e.g. "improve"),
    // Rust creates a review-decision dispatch for the original agent.
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (
                id, name, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
             ) VALUES ('agent-1', 'Agent 1', 'claude', '123', '456', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-1', 'Needs follow-up', 'review', 'agent-1', 'dispatch-review', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, context, created_at, updated_at)
             VALUES ('dispatch-review', 'card-1', 'agent-1', 'review', 'completed', '[Review R1] card-1', '{\"verdict\":\"improve\"}', '{\"from_provider\":\"codex\",\"target_provider\":\"claude\"}', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    handle_completed_dispatch_followups(&db, "dispatch-review")
        .await
        .expect("review followup should succeed");

    let conn = db.lock().unwrap();
    let latest_dispatch_id: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(latest_dispatch_id, "dispatch-review");
    let (dispatch_type, dispatch_status, context): (String, String, Option<String>) = conn
        .query_row(
            "SELECT dispatch_type, status, context FROM task_dispatches WHERE id = ?1",
            [&latest_dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(dispatch_type, "review-decision");
    assert_eq!(dispatch_status, "pending");
    let context = context.expect("review-decision should persist provider routing context");
    assert!(context.contains("\"from_provider\":\"codex\""));
}

#[test]
fn review_decision_routing_falls_back_to_latest_completed_review_provider() {
    let db = test_db();
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO agents (
            id, name, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
         ) VALUES ('agent-1', 'Agent 1', 'claude', '123', '456', '123', '456')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
         VALUES ('card-route', 'Route test', 'review', 'agent-1', 'dispatch-rd', datetime('now'), datetime('now'))",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
         VALUES ('dispatch-review', 'card-route', 'agent-1', 'review', 'completed', '[Review R1] card-route', '{\"from_provider\":\"codex\",\"target_provider\":\"claude\"}', datetime('now', '-1 minute'), datetime('now', '-1 minute'))",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
         VALUES ('dispatch-rd', 'card-route', 'agent-1', 'review-decision', 'pending', '[리뷰 검토] card-route', datetime('now'), datetime('now'))",
        [],
    )
    .unwrap();

    let channel = super::discord_delivery::resolve_dispatch_delivery_channel_on_conn(
        &conn,
        "agent-1",
        "card-route",
        Some("review-decision"),
        None,
    )
    .unwrap();
    assert_eq!(
        channel.as_deref(),
        Some("456"),
        "review-decision should route back to the implementation provider channel"
    );
}

#[tokio::test]
async fn auto_completed_review_dispatch_skips_rust_followup() {
    // When a review dispatch is auto-completed without a verdict,
    // Rust should NOT create a followup (policy engine handles it).
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-1', 'Auto test', 'review', 'agent-1', 'dispatch-auto', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
             VALUES ('dispatch-auto', 'card-1', 'agent-1', 'review', 'completed', '[Review R1] card-1', '{\"auto_completed\":true,\"completion_source\":\"session_idle\"}', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    handle_completed_dispatch_followups(&db, "dispatch-auto")
        .await
        .expect("auto-completed review followup should succeed");

    let conn = db.lock().unwrap();
    let latest_dispatch_id: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // latest_dispatch_id should remain unchanged — auto-complete with "unknown" verdict skips Rust followup
    assert_eq!(latest_dispatch_id, "dispatch-auto");
}

/// After an implementation dispatch completes, if hooks created a review dispatch
/// (latest_dispatch_id changed), handle_completed_dispatch_followups should detect it
/// and attempt to send it to Discord. This test verifies the detection logic without
/// actually hitting Discord (send_dispatch_to_discord will no-op without a bot token).
#[tokio::test]
async fn impl_dispatch_followup_detects_new_review_dispatch() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-1', 'Impl card', 'review', 'agent-1', 'dispatch-review', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        // The completed implementation dispatch
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
             VALUES ('dispatch-impl', 'card-1', 'agent-1', 'implementation', 'completed', 'Impl card', '{\"auto_completed\":true}', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        // The review dispatch created by hooks after implementation completion
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
             VALUES ('dispatch-review', 'card-1', 'agent-1', 'review', 'pending', '[Review R1] card-1', '{}', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    // handle_completed_dispatch_followups should detect that latest_dispatch_id
    // ('dispatch-review') differs from the completed dispatch ('dispatch-impl')
    // and attempt send_dispatch_to_discord (which no-ops without bot token).
    // The key assertion: no panic, no error, and the review dispatch stays pending.
    handle_completed_dispatch_followups(&db, "dispatch-impl")
        .await
        .expect("implementation followup should succeed");

    let conn = db.lock().unwrap();
    // latest_dispatch_id should still point to the review dispatch
    let latest: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(latest, "dispatch-review");

    // Review dispatch should remain pending (not modified by followup handler)
    let review_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-review'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(review_status, "pending");
}

#[tokio::test]
async fn thread_not_archived_when_card_not_done() {
    // When an implementation dispatch completes but card is in "review" (not done),
    // the thread should NOT be archived — it may be reused for rework/review-decision.
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, active_thread_id)
             VALUES ('card-1', 'In Review', 'review', 'agent-1', 'dispatch-impl', '999888777')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id, created_at, updated_at)
             VALUES ('dispatch-impl', 'card-1', 'agent-1', 'implementation', 'completed', 'card-1', '999888777', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    handle_completed_dispatch_followups(&db, "dispatch-impl")
        .await
        .expect("thread reuse followup should succeed");

    // active_thread_id should still be set (NOT cleared) because card is not done
    let conn = db.lock().unwrap();
    let active_thread: Option<String> = conn
        .query_row(
            "SELECT active_thread_id FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active_thread, Some("999888777".to_string()));
}

#[tokio::test]
async fn thread_archived_and_cleared_when_card_done() {
    // When a card reaches "done", active_thread_id should be cleared.
    // (Thread archiving requires Discord API call, but we verify the DB cleanup.)
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, active_thread_id)
             VALUES ('card-1', 'Done Card', 'done', 'agent-1', 'dispatch-final', '999888777')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id, created_at, updated_at)
             VALUES ('dispatch-final', 'card-1', 'agent-1', 'implementation', 'completed', 'card-1', '999888777', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    handle_completed_dispatch_followups(&db, "dispatch-final")
        .await
        .expect("done-card followup should succeed");

    // active_thread_id should be cleared when card is done
    let conn = db.lock().unwrap();
    let active_thread: Option<String> = conn
        .query_row(
            "SELECT active_thread_id FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(active_thread.is_none());
}

/// When an explicit review verdict (improve/rework/reject) completes,
/// send_review_result_to_primary creates the review-decision dispatch
/// and sets review_followup_handled=true, preventing duplicate resend
/// via the generic latest_dispatch_id check.
#[tokio::test]
#[ignore] // CI: send_review_result_to_primary early-returns without local ADK runtime
async fn review_followup_skips_generic_resend_for_explicit_verdict() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-1', 'Review test', 'review', 'agent-1', 'dispatch-review', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
             VALUES ('dispatch-review', 'card-1', 'agent-1', 'review', 'completed', '[Review R1] card-1', '{\"verdict\":\"rework\"}', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    handle_completed_dispatch_followups(&db, "dispatch-review")
        .await
        .expect("explicit review verdict followup should succeed");

    let conn = db.lock().unwrap();
    // A review-decision dispatch should have been created
    let latest_dispatch_id: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(latest_dispatch_id, "dispatch-review");

    // Count total dispatches — should be exactly 2 (original review + one review-decision).
    // Before this fix, the generic latest_dispatch_id check would call send_dispatch_to_discord
    // again, potentially creating duplicate notifications.
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 2,
        "should have exactly 2 dispatches (review + review-decision), not more"
    );

    let (dt, ds): (String, String) = conn
        .query_row(
            "SELECT dispatch_type, status FROM task_dispatches WHERE id = ?1",
            [&latest_dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(dt, "review-decision");
    assert_eq!(ds, "pending");
}

/// When the agent's discord_channel_id points to a non-existent channel,
/// send_dispatch_to_discord must NOT write the notified marker.
/// This ensures that Discord send failures leave the dispatch recoverable
/// by timeouts.js [I-0].
#[tokio::test]
#[ignore] // CI: send_dispatch_to_discord early-returns without local ADK runtime
async fn no_notified_marker_when_discord_send_fails() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        // Use a bogus numeric channel ID that will fail at Discord API
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-1', 'Test card', 'requested', 'agent-1', 'dispatch-1', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-1', 'card-1', 'agent-1', 'implementation', 'pending', 'Test card', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    // Channel ID "1" is a valid u64 but not a real Discord channel.
    // Thread creation and fallback will both fail with Discord API errors.
    // No notified marker should be written.
    let send_result = super::discord_delivery::send_dispatch_to_discord(
        &db,
        "agent-1",
        "Test card",
        "card-1",
        "dispatch-1",
    )
    .await;
    assert!(
        send_result.is_err(),
        "bogus Discord channel should fail delivery"
    );

    let conn = db.lock().unwrap();
    let marker_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = 'dispatch_notified:dispatch-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        marker_count, 0,
        "notified marker must not be written when Discord send fails"
    );

    // thread_id should also NOT be saved (rollback on failure)
    let thread_id: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        thread_id.is_none(),
        "thread_id must not be saved when thread message POST fails"
    );
}

/// send_review_result_to_primary must not create a review-decision dispatch
/// for done cards — the central create_dispatch_core done guard blocks it.
#[tokio::test]
async fn review_followup_does_not_create_dispatch_for_done_card() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-done', 'Done Card', 'done', 'agent-1', 'dispatch-review', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
             VALUES ('dispatch-review', 'card-done', 'agent-1', 'review', 'completed', '[Review R1]', '{\"verdict\":\"rework\"}', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
    }

    // This triggers send_review_result_to_primary for a done card.
    // The done-card guard should reject creating a review-decision dispatch,
    // but the original dispatch/card state must remain unchanged.
    let result = handle_completed_dispatch_followups(&db, "dispatch-review").await;
    let error = result.expect_err("done-card review followup must fail closed");
    assert!(
        error.contains("Cannot create review-decision dispatch for terminal card"),
        "unexpected followup error: {error}"
    );

    let conn = db.lock().unwrap();
    // latest_dispatch_id should NOT have changed (no new dispatch created)
    let latest: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-done'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        latest, "dispatch-review",
        "done card latest_dispatch_id must not be overwritten"
    );

    // Only the original dispatch should exist — no review-decision was created
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-done'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "no review-decision dispatch should be created for done card"
    );
}

#[tokio::test]
async fn pending_dispatch_lookup_ignores_legacy_auto_queue_run_unified_thread_id() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '111222333')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
            ) VALUES (
                'card-legacy-run', 'Legacy run thread', 'review', 'agent-1',
                'dispatch-review', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-review', 'card-legacy-run', 'review', 'pending',
                '[Review R1] card-legacy-run', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread, unified_thread_id
            ) VALUES (
                'run-legacy', 'test/repo', 'agent-1', 'active', 1,
                '{\"111222333\":\"999888777\"}'
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, dispatch_id, status
            ) VALUES (
                'entry-legacy', 'run-legacy', 'card-legacy-run', 'agent-1',
                'dispatch-review', 'dispatched'
            )",
            [],
        )
        .unwrap();
    }

    let state = AppState::test_state(db.clone(), test_engine(&db));
    let (status, body) = super::get_pending_dispatch_for_thread(
        State(state),
        Query(std::collections::HashMap::from([(
            "thread_id".to_string(),
            "999888777".to_string(),
        )])),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.0["error"], "no pending dispatch for thread");
}

#[tokio::test]
async fn pending_dispatch_lookup_finds_review_thread_dispatch() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, active_thread_id, created_at, updated_at)
             VALUES ('card-review', 'Review card', 'review', '999888777', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-review', 'card-review', 'review', 'pending', '[Review R1] card-review', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    let state = AppState::test_state(db.clone(), test_engine(&db));
    let (status, body) = super::get_pending_dispatch_for_thread(
        State(state),
        Query(std::collections::HashMap::from([(
            "thread_id".to_string(),
            "999888777".to_string(),
        )])),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.0["dispatch_id"], "dispatch-review");
}

#[tokio::test]
async fn pending_dispatch_lookup_finds_review_decision_thread_dispatch() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, channel_thread_map, active_thread_id, created_at, updated_at)
             VALUES ('card-decision', 'Decision card', 'review', '{\"123456789\":\"999888777\"}', '999888777', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-review', 'card-decision', 'review', 'completed', '[Review R1] card-decision', datetime('now', '-1 minute'), datetime('now', '-1 minute'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-review-decision', 'card-decision', 'review-decision', 'pending', '[리뷰 검토] card-decision', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    let state = AppState::test_state(db.clone(), test_engine(&db));
    let (status, body) = super::get_pending_dispatch_for_thread(
        State(state),
        Query(std::collections::HashMap::from([(
            "thread_id".to_string(),
            "999888777".to_string(),
        )])),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.0["dispatch_id"], "dispatch-review-decision");
}
