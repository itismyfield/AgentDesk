use super::*;
use crate::services::discord::scheduled_turn::ScheduledTurnMark;
use crate::services::scheduled_messages::turn_completion::tests::{
    launch_agent_delivery, stored_outcome,
};

const ASSISTANT: &str =
    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]}}"#;

fn transcript(dir: &std::path::Path, name: &str, is_error: bool) -> (String, u64) {
    let path = dir.join(name);
    let result = format!(
        r#"{{"type":"result","subtype":"success","is_error":{is_error},"result":"plain text answer"}}"#
    );
    std::fs::write(&path, format!("{ASSISTANT}\n{result}\n")).unwrap();
    (path.display().to_string(), ASSISTANT.len() as u64 + 1)
}

fn standby_row(channel: u64, output_path: &str, turn_id: &str) -> InflightTurnState {
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
    state.turn_start_offset = Some(0);
    state.scheduled = ScheduledTurnMark::Scheduled {
        turn_id: turn_id.to_string(),
        isolated: true,
        outcome: None,
    };
    state
}

#[test]
fn only_a_typed_error_result_line_reads_as_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (failed, offset) = transcript(dir.path(), "failed.jsonl", true);
    let (ok, _) = transcript(dir.path(), "ok.jsonl", false);
    assert!(result_line_is_error(&failed, offset));
    assert!(
        !result_line_is_error(&failed, 0),
        "the assistant line is no result"
    );
    assert!(!result_line_is_error(&ok, offset));
    assert!(!result_line_is_error(&failed, 10_000));
    assert!(!result_line_is_error("/nonexistent/standby.jsonl", 0));
}

/// The standby relay settles only the row of its own binding, and a plain-text
/// answer of an `is_error` result settles as a provider error.
#[tokio::test]
async fn standby_settles_only_its_binding_and_never_delivers_an_error_result_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let dir = tempfile::tempdir().unwrap();
    let provider = ProviderKind::Claude;
    let settle_case = |channel: u64, turn_id: &'static str, is_error: bool, rebind: bool| {
        let (shared, provider, dir) = (shared.clone(), provider.clone(), dir.path().to_owned());
        async move {
            let (path, offset) = transcript(&dir, &format!("{channel}.jsonl"), is_error);
            let state = standby_row(channel, &path, turn_id);
            crate::services::discord::inflight::save_inflight_state(&state).unwrap();
            let mut bound = state.clone();
            if rebind {
                bound.turn_start_offset = Some(1);
            }
            let binding = StandbyRelayTurnBinding::from_state(&bound);
            settle(
                &shared,
                &provider,
                ChannelId::new(channel),
                &path,
                None,
                &binding,
                ("plain text answer", Some(offset)),
            )
            .await;
        }
    };
    for turn in ["standby-ok", "standby-error", "standby-other"] {
        launch_agent_delivery(&pool, turn, turn, true, "fail").await;
    }

    settle_case(7_420_101, "standby-ok", false, false).await;
    settle_case(7_420_102, "standby-error", true, false).await;
    settle_case(7_420_103, "standby-other", false, true).await;

    assert_eq!(
        stored_outcome(&pool, "standby-ok").await.as_deref(),
        Some("delivered")
    );
    assert_eq!(
        stored_outcome(&pool, "standby-error").await.as_deref(),
        Some("provider_error")
    );
    assert_eq!(
        stored_outcome(&pool, "standby-other").await,
        None,
        "a binding of another turn start settles nothing"
    );

    pool.close().await;
    db.drop().await;
}
