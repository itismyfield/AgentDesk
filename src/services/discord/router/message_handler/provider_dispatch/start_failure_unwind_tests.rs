//! A Herdr start held before its first key and the same front's next admission.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::discord::inflight::{self, InflightTurnState};
use crate::services::discord::queue_io::input_owner::drive::claim_turn;
use crate::services::discord::router::message_handler::foreign_row;
use crate::services::provider::cancel_token_claude_interrupt::HerdrSubmission;
use crate::services::turn_host::TurnHost;
use crate::services::turn_orchestrator::QueuePersistenceContext;
use poise::serenity_prelude::{ChannelId, UserId};

const HELD: &str = "NotSent(\"held: PreparationHold { reason: \\\"herdr_composer_not_ready\\\", phase: BeforePayload }\")";
const MESSAGE: u64 = 8_441_001;

fn row(channel: ChannelId, token: &CancelToken) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel.get(),
        Some("unwind-fixture".into()),
        7,
        MESSAGE,
        900_001,
        "original".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_nonce = token.turn_nonce().map(str::to_string);
    row
}

/// The next admission's verdict, the retry's row key, the first row's key and the durable one.
type Retried = (Result<bool, String>, [Option<(Option<String>, u64)>; 3]);

/// One owner start of `MESSAGE` whose input ended as `ended` over the row `shape` left on disk,
/// its unsent failure exit, then the same front's next create through the intake admission.
async fn held_then_retried(
    channel: u64,
    ended: (HerdrSubmission, bool),
    shape: impl FnOnce(&mut InflightTurnState),
) -> Retried {
    let key = |row: InflightTurnState| (row.turn_nonce, row.user_msg_id);
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Claude;
    let channel = ChannelId::new(channel);
    let mailbox = shared.mailbox(channel);
    let queue = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&shared.token_hash)
        .join(format!("{channel}.json"));
    mailbox
        .v2_adopt(queue, shared.token_hash.clone())
        .await
        .unwrap();
    let item: PendingQueueItem = serde_json::from_value(serde_json::json!({"author_id":7,"message_id":MESSAGE,"channel_id":channel.get(),"queued_generation":1,"text":"exact original"})).unwrap();
    mailbox
        .v2_admit_ticket(item, "unwind".into())
        .await
        .unwrap();
    let front = mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("owner")?;
            let front = owner.front().ok_or("front")?;
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    let claim = claim_turn(&mailbox, MESSAGE, &front)
        .await
        .unwrap()
        .unwrap();
    let token = Arc::new(CancelToken::new());
    let state = token.prepare_herdr_interrupt(
        ProviderKind::Claude,
        &HostedOwner {
            provider: "claude".into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: channel.to_string(),
            logical_key: format!("AgentDesk-claude-unwind-{channel}"),
            owner_node: "test-node".into(),
            runtime_root: "test-root".into(),
        },
    );
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    assert!(
        mailbox
            .try_start_owner_front((token.clone(), UserId::new(7)), front, persistence)
            .await
            .started
    );
    let mut first = row(channel, &token);
    shape(&mut first);
    inflight::save_inflight_state_create_new(&first).unwrap();
    {
        let mut input = state.submission.lock().unwrap();
        assert!(state.prepare_input(&mut input));
        state.finish_input(&mut input, ended.0, ended.1);
    }
    let watch = row_removed::watch(&ProviderKind::Claude, channel.get());
    let host = TurnHost::Tmux;
    let turn = StreamingTurn {
        pool: None,
        registration_origin: None,
        agy_foreground: None,
        shared: Some(&shared),
        provider: &ProviderKind::Claude,
        prompt: "exact original",
        session_id: None,
        working_dir: "/tmp",
        system_prompt: None,
        allowed_tools: &[],
        cancel: token.clone(),
        codex_observation: None,
        tmux_session_name: None,
        teardown: None,
        host: &host,
        channel_id: channel.get(),
        model: None,
        native_fast_mode: None,
        effort: None,
        codex_goals: None,
        compact_percent: None,
        compact_lower_bound_tokens: 0,
        compact_token_limit: None,
        cache_ttl_minutes: None,
        dispatch_type: None,
        force_fresh: false,
        submission: None,
    };
    let exit = Failure::claimed(&mailbox, &claim).finish(&turn, watch, Err(HELD.into()));
    assert_eq!(exit, Err(HELD.to_string()), "the owner keeps its front");

    let retry = Arc::new(CancelToken::new());
    let mut next = row(channel, &retry);
    next.set_followup_requeue_context(None, false, false, Vec::new(), true);
    let create = |state: &mut InflightTurnState| inflight::save_inflight_state_create_new(state);
    let verdict = foreign_row::admit(&shared, &ProviderKind::Claude, &mut next, &retry, create);
    let verdict = verdict.await;
    let durable = inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get());
    let rows = [Some(next), Some(first), durable].map(|row| row.map(key));
    (verdict, rows)
}

#[tokio::test]
async fn a_start_held_before_its_first_key_lets_the_same_front_start_next() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let untouched = (HerdrSubmission::Unsubmitted, true);
    let (verdict, [next, _, durable]) = held_then_retried(8_441_101, untouched, |_| {}).await;
    // The held attempt's row is gone, so the retry is admitted over its own row alone.
    assert_eq!((verdict, durable), (Ok(true), next));
}

#[tokio::test]
async fn a_start_whose_key_may_have_reached_the_pane_keeps_its_row() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let reached = [
        (HerdrSubmission::Submitted, false),
        (HerdrSubmission::Unknown, false),
        (HerdrSubmission::Unsubmitted, false),
    ];
    for (case, ended) in reached.into_iter().enumerate() {
        let (verdict, [_, first, durable]) =
            held_then_retried(8_441_201 + case as u64, ended, |_| {}).await;
        let admitted = verdict == Ok(true);
        assert_eq!((admitted, durable), (false, first), "case {case}");
    }
}

#[tokio::test]
async fn a_row_of_another_message_generation_or_turn_is_not_rewound() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let untouched = (HerdrSubmission::Unsubmitted, true);
    let other: [fn(&mut InflightTurnState); 3] = [
        |row| row.user_msg_id = MESSAGE + 1,
        |row| row.born_generation += 1,
        |row| row.turn_nonce = Some("another-turn".into()),
    ];
    for (case, shape) in other.into_iter().enumerate() {
        let (verdict, [_, first, durable]) =
            held_then_retried(8_441_301 + case as u64, untouched, shape).await;
        let admitted = verdict == Ok(true);
        assert_eq!((admitted, durable), (false, first), "case {case}");
    }
}
