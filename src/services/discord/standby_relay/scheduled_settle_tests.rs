use super::*;
use crate::services::discord::scheduled_turn::ScheduledTurnMark;
use crate::services::scheduled_messages::turn_completion::tests::{
    launch_agent_delivery, stored_outcome,
};

const PRIOR: &str = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"previous turn failed"}"#;
const ASSISTANT: &str =
    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"plain text answer"}]}}"#;
const OK: &str =
    r#"{"type":"result","subtype":"success","is_error":false,"result":"plain text answer"}"#;
const ERROR_WITH_BODY: &str =
    r#"{"type":"result","subtype":"success","is_error":true,"result":"plain text answer"}"#;
const ERROR_WITHOUT_BODY: &str =
    r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["boom"]}"#;
const ERROR_EMPTY_BODY: &str =
    r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":""}"#;

/// A transcript holding a previous turn's failed result, then this turn's lines;
/// returns the path and this turn's start offset.
fn transcript(dir: &std::path::Path, name: &str, turn: &[&str]) -> (String, u64) {
    let path = dir.join(name);
    let mut text = format!("{PRIOR}\n");
    for line in turn {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    (path.display().to_string(), PRIOR.len() as u64 + 1)
}

#[test]
fn only_this_turns_terminal_result_decides_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let cases: [(&str, &[&str], Option<bool>); 5] = [
        ("ok", &[ASSISTANT, OK], Some(false)),
        ("error-body", &[ASSISTANT, ERROR_WITH_BODY], Some(true)),
        (
            "error-no-body",
            &[ASSISTANT, ERROR_WITHOUT_BODY],
            Some(true),
        ),
        ("error-empty", &[ASSISTANT, ERROR_EMPTY_BODY], Some(true)),
        ("fallback", &[ASSISTANT], None),
    ];
    for (name, lines, expected) in cases {
        let (path, start) = transcript(dir.path(), name, lines);
        assert_eq!(terminal_result_is_error(&path, start), expected, "{name}");
    }
    assert_eq!(
        terminal_result_is_error("/nonexistent/standby.jsonl", 0),
        None
    );
}

fn standby_row(channel: u64, output_path: &str, start: u64, turn_id: &str) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        channel + 1,
        channel + 2,
        "scheduled prompt".to_string(),
        None,
        None,
        Some(output_path.to_string()),
        None,
        0,
    );
    state.turn_start_offset = Some(start);
    state.scheduled = ScheduledTurnMark::Scheduled {
        turn_id: turn_id.to_string(),
        isolated: true,
        outcome: None,
    };
    state
}

/// The standby relay settles only its binding's row, never as delivered when this
/// turn's terminal result is a typed error, and not at all without a terminal result.
#[tokio::test]
async fn standby_settles_only_its_binding_by_its_terminal_result_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let dir = tempfile::tempdir().unwrap();
    let cases: [(&str, &[&str], bool, Option<&str>); 6] = [
        ("standby-ok", &[ASSISTANT, OK], false, Some("delivered")),
        (
            "standby-error-body",
            &[ASSISTANT, ERROR_WITH_BODY],
            false,
            Some("provider_error"),
        ),
        (
            "standby-error-no-body",
            &[ASSISTANT, ERROR_WITHOUT_BODY],
            false,
            Some("provider_error"),
        ),
        (
            "standby-error-empty",
            &[ASSISTANT, ERROR_EMPTY_BODY],
            false,
            Some("provider_error"),
        ),
        ("standby-fallback", &[ASSISTANT], false, None),
        ("standby-other", &[ASSISTANT, OK], true, None),
    ];
    for (n, (turn, lines, other_binding, expected)) in cases.into_iter().enumerate() {
        launch_agent_delivery(&pool, turn, turn, true, "fail").await;
        let channel = 7_420_101 + n as u64;
        let (path, start) = transcript(dir.path(), turn, lines);
        let state = standby_row(channel, &path, start, turn);
        crate::services::discord::inflight::save_inflight_state(&state).unwrap();
        let mut bound = state.clone();
        if other_binding {
            bound.turn_start_offset = Some(start + 1);
        }
        let binding = StandbyRelayTurnBinding::from_state(&bound);
        settle(
            &shared,
            &ProviderKind::Claude,
            ChannelId::new(channel),
            &path,
            None,
            &binding,
            "plain text answer",
        )
        .await;
        assert_eq!(
            stored_outcome(&pool, turn).await.as_deref(),
            expected,
            "{turn}"
        );
    }

    pool.close().await;
    db.drop().await;
}
