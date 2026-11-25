//! An idle owner's acceptance ⏳ lands before the wake, stays for a start, else 📬 or cleared.

use super::super::drive::{Front, Handoff};
use super::super::ingress::{AFTER_EARLY_NOTE, BEFORE_EARLY_NOTE, BEFORE_RESUME, Intake, Pause};
use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests;
use crate::services::discord::turn_view_reconciler::TurnViewReconciler;
use crate::services::discord::{IntakeRequest, TurnKind, make_shared_data_for_tests};
use crate::services::provider::ProviderKind;
use crate::test_support::wait::{WAIT_CAP, await_within};
use poise::serenity_prelude::UserId;
use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use tokio::sync::oneshot;

/// A booted process has a settled queue generation; a fresh test root has none.
fn seed_generation() {
    let path = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "7").unwrap();
}

fn request(channel: u64, message: u64, text: &str) -> IntakeRequest {
    merging(channel, message, text, false)
}

fn merging(channel: u64, message: u64, text: &str, merge_consecutive: bool) -> IntakeRequest {
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
        merge_consecutive,
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
    admit(shared, request(channel, message, "please look")).await
}

async fn admit(shared: &Arc<SharedData>, request: IntakeRequest) -> Option<Intake> {
    super::super::ingress::intake(
        shared,
        &ProviderKind::Claude,
        &request,
        Vec::new(),
        false,
        false,
    )
    .await
}

fn target(channel: u64, message: u64) -> TurnViewTarget {
    TurnViewTarget::intake_user_message(ChannelId::new(channel), MessageId::new(message))
}

/// A paused seam and the handles that see the task reach it and let it go.
fn seam() -> (
    RefCell<Option<Pause>>,
    oneshot::Receiver<()>,
    oneshot::Sender<()>,
) {
    let (entered, reached) = oneshot::channel();
    let (release, released) = oneshot::channel();
    (RefCell::new(Some((entered, released))), reached, release)
}

async fn queued(shared: &SharedData, channel: u64) -> Vec<PendingQueueItem> {
    shared
        .mailbox(ChannelId::new(channel))
        .v2_request(|slot| {
            Ok(slot
                .as_ref()
                .map(|o| o.snapshot.items.clone())
                .unwrap_or_default())
        })
        .await
        .unwrap()
}

/// Takes every waiting input out of the owner without queue-exit feedback, as a submit does.
async fn take_all(shared: &SharedData, channel: u64) {
    shared
        .mailbox(ChannelId::new(channel))
        .v2_request(|slot| slot.as_mut().ok_or("no owner")?.clear_waiting().map(drop))
        .await
        .unwrap();
}

/// The real owner drive's handoff, whose idle start is `idle`.
fn real_handoff(shared: &Arc<SharedData>, channel: u64, idle: Handoff) -> Handoff {
    let drive = crate::services::discord::router::message_handler::owner_drive_for_test(
        (shared.mailbox(ChannelId::new(channel)), None),
        (ProviderKind::Codex, channel),
        idle,
        Some(shared.clone()),
    );
    drive.handoff.clone()
}

fn front(item: PendingQueueItem) -> Front {
    (None, item)
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
    assert_eq!(ops(&shared, target(channel, message)), vec![('⏳', true)]);
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

    assert_eq!(
        ops(&shared, target(channel, second)),
        Vec::new(),
        "it keeps its queue view"
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

/// Two arrivals both pass the old empty-owner check before either is admitted; only the
/// one the owner's own turn finds as its sole head is marked, and the merged submit leaves none.
#[tokio::test]
async fn concurrent_arrivals_mark_only_the_head_the_owner_opened() {
    let _root = TestRuntimeRootGuard::new();
    let channel = 838_131;
    let (a, b) = (100_000_000_838_131, 100_000_000_838_132);
    let shared = ready_owner(channel).await;
    let _probe = WakeProbe::install(&shared, channel, a);
    let (seam_a, reached_a, release_a) = seam();
    let (seam_b, reached_b, release_b) = seam();
    let task = |message: u64, pause: RefCell<Option<Pause>>| {
        let shared = shared.clone();
        tokio::spawn(BEFORE_RESUME.scope(pause, async move {
            admit(&shared, merging(channel, message, "same author", true)).await
        }))
    };
    let (task_a, task_b) = (task(a, seam_a), task(b, seam_b));
    await_within(reached_a, WAIT_CAP).await.unwrap();
    await_within(reached_b, WAIT_CAP).await.unwrap();

    release_a.send(()).unwrap();
    assert_eq!(
        await_within(task_a, WAIT_CAP).await.unwrap(),
        Some(Intake::Accepted)
    );
    release_b.send(()).unwrap();
    assert_eq!(
        await_within(task_b, WAIT_CAP).await.unwrap(),
        Some(Intake::Accepted)
    );
    assert_eq!(
        ops(&shared, target(channel, b)),
        Vec::new(),
        "the follower gets no early ⏳"
    );

    // The real drive hands A; the provider submits A with B merged and A's start takes over.
    let head = queued(&shared, channel).await.remove(0);
    let starter = shared.clone();
    let idle: Handoff = Arc::new(move |front: Front| {
        let shared = starter.clone();
        Box::pin(async move {
            take_all(&shared, channel).await;
            let http = Arc::new(poise::serenity_prelude::Http::new(""));
            crate::services::discord::turn_view_reconciler::note_intake_turn_started_current_with_attempt(
                &shared,
                &http,
                ChannelId::new(channel),
                MessageId::new(front.1.message_id),
                "queued_replay_input_started",
            )
            .await;
            Ok(())
        })
    });
    let handoff = real_handoff(&shared, channel, idle);
    assert_eq!(handoff(front(head)).await, Ok(()));

    assert_eq!(
        ops(&shared, target(channel, a)),
        vec![('⏳', true)],
        "A keeps its one ⏳"
    );
    assert_eq!(ops(&shared, target(channel, b)), Vec::new());
    assert_eq!(
        TurnViewReconciler::persisted_for_test(target(channel, b)),
        None
    );
}

/// A cancel while the early reaction is in flight finds no record yet; the custody check
/// after the note clears it.
#[tokio::test]
async fn a_cancel_during_the_early_note_leaves_no_hourglass() {
    let _root = TestRuntimeRootGuard::new();
    let (channel, message) = (838_141, 100_000_000_838_141);
    let shared = ready_owner(channel).await;
    let _probe = WakeProbe::install(&shared, channel, message);
    let (pause, reached, release) = seam();
    let accepting = {
        let shared = shared.clone();
        tokio::spawn(BEFORE_EARLY_NOTE.scope(pause, async move {
            accept(&shared, channel, message).await
        }))
    };
    await_within(reached, WAIT_CAP).await.unwrap();

    let cancelled = crate::services::discord::queue_io::mailbox_cancel_all_waiting(
        &shared,
        &ProviderKind::Claude,
        ChannelId::new(channel),
    )
    .await;
    assert_eq!(cancelled.queued.len(), 1);
    release.send(()).unwrap();
    assert_eq!(
        await_within(accepting, WAIT_CAP).await.unwrap(),
        Some(Intake::Accepted)
    );

    let view = ops(&shared, target(channel, message));
    assert_eq!(view.iter().filter(|op| op.0 == '⏳').count(), 2, "{view:?}");
    assert_eq!(
        view.iter().rev().find(|op| op.0 == '⏳'),
        Some(&('⏳', false))
    );
    assert!(
        queued(&shared, channel).await.is_empty(),
        "no front is left for a handoff"
    );
    assert_eq!(
        TurnViewReconciler::persisted_for_test(target(channel, message)),
        None
    );
}

/// A cancel after the note and its custody check is cleared by the queue exit itself.
#[tokio::test]
async fn a_cancel_after_the_early_note_is_cleared_by_the_queue_exit() {
    let _root = TestRuntimeRootGuard::new();
    let (channel, message) = (838_151, 100_000_000_838_151);
    let shared = ready_owner(channel).await;
    let _probe = WakeProbe::install(&shared, channel, message);
    let (pause, reached, release) = seam();
    let accepting = {
        let shared = shared.clone();
        tokio::spawn(AFTER_EARLY_NOTE.scope(pause, async move {
            accept(&shared, channel, message).await
        }))
    };
    await_within(reached, WAIT_CAP).await.unwrap();
    assert_eq!(ops(&shared, target(channel, message)), vec![('⏳', true)]);

    crate::services::discord::queue_io::mailbox_cancel_all_waiting(
        &shared,
        &ProviderKind::Claude,
        ChannelId::new(channel),
    )
    .await;
    release.send(()).unwrap();
    await_within(accepting, WAIT_CAP).await.unwrap();

    let view = ops(&shared, target(channel, message));
    assert_eq!(&view[..2], &[('⏳', true), ('⏳', false)], "{view:?}");
    assert_eq!(
        TurnViewReconciler::persisted_for_test(target(channel, message)),
        None
    );
}

/// The real drive's handoff settles a front that ends without a start and stays queued.
#[tokio::test]
async fn the_real_handoff_rolls_a_waiting_front_back_to_mailbox() {
    let _root = TestRuntimeRootGuard::new();
    let (channel, message) = (838_161, 100_000_000_838_161);
    let shared = ready_owner(channel).await;
    let _probe = WakeProbe::install(&shared, channel, message);
    assert_eq!(
        accept(&shared, channel, message).await,
        Some(Intake::Accepted)
    );
    let idle: Handoff = Arc::new(|_| Box::pin(async { Err("front waits".to_string()) }));

    let head = queued(&shared, channel).await.remove(0);
    let result = real_handoff(&shared, channel, idle)(front(head)).await;

    assert_eq!(result, Err("front waits".to_string()));
    assert_eq!(
        ops(&shared, target(channel, message)),
        vec![('⏳', true), ('⏳', false), ('📬', true)]
    );
}

/// The real drive's handoff clears a front that ends without a start and leaves the queue.
#[tokio::test]
async fn the_real_handoff_clears_a_front_that_left_the_queue() {
    let _root = TestRuntimeRootGuard::new();
    let (channel, message) = (838_171, 100_000_000_838_171);
    let shared = ready_owner(channel).await;
    let _probe = WakeProbe::install(&shared, channel, message);
    assert_eq!(
        accept(&shared, channel, message).await,
        Some(Intake::Accepted)
    );
    let taker = shared.clone();
    let idle: Handoff = Arc::new(move |_| {
        let shared = taker.clone();
        Box::pin(async move {
            take_all(&shared, channel).await;
            Ok(())
        })
    });

    let head = queued(&shared, channel).await.remove(0);
    let result = real_handoff(&shared, channel, idle)(front(head)).await;

    assert_eq!(result, Ok(()));
    assert_eq!(
        ops(&shared, target(channel, message)),
        vec![('⏳', true), ('⏳', false)]
    );
    assert_eq!(
        TurnViewReconciler::persisted_for_test(target(channel, message)),
        None
    );
}

/// An early ⏳ whose input left the queue before a restart cut its settle short is cleared
/// by the boot recovery with no handoff; an ordinary start's Pending in the channel stays.
#[tokio::test]
async fn boot_recovery_clears_an_early_hourglass_whose_input_is_gone() {
    let _root = TestRuntimeRootGuard::new();
    let channel = 838_181;
    let (early, started) = (100_000_000_838_181, 100_000_000_838_182);
    let before = ready_owner(channel).await;
    let _probe = WakeProbe::install(&before, channel, early);
    assert_eq!(
        accept(&before, channel, early).await,
        Some(Intake::Accepted)
    );
    let http = Arc::new(poise::serenity_prelude::Http::new(""));
    crate::services::discord::turn_view_reconciler::note_intake_turn_started_current(
        &before,
        &http,
        ChannelId::new(channel),
        MessageId::new(started),
        "intake_start",
    )
    .await;
    // The input leaves before any handoff, then the process restarts.
    take_all(&before, channel).await;

    let after = make_shared_data_for_tests();
    crate::services::discord::router::message_handler::owner_recover(
        &after,
        ChannelId::new(channel),
    )
    .await;

    assert_eq!(ops(&after, target(channel, early)), vec![('⏳', false)]);
    assert_eq!(
        TurnViewReconciler::persisted_for_test(target(channel, early)),
        None
    );
    assert_eq!(ops(&after, target(channel, started)), Vec::new());
    assert_eq!(
        TurnViewReconciler::persisted_for_test(target(channel, started)),
        Some(("pending".to_string(), None))
    );
}
