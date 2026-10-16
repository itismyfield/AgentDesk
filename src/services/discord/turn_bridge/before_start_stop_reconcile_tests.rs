//! A late policy close of an episode the finalizer already released exactly clears its receipt and
//! nothing else; without both the finalized ledger entry and the exact release fence it stays held.
use super::policy::armed;
use super::*;
use crate::services::discord::turn_finalizer::{
    FinalizeContext, FinalizeOutcome, PolicyCloseOutcome, TerminalEvent,
};
use crate::services::turn_orchestrator::policy_close::PolicyRetain;
use crate::services::turn_orchestrator::policy_close::test_hooks::Stage;
use std::sync::Mutex;

/// The ordinary exact terminal of the close's own episode, through the finalizer.
async fn finish_normally(shared: &Arc<SharedData>, fin: &TurnFinalizer, row: &InflightTurnState) {
    let channel = ChannelId::new(row.channel_id);
    let key = TurnKey::new(channel, row.user_msg_id, shared.restart.current_generation);
    let outcome = fin
        .submit_terminal_with_episode_nonce(
            key,
            shared.provider.clone(),
            TerminalEvent::Complete,
            FinalizeContext::bridge(),
            row.turn_nonce.clone(),
            shared.clone(),
        )
        .await;
    assert!(
        matches!(
            &outcome,
            FinalizeOutcome::Finalized {
                removed_token: Some(_),
                ..
            }
        ),
        "the exact terminal releases the episode"
    );
}

/// Starts a successor turn with its +1 in the close's channel.
async fn start_successor(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    message: u64,
) -> Arc<CancelToken> {
    let successor = Arc::new(CancelToken::new());
    assert!(
        discord::mailbox_try_start_turn_kinded(
            shared,
            channel,
            successor.clone(),
            UserId::new(1),
            MessageId::new(message),
            crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
        )
        .await
    );
    discord::increment_global_active(shared, "successor");
    successor
}

/// What a reconcile must leave alone: the slot owner, the count, the queue and the release events.
struct Untouched {
    slot: Option<Arc<CancelToken>>,
    active: usize,
    queued: Vec<(MessageId, String, Vec<MessageId>)>,
}

impl Untouched {
    async fn of(close: &Close) -> Self {
        Self {
            slot: close.slot().await,
            active: close.active(),
            queued: close.queued().await,
        }
    }

    async fn assert_same(&self, close: &Close, case: &str) {
        let slot = close.slot().await;
        assert_eq!(
            slot.as_ref().map(Arc::as_ptr),
            self.slot.as_ref().map(Arc::as_ptr),
            "{case}: the slot owner stays"
        );
        assert!(
            slot.is_none_or(|token| !token.cancelled.load(Ordering::Relaxed)),
            "{case}: the successor is not cancelled"
        );
        assert_eq!(close.active(), self.active, "{case}: nothing debited");
        assert_eq!(close.queued().await, self.queued, "{case}: the queue stays");
    }
}

/// The normal exact terminal wins the race while the bridge is about to submit its close: the
/// late close clears only the delivered receipt, and the successor, count and queue stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s2cd_normal_terminal_first_reconciles_receipt_only() {
    let close = Close::open(5_341_001, |_| {}).await;
    close.other_turn_and_queue().await;
    let mut events = close.events();
    let raced = Arc::new(Mutex::new(None));
    let (shared, fin, row) = (close.shared.clone(), close.fin.clone(), close.row.clone());
    let (handle, slot) = (tokio::runtime::Handle::current(), raced.clone());
    test_hooks::observe(Some(Box::new(move |step, _| {
        if step != "before_submit" {
            return;
        }
        let (shared, fin, row, handle) = (shared.clone(), fin.clone(), row.clone(), handle.clone());
        // The terminal and the successor run on another thread while the bridge waits here.
        let raced = std::thread::spawn(move || {
            handle.block_on(async move {
                finish_normally(&shared, &fin, &row).await;
                let channel = ChannelId::new(row.channel_id);
                let successor = start_successor(&shared, channel, row.user_msg_id + 9).await;
                let mailbox = shared.mailbox_peek(channel).unwrap();
                let queue = mailbox.snapshot().await.intervention_queue.len();
                let active = shared.restart.global_active.load(Ordering::Relaxed);
                (successor, queue, active)
            })
        })
        .join()
        .unwrap();
        *slot.lock().unwrap() = Some(raced);
    })));
    let seen = armed();
    let settled = close.consume(shown()).await;
    test_hooks::observe(None);
    let raced = raced.lock().unwrap().take();
    let (successor, queue, active) = raced.expect("the terminal raced the submit");
    assert!(settled, "the late close confirms the release");
    assert!(
        close.durable().is_none(),
        "the delivered receipt is cleared"
    );
    let stages = seen.lock().unwrap().clone();
    assert_eq!(
        stages,
        [Stage::BeforeClaim],
        "no commit stage and no finalize tail"
    );
    let after = Untouched::of(&close).await;
    assert!(
        after
            .slot
            .is_some_and(|slot| Arc::ptr_eq(&slot, &successor))
    );
    assert!(!successor.cancelled.load(Ordering::Relaxed));
    assert_eq!(close.active(), active, "nothing debited after the terminal");
    assert_eq!(
        after.queued.len(),
        queue,
        "the queue stays as the terminal left it"
    );
    assert_eq!(
        close.released(&mut events),
        1,
        "only the terminal's own release"
    );
}

/// A's completed close was displaced from the one-slot witness by B's: A's stale exact request
/// is confirmed from the finalized ledger and fence without re-running A's tail.
#[tokio::test(flavor = "current_thread")]
async fn s2cd_displaced_witness_reconciles_without_tail() {
    let close = Close::open(5_341_002, |_| {}).await;
    let acked = close.acked().await;
    let (first, stale) = (close.intent(acked.clone()), close.intent(acked));
    assert!(matches!(
        close.submit(first).await,
        PolicyCloseOutcome::Settled
    ));
    // B, the next herdr episode on the same channel, settles its own close through the bridge.
    let channel = close.channel();
    let (b_actor, owner) = prepared(ProviderKind::Claude, channel.get());
    let mut b_row = close.row.clone();
    b_row.user_msg_id += 4;
    b_row.current_msg_id += 4;
    b_row.turn_nonce = b_actor.turn_nonce().map(str::to_owned);
    let b_message = MessageId::new(b_row.user_msg_id);
    assert!(
        discord::mailbox_try_start_turn_kinded(
            &close.shared,
            channel,
            b_actor.clone(),
            UserId::new(1),
            b_message,
            crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
        )
        .await
    );
    discord::increment_global_active(&close.shared, "episode_b");
    discord::inflight::save_inflight_state(&b_row).unwrap();
    actual_fresh_exit(b_actor.clone(), owner, ProviderKind::Claude, true).await;
    let ExitDecision::PolicyClose(b_proof) = seal_exit(&b_actor, false, false) else {
        panic!("positive proof required");
    };
    let generation = close.shared.restart.current_generation;
    let b_key = TurnKey::new(channel, b_row.user_msg_id, generation)
        .with_episode_nonce(b_row.turn_nonce.as_deref());
    close.fin.register_start(
        b_key,
        ProviderKind::Claude,
        RelayOwnerKind::None,
        &close.shared,
    );
    let b_anchor = MessageId::new(b_row.current_msg_id);
    let b_close = (&b_proof, b_anchor);
    assert!(
        consume(
            &close.shared,
            &close.fin,
            &b_actor,
            &b_row,
            b_close,
            shown()
        )
        .await
    );
    let successor = start_successor(&close.shared, channel, b_row.user_msg_id + 4).await;
    let queued = close.other_turn_and_queue().await;
    let before = Untouched::of(&close).await;
    let mut events = close.events();
    let probe = stale.request.trace.probe();
    let again = close.submit(stale).await;
    assert!(
        matches!(again, PolicyCloseOutcome::DuplicateConfirmed),
        "{again:?}"
    );
    assert_eq!(
        probe.stages(),
        [Stage::BeforeClaim],
        "no commit stage and no tail"
    );
    assert!(close.durable().is_none(), "no receipt is left");
    before.assert_same(&close, "displaced witness").await;
    assert_eq!(close.queued().await, queued);
    assert!(!successor.cancelled.load(Ordering::Relaxed));
    assert_eq!(close.released(&mut events), 0, "no second release or tail");
}

/// The actor that released A exactly was purged and a successor actor serves the channel: A's
/// close on the old captured handle clears the receipt through the shared fence, and only then.
#[tokio::test(flavor = "current_thread")]
async fn s2cd_purged_actor_reconciles_only_with_ledger_and_fence() {
    // Only the finalizer that finalized A, after A's exact release, reconciles.
    for (same_finalizer, exact_release) in [(true, true), (false, true), (true, false)] {
        let close = Close::open(5_341_003, |_| {}).await;
        let acked = close.acked().await;
        let stale = close.intent(acked);
        let (channel, provider) = (close.channel(), close.shared.provider.clone());
        if !exact_release {
            // A channel-wide finish names no episode, so the terminal after it fences nothing.
            discord::mailbox_finish_turn(&close.shared, &provider, channel).await;
            let key = TurnKey::new(
                channel,
                close.row.user_msg_id,
                close.shared.restart.current_generation,
            )
            .with_episode_nonce(close.row.turn_nonce.as_deref());
            let finalized = close
                .fin
                .submit_terminal(
                    key,
                    provider.clone(),
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    close.shared.clone(),
                )
                .await;
            assert!(matches!(
                finalized,
                FinalizeOutcome::Finalized {
                    removed_token: None,
                    ..
                }
            ));
        } else {
            finish_normally(&close.shared, &close.fin, &close.row).await;
        }
        assert!(matches!(
            close.shared.mailboxes.remove_idle_entry(channel).await,
            crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome::Removed
        ));
        let successor = start_successor(&close.shared, channel, close.row.user_msg_id + 5).await;
        close.other_turn_and_queue().await;
        let before = Untouched::of(&close).await;
        let bytes = close.bytes();
        let mut events = close.events();
        // A finalizer other than the one that finalized A holds no ledger entry for it.
        let fin = if same_finalizer {
            close.fin.clone()
        } else {
            TurnFinalizer::spawn()
        };
        let outcome = fin.submit_policy_close(stale, close.shared.clone()).await;
        let case = format!("same_finalizer={same_finalizer} exact_release={exact_release}");
        if same_finalizer && exact_release {
            assert!(
                matches!(outcome, PolicyCloseOutcome::DuplicateConfirmed),
                "{case}: {outcome:?}"
            );
            assert!(close.durable().is_none(), "{case}: the receipt is cleared");
        } else {
            assert!(
                matches!(
                    outcome,
                    PolicyCloseOutcome::Retained(PolicyRetain::ActorClosed)
                ),
                "{case}: {outcome:?}"
            );
            assert_eq!(
                close.bytes(),
                bytes,
                "{case}: the receipt row keeps its bytes"
            );
            assert!(
                close.receipt_pending().is_some(),
                "{case}: the receipt stays"
            );
        }
        before.assert_same(&close, &case).await;
        assert!(Arc::ptr_eq(before.slot.as_ref().unwrap(), &successor));
        assert_eq!(close.released(&mut events), 0, "{case}: no release");
    }
}

/// Same-process reconciliation lasts only while the finalized ledger entry does: past its TTL the
/// late close keeps the receipt, the count and the queue.
#[tokio::test(flavor = "current_thread")]
async fn s2cd_reconcile_window_ends_with_the_finalized_entry() {
    for (aged, reconciled) in [(9 * 60, true), (10 * 60 + 5, false)] {
        let close = Close::open(5_341_004, |_| {}).await;
        let acked = close.acked().await;
        let stale = close.intent(acked);
        finish_normally(&close.shared, &close.fin, &close.row).await;
        close.other_turn_and_queue().await;
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(aged)).await;
        // A few reconcile ticks run the ledger GC at the aged clock.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        tokio::time::resume();
        let before = Untouched::of(&close).await;
        let bytes = close.bytes();
        let outcome = close.submit(stale).await;
        if reconciled {
            assert!(
                matches!(outcome, PolicyCloseOutcome::DuplicateConfirmed),
                "{outcome:?}"
            );
            assert!(close.durable().is_none());
        } else {
            assert!(
                matches!(outcome, PolicyCloseOutcome::Superseded),
                "{outcome:?}"
            );
            assert_eq!(close.bytes(), bytes, "the receipt row keeps its bytes");
            assert!(close.receipt_pending().is_some());
        }
        before.assert_same(&close, &format!("aged {aged}s")).await;
    }
}

/// A tmux turn has no Herdr stop state: its bridge never submits a close, before or after its
/// normal terminal, and its row is never touched by one.
#[tokio::test(flavor = "current_thread")]
async fn s2cd_tmux_turn_never_reaches_the_reconcile() {
    let close = Close::open(5_341_005, |_| {}).await;
    let tmux = Arc::new(CancelToken::from_persisted_turn_nonce(
        close.row.turn_nonce.clone(),
    ));
    let submitted = test_hooks::submitted();
    let bytes = close.bytes();
    let tmux_close = (&close.proof, close.anchor());
    assert!(
        !consume(
            &close.shared,
            &close.fin,
            &tmux,
            &close.row,
            tmux_close,
            shown()
        )
        .await
    );
    finish_normally(&close.shared, &close.fin, &close.row).await;
    let tmux_close = (&close.proof, close.anchor());
    assert!(
        !consume(
            &close.shared,
            &close.fin,
            &tmux,
            &close.row,
            tmux_close,
            shown()
        )
        .await
    );
    assert_eq!(test_hooks::submitted(), submitted, "no policy submission");
    assert_eq!(close.bytes(), bytes, "the row keeps its bytes");
}
