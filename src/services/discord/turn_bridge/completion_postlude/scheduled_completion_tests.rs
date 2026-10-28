use super::*;
use crate::db::session_transcripts::SessionTranscriptEventKind as Kind;
use crate::services::discord::scheduled_turn::ScheduledTurnMark;
use crate::services::provider::ProviderKind;
use crate::services::scheduled_messages::turn_completion::tests::{
    launch_agent_delivery, stored_outcome,
};
use ScheduledTurnOutcome as Outcome;

fn event(kind: Kind) -> SessionTranscriptEvent {
    SessionTranscriptEvent {
        kind,
        tool_name: None,
        summary: None,
        content: "event".to_string(),
        status: None,
        is_error: kind == Kind::Error,
    }
}

fn row(mark: ScheduledTurnMark) -> crate::services::discord::inflight::InflightTurnState {
    let mut state = crate::services::discord::inflight::InflightTurnState::new(
        ProviderKind::Claude,
        7_420_001,
        None,
        7,
        7_420_002,
        7_420_003,
        "scheduled prompt".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    state.scheduled = mark;
    state
}

fn scheduled(turn_id: &str, outcome: Option<Outcome>) -> ScheduledTurnMark {
    ScheduledTurnMark::Scheduled {
        turn_id: turn_id.to_string(),
        isolated: true,
        outcome,
    }
}

fn completion<'a>(
    body: &'a str,
    events: &'a [SessionTranscriptEvent],
    provider_failure: bool,
) -> ScheduledCompletion<'a> {
    ScheduledCompletion {
        answer_committed: true,
        empty_response: false,
        full_response: body,
        transcript_events: events,
        provider_failure,
    }
}

#[test]
fn the_postlude_classifies_a_scheduled_answer_by_its_final_typed_event() {
    let unmarked = row(scheduled("t", None));
    let mid_error = [
        event(Kind::Error),
        event(Kind::ToolResult),
        event(Kind::Assistant),
    ];
    let final_error = [
        event(Kind::Assistant),
        event(Kind::Error),
        event(Kind::System),
    ];
    let cases: [(&str, &[SessionTranscriptEvent], bool, Option<Outcome>); 5] = [
        ("answer", &mid_error, false, Some(Outcome::Delivered)),
        ("answer", &final_error, false, Some(Outcome::ProviderError)),
        ("answer", &[], true, Some(Outcome::ProviderError)),
        ("NO_REPLY", &[], false, Some(Outcome::NoReply)),
        ("", &[], false, None),
    ];
    for (body, events, provider_failure, expected) in cases {
        assert_eq!(
            completion(body, events, provider_failure).outcome(&unmarked),
            expected,
            "{body:?} {events:?} provider_failure={provider_failure}"
        );
    }

    let empty = ScheduledCompletion {
        empty_response: true,
        ..completion("", &[], false)
    };
    assert_eq!(empty.outcome(&unmarked), Some(Outcome::EmptyResponse));
    let uncommitted = ScheduledCompletion {
        answer_committed: false,
        ..completion("answer", &[], false)
    };
    assert_eq!(uncommitted.outcome(&unmarked), None);
    let mirrored = row(scheduled("t", Some(Outcome::ProviderError)));
    assert_eq!(
        completion("answer", &[], false).outcome(&mirrored),
        Some(Outcome::ProviderError),
        "the outcome stored with the delivery commit wins"
    );
}

#[tokio::test]
async fn the_postlude_records_a_committed_answer_but_not_a_cancelled_one_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    launch_agent_delivery(&pool, "postlude-ok", "postlude-ok-turn", true, "fail").await;
    launch_agent_delivery(
        &pool,
        "postlude-cancel",
        "postlude-cancel-turn",
        true,
        "fail",
    )
    .await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));

    let token = crate::services::provider::CancelToken::new();
    let delivered = row(scheduled("postlude-ok-turn", None));
    record_scheduled_completion(
        &shared,
        &token,
        &delivered,
        completion("answer", &[], false),
    )
    .await;
    assert_eq!(
        stored_outcome(&pool, "postlude-ok-turn").await.as_deref(),
        Some("delivered")
    );

    let cancelled_token = crate::services::provider::CancelToken::new();
    cancelled_token
        .cancelled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let cancelled = row(scheduled("postlude-cancel-turn", None));
    record_scheduled_completion(
        &shared,
        &cancelled_token,
        &cancelled,
        completion("answer", &[], false),
    )
    .await;
    assert_eq!(stored_outcome(&pool, "postlude-cancel-turn").await, None);

    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn an_isolated_mark_alone_scopes_the_completion_as_isolated() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = poise::serenity_prelude::ChannelId::new(7_420_401);
    for (mark, isolated) in [
        (scheduled("t", None), true),
        (ScheduledTurnMark::NotScheduled, false),
    ] {
        let (scope, _routine) = resolve_completion_scope(
            &shared,
            channel,
            &ProviderKind::Claude,
            None,
            "t",
            &row(mark.clone()),
        )
        .await;
        assert_eq!(scope, isolated, "{mark:?}");
    }
}
