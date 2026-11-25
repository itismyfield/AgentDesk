//! #8398: an idle owner's acceptance puts ⏳ on a person's input before the owner wakes; the
//! handoff's return keeps it for a start and otherwise rolls it back to 📬 or clears it.

use super::super::ingress::Intake;
use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests;
use crate::services::discord::{IntakeRequest, TurnKind, make_shared_data_for_tests};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::UserId;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

/// A booted process has a settled queue generation; a fresh test root has none.
fn seed_generation() {
    let path = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "7").unwrap();
}

fn request(channel: u64, message: u64, text: &str) -> IntakeRequest {
    IntakeRequest {
        intake_outbox_id: None,
        channel_id: ChannelId::new(channel),
        user_msg_id: MessageId::new(message),
        source_message_ids: vec![],
        busy_followup_retry_user_msg_id: MessageId::new(message),
        request_owner: UserId::new(7),
        request_owner_name: "author".into(),
        user_text: text.into(),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: false,
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: None,
        turn_kind: TurnKind::Foreground,
        preserve_on_cancel: true,
    }
}

/// A test runtime whose startup reconcile is done, with the channel's owner adopted.
async fn ready_owner(channel: u64) -> Arc<SharedData> {
    seed_generation();
    let shared = make_shared_data_for_tests();
    shared.restart.reconcile_done.store(true, Ordering::SeqCst);
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&shared.token_hash)
        .join(format!("{channel}.json"));
    shared
        .mailbox(ChannelId::new(channel))
        .v2_adopt(path, shared.token_hash.clone())
        .await
        .unwrap();
    shared
}

/// Replaces the channel's drive with a probe that counts the ⏳ adds already made at wake time.
struct WakeProbe {
    channel: ChannelId,
    seen: Arc<Mutex<Vec<usize>>>,
}

impl WakeProbe {
    fn install(shared: &Arc<SharedData>, channel: u64, message: u64) -> Self {
        let channel = ChannelId::new(channel);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (shared, record) = (shared.clone(), seen.clone());
        let target = TurnViewTarget::intake_user_message(channel, MessageId::new(message));
        set_owner_wake_for_tests(
            channel,
            Some(Arc::new(move || {
                let adds = hourglass_adds(&shared, target);
                record.lock().unwrap().push(adds);
            })),
        );
        Self { channel, seen }
    }

    fn seen(&self) -> Vec<usize> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for WakeProbe {
    fn drop(&mut self) {
        set_owner_wake_for_tests(self.channel, None);
    }
}

fn hourglass_adds(shared: &SharedData, target: TurnViewTarget) -> usize {
    shared
        .turn_view_reconciler
        .ops()
        .iter()
        .filter(|op| op.target == target && op.add && op.emoji == '⏳')
        .count()
}

/// The target's reaction operations as (emoji, add) in order.
fn ops(shared: &SharedData, target: TurnViewTarget) -> Vec<(char, bool)> {
    shared
        .turn_view_reconciler
        .ops()
        .iter()
        .filter(|op| op.target == target)
        .map(|op| (op.emoji, op.add))
        .collect()
}

async fn accept(shared: &Arc<SharedData>, channel: u64, message: u64) -> Option<Intake> {
    super::super::ingress::intake(
        shared,
        &ProviderKind::Claude,
        &request(channel, message, "please look"),
        Vec::new(),
        false,
        false,
    )
    .await
}

#[tokio::test]
async fn idle_acceptance_puts_hourglass_before_the_owner_wakes() {
    let _root = TestRuntimeRootGuard::new();
    let (channel, message) = (838_101, 100_000_000_838_101);
    let shared = ready_owner(channel).await;
    let probe = WakeProbe::install(&shared, channel, message);

    assert_eq!(
        accept(&shared, channel, message).await,
        Some(Intake::Accepted)
    );

    assert_eq!(
        probe.seen(),
        vec![1],
        "⏳ lands before the start is decided"
    );
    let target =
        TurnViewTarget::intake_user_message(ChannelId::new(channel), MessageId::new(message));
    assert_eq!(ops(&shared, target), vec![('⏳', true)]);
}

#[tokio::test]
async fn an_owner_already_holding_input_takes_no_early_hourglass() {
    let _root = TestRuntimeRootGuard::new();
    let channel = 838_111;
    let (first, second) = (100_000_000_838_111, 100_000_000_838_112);
    let shared = ready_owner(channel).await;
    let _probe = WakeProbe::install(&shared, channel, first);

    assert_eq!(
        accept(&shared, channel, first).await,
        Some(Intake::Accepted)
    );
    assert_eq!(
        accept(&shared, channel, second).await,
        Some(Intake::Accepted)
    );

    let second =
        TurnViewTarget::intake_user_message(ChannelId::new(channel), MessageId::new(second));
    assert_eq!(
        ops(&shared, second),
        Vec::new(),
        "a waiting input keeps its queue view"
    );
}

#[test]
fn only_a_persons_real_message_is_eligible() {
    let person = request(838_121, 100_000_000_838_121, "please look");
    let owned = |request: &IntakeRequest, bot: bool| {
        crate::services::discord::router::input_payload::owned_original(
            request,
            bot,
            Vec::new(),
            false,
        )
    };
    assert!(eligible(&owned(&person, false)));
    assert!(
        !eligible(&owned(&person, true)),
        "bot or background trigger"
    );
    let dispatch = request(838_121, 100_000_000_838_122, "DISPATCH: card 7");
    assert!(!eligible(&owned(&dispatch, false)));
    let internal = request(
        838_121,
        crate::services::discord::INTERNAL_MESSAGE_ID_START + 1,
        "please look",
    );
    assert!(!eligible(&owned(&internal, false)));
}

#[tokio::test]
async fn handoff_return_rolls_an_unstarted_early_hourglass_back_to_mailbox() {
    let _root = TestRuntimeRootGuard::new();
    let (channel, message) = (838_131, 100_000_000_838_131);
    let shared = ready_owner(channel).await;
    let _probe = WakeProbe::install(&shared, channel, message);
    assert_eq!(
        accept(&shared, channel, message).await,
        Some(Intake::Accepted)
    );

    // Busy pane, race loss, replay deferral or a writer hold: the owner keeps the original.
    let result = after_handoff(Some(shared.clone()), channel, message, async {
        Err("NotSent(\"busy\")".to_string())
    })
    .await;

    assert_eq!(result, Err("NotSent(\"busy\")".to_string()));
    let target =
        TurnViewTarget::intake_user_message(ChannelId::new(channel), MessageId::new(message));
    assert_eq!(
        ops(&shared, target),
        vec![('⏳', true), ('⏳', false), ('📬', true)]
    );
}

#[tokio::test]
async fn handoff_return_clears_the_early_hourglass_of_input_that_left_the_queue() {
    let _root = TestRuntimeRootGuard::new();
    let (channel, message) = (838_141, 100_000_000_838_141);
    let shared = ready_owner(channel).await;
    let id = ChannelId::new(channel);
    // Replay refusal, a blocked admission or an injection: the owner no longer holds it.
    assert!(note(&shared, id, MessageId::new(message)).await);

    let result = after_handoff(Some(shared.clone()), channel, message, async { Ok(()) }).await;

    assert_eq!(result, Ok(()));
    let target = TurnViewTarget::intake_user_message(id, MessageId::new(message));
    assert_eq!(ops(&shared, target), vec![('⏳', true), ('⏳', false)]);
}

#[tokio::test]
async fn a_start_inside_the_handoff_takes_over_the_early_hourglass() {
    let _root = TestRuntimeRootGuard::new();
    let (channel, message) = (838_151, 100_000_000_838_151);
    let shared = ready_owner(channel).await;
    let _probe = WakeProbe::install(&shared, channel, message);
    assert_eq!(
        accept(&shared, channel, message).await,
        Some(Intake::Accepted)
    );
    let http = Arc::new(poise::serenity_prelude::http::Http::new(""));
    let id = ChannelId::new(channel);

    let start = {
        let (shared, http) = (shared.clone(), http.clone());
        async move {
            crate::services::discord::turn_view_reconciler::note_intake_turn_started_current_with_attempt(
                &shared,
                &http,
                id,
                MessageId::new(message),
                "queued_replay_input_started",
            )
            .await;
            Ok(())
        }
    };
    let result = after_handoff(Some(shared.clone()), channel, message, start).await;

    assert_eq!(result, Ok(()));
    let target = TurnViewTarget::intake_user_message(id, MessageId::new(message));
    assert_eq!(
        ops(&shared, target),
        vec![('⏳', true)],
        "one ⏳ add, never removed"
    );
}
