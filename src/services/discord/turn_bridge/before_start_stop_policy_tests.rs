//! The policy close commits in one captured mailbox step: clear, release, debit, fence, witness.
//! Any other outcome keeps the turn held with its receipt row, its +1 and the queued input.
use super::*;
use crate::services::discord::turn_finalizer::{PolicyCloseIntent, PolicyCloseOutcome};
use crate::services::turn_orchestrator::policy_close::{
    PolicyClear, PolicyRetain, PolicyStage, test_hooks as step,
};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

type Seen = Rc<RefCell<Vec<&'static str>>>;

/// Records each stage the mailbox step reaches on this thread; panics once at `panic_at`.
fn stages(panic_at: Option<&'static str>) -> Seen {
    let seen = Seen::default();
    let (slot, fired) = (seen.clone(), Cell::new(false));
    step::observe(Some(Box::new(move |stage| {
        slot.borrow_mut().push(stage);
        if Some(stage) == panic_at && !fired.replace(true) {
            panic!("injected policy close panic at {stage}");
        }
    })));
    seen
}

/// A queued successor input: its id, payload and source ids must survive every close.
fn successor_input(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(3),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![MessageId::new(id), MessageId::new(id + 1)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("successor input {id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

type Queued = Vec<(MessageId, String, Vec<MessageId>)>;

impl Close {
    fn channel(&self) -> ChannelId {
        ChannelId::new(self.row.channel_id)
    }

    fn active(&self) -> usize {
        self.shared.restart.global_active.load(Ordering::Relaxed)
    }

    async fn slot(&self) -> Option<Arc<CancelToken>> {
        let mailbox = self.shared.mailbox_peek(self.channel()).unwrap();
        mailbox.snapshot().await.cancel_token
    }

    /// Whether the slot is still this close's own actor.
    async fn actor_holds(&self) -> bool {
        self.slot()
            .await
            .is_some_and(|slot| Arc::ptr_eq(&slot, &self.actor))
    }

    async fn queued(&self) -> Queued {
        let mailbox = self.shared.mailbox_peek(self.channel()).unwrap();
        let queue = mailbox.snapshot().await.intervention_queue;
        queue
            .into_iter()
            .map(|item| (item.message_id, item.text, item.source_message_ids))
            .collect()
    }

    /// Another live turn's +1 and one queued successor input, both of which a close must keep.
    async fn other_turn_and_queue(&self) -> Queued {
        discord::increment_global_active(&self.shared, "other_turn");
        let provider = self.shared.provider.clone();
        let input = successor_input(self.row.user_msg_id + 50);
        discord::mailbox_enqueue_intervention(&self.shared, &provider, self.channel(), input).await;
        let queued = self.queued().await;
        assert_eq!(queued.len(), 1);
        queued
    }

    /// Writes the receipt and acks a shown notice, as `consume` does before it submits.
    async fn acked(&self) -> InflightTurnState {
        let generation = self.shared.restart.current_generation;
        let admitted = self
            .row
            .admit_before_start_receipt(&self.proof.turn_nonce, self.proof.basis, generation)
            .await
            .unwrap();
        admitted
            .ack_before_start_notice(&shown().await)
            .await
            .unwrap()
    }

    /// The intent `consume` submits for `acked`, on the actor it captured.
    fn intent(&self, acked: InflightTurnState) -> PolicyCloseIntent {
        let mailbox = self.shared.mailbox_peek(self.channel()).unwrap();
        policy_intent(
            &self.shared,
            &self.actor,
            &self.row,
            &self.proof,
            mailbox,
            acked,
        )
        .unwrap()
    }

    async fn submit(&self, intent: PolicyCloseIntent) -> PolicyCloseOutcome {
        self.fin
            .submit_policy_close(intent, self.shared.clone())
            .await
    }

    /// Mailbox-release events published for this channel since `events` subscribed.
    fn released(
        &self,
        events: &mut tokio::sync::broadcast::Receiver<
            crate::services::discord::turn_completion_events::TurnCompletionEvent,
        >,
    ) -> usize {
        let mut count = 0;
        while let Ok(event) = events.try_recv() {
            count += usize::from(
                event.channel_id == self.channel()
                    && event.phase
                        == crate::services::discord::turn_completion_events::TurnCompletionPhase::MailboxReleased,
            );
        }
        count
    }

    fn events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<
        crate::services::discord::turn_completion_events::TurnCompletionEvent,
    > {
        crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
            &self.shared,
        )
    }
}

/// The held state: actor in the slot, the delivered receipt row's exact bytes, this turn's +1
/// beside the other turn's, and the successor input unchanged.
async fn assert_held(close: &Close, bytes: &Option<Vec<u8>>, queued: &Queued, case: &str) {
    assert!(
        close.actor_holds().await,
        "{case}: the actor keeps the slot"
    );
    assert_eq!(
        close.bytes(),
        *bytes,
        "{case}: the receipt row keeps its bytes"
    );
    assert_eq!(close.active(), 2, "{case}: nothing debited");
    assert_eq!(
        close.queued().await,
        *queued,
        "{case}: the successor input stays"
    );
}

/// The released state: no row, no slot, exactly this turn's debit, the successor input kept.
async fn assert_released(close: &Close, queued: &Queued, case: &str) {
    assert!(close.slot().await.is_none(), "{case}: the slot is free");
    assert!(close.durable().is_none(), "{case}: the receipt row is gone");
    assert_eq!(close.active(), 1, "{case}: exactly one debit");
    assert_eq!(
        close.queued().await,
        *queued,
        "{case}: the successor input stays"
    );
}

/// A shown notice whose durable ACK never lands keeps the receipt pending; a failed notice keeps
/// it pending too. Neither submits, clears, releases or debits.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_notice_or_durable_ack_failure_retains() {
    for (case, ack_fault) in [
        ("notice", None),
        ("ack write", Some(false)),
        ("ack rename", Some(true)),
    ] {
        let close = Close::open(5_340_971, |_| {}).await;
        let queued = close.other_turn_and_queue().await;
        let seen = stages(None);
        let submitted = test_hooks::submitted();
        if let Some(rename) = ack_fault {
            test_hooks::observe(Some(Box::new(move |step, row: &InflightTurnState| {
                if step == "after_receipt" {
                    InflightTurnState::arm_before_start_receipt_fault(row.channel_id, rename);
                }
            })));
        }
        let settled = if ack_fault.is_some() {
            close.consume(shown()).await
        } else {
            close
                .consume(async { Err("send failed".to_string()) })
                .await
        };
        test_hooks::observe(None);
        step::observe(None);
        assert!(!settled, "{case}");
        assert!(
            InflightTurnState::before_start_receipt_faults_consumed(close.row.channel_id),
            "{case}: an armed fault ran"
        );
        assert_eq!(close.receipt_pending(), Some(true), "{case}: still pending");
        assert_eq!(
            test_hooks::submitted(),
            submitted,
            "{case}: nothing submitted"
        );
        assert!(seen.borrow().is_empty(), "{case}: no mailbox step");
        assert!(close.actor_holds().await, "{case}");
        assert_eq!(close.active(), 2, "{case}");
        assert_eq!(close.queued().await, queued, "{case}");
    }
}

/// A finalizer that cannot accept the close owns nothing: the delivered row, token and +1 stay.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_send_failure_keeps_delivered_row() {
    let mut close = Close::open(5_340_972, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    // Built off any runtime, its actor never runs and its channel is closed.
    close.fin = std::thread::spawn(TurnFinalizer::spawn).join().unwrap();
    let seen = stages(None);
    let mut events = close.events();
    assert!(!close.consume(shown()).await);
    step::observe(None);
    assert_eq!(close.receipt_pending(), Some(false), "delivered");
    assert!(close.actor_holds().await);
    assert_eq!(close.active(), 2);
    assert_eq!(close.queued().await, queued);
    assert!(seen.borrow().is_empty());
    assert_eq!(close.released(&mut events), 0);
    let acked = close.durable().unwrap();
    assert!(matches!(
        close.submit(close.intent(acked)).await,
        PolicyCloseOutcome::Unconfirmed(PolicyStage::FinalizerSend)
    ));
}

/// An accepted close whose ACK is lost before it runs is `Unconfirmed`, never settled; nothing
/// ran, and the same close on a live finalizer is a first commit, not a duplicate.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_ack_pre_commit_unconfirmed() {
    let close = Close::open(5_340_973, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    let before = close.bytes();
    let dropping = TurnFinalizer::spawn_dropping_for_test();
    let outcome = dropping
        .submit_policy_close(close.intent(acked.clone()), close.shared.clone())
        .await;
    assert!(
        matches!(
            outcome,
            PolicyCloseOutcome::Unconfirmed(PolicyStage::FinalizerAck)
        ),
        "{outcome:?}"
    );
    assert_held(&close, &before, &queued, "ack before commit").await;
    let outcome = close.submit(close.intent(acked)).await;
    assert!(
        matches!(outcome, PolicyCloseOutcome::Settled),
        "{outcome:?}"
    );
    assert_released(&close, &queued, "first commit").await;
}

/// A commit whose ACK alone is lost released once; the same actor's witness confirms a re-ask with
/// no second debit or event, and never confirms another episode's key.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_ack_post_commit_exact_witness() {
    let close = Close::open(5_340_974, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    let mut events = close.events();
    {
        let submit = close.submit(close.intent(acked.clone()));
        tokio::pin!(submit);
        assert!(futures::poll!(submit.as_mut()).is_pending(), "accepted");
    }
    // The finalizer serves messages in order, so this answer follows the dropped close.
    let generation = close.shared.restart.current_generation;
    close
        .fin
        .has_live_watcher_pending(close.channel(), generation)
        .await;
    assert_released(&close, &queued, "ack lost after commit").await;
    assert_eq!(close.released(&mut events), 1);
    let again = close.submit(close.intent(acked.clone())).await;
    assert!(
        matches!(again, PolicyCloseOutcome::DuplicateConfirmed),
        "{again:?}"
    );
    let mut other = close.intent(acked);
    other.request.turn_nonce.push('x');
    let other = close.submit(other).await;
    assert!(matches!(other, PolicyCloseOutcome::Superseded), "{other:?}");
    assert_released(&close, &queued, "after the duplicate").await;
    assert_eq!(close.released(&mut events), 0, "no second release event");
}

/// Every refused clear releases nothing: a same-episode save, a moved birth pin, a replaced row,
/// a planned restart, a missing row, a lock file that cannot open and the pre-ACK row.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_clear_refusal_is_nonreleasing() {
    type Change = fn(&mut InflightTurnState);
    let cases: [(&str, Option<Change>, PolicyClear); 6] = [
        (
            "save generation",
            Some(|row| row.save_generation += 1),
            PolicyClear::Mismatch,
        ),
        (
            "birth pin",
            Some(|row| row.born_generation += 1),
            PolicyClear::Mismatch,
        ),
        (
            "replaced row",
            Some(|row| row.turn_nonce = Some("successor".into())),
            PolicyClear::Mismatch,
        ),
        (
            "planned restart",
            Some(|row| {
                row.restart_mode = Some(crate::services::discord::InflightRestartMode::DrainRestart)
            }),
            PolicyClear::Mismatch,
        ),
        ("missing row", None, PolicyClear::Missing),
        ("lock unopenable", None, PolicyClear::Io),
    ];
    for (case, change, expected) in cases {
        let close = Close::open(5_340_975, |_| {}).await;
        let queued = close.other_turn_and_queue().await;
        let acked = close.acked().await;
        let intent = close.intent(acked.clone());
        // The fresh row moves on disk after the ACK, written raw so no writer gate interferes.
        match (change, case) {
            (Some(change), _) => {
                let mut fresh = acked;
                change(&mut fresh);
                std::fs::write(close.path(), serde_json::to_vec(&fresh).unwrap()).unwrap();
            }
            (None, "missing row") => std::fs::remove_file(close.path()).unwrap(),
            (None, _) => {
                let lock = close.path().with_extension("json.lock");
                let _ = std::fs::remove_file(&lock);
                std::fs::create_dir_all(lock).unwrap();
            }
        }
        let bytes = close.bytes();
        let seen = stages(None);
        let outcome = close.submit(intent).await;
        step::observe(None);
        assert!(
            matches!(&outcome, PolicyCloseOutcome::Retained(PolicyRetain::Clear(clear)) if *clear == expected),
            "{case}: {outcome:?}"
        );
        assert_eq!(
            *seen.borrow(),
            ["before_claim", "before_clear", "clear_refused"],
            "{case}"
        );
        assert!(close.actor_holds().await, "{case}");
        assert_eq!(close.bytes(), bytes, "{case}: the row stays as it is");
        assert_eq!(close.active(), 2, "{case}");
        assert_eq!(close.queued().await, queued, "{case}");
    }
    // The row before its notice ACK is never the clear's target.
    let close = Close::open(5_340_976, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    let generation = close.shared.restart.current_generation;
    let admitted = close
        .row
        .admit_before_start_receipt(&close.proof.turn_nonce, close.proof.basis, generation)
        .await
        .unwrap();
    let before = close.bytes();
    let outcome = close.submit(close.intent(admitted)).await;
    assert!(
        matches!(
            outcome,
            PolicyCloseOutcome::Retained(PolicyRetain::Clear(PolicyClear::Mismatch))
        ),
        "{outcome:?}"
    );
    assert_held(&close, &before, &queued, "pre-ACK row").await;
}

/// A row lock held by an episode guard elsewhere never blocks the mailbox: the close answers
/// `LockBusy` within a deadline, a snapshot queued behind it answers too, and nothing changed.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_lock_busy_snapshot_remains_responsive() {
    let close = Close::open(5_340_977, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    let before = close.bytes();
    let deadline = std::time::Duration::from_secs(2);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (provider, channel, pin) = (
        close.shared.provider.clone(),
        close.row.channel_id,
        discord::inflight::InflightEpisodePin::from_state(&acked),
    );
    let holder = std::thread::spawn(move || {
        let guard = discord::inflight::lock_inflight_episode(&provider, channel, &pin).unwrap();
        held_tx.send(()).unwrap();
        // Released by the test, or on its own past the deadline so a blocking clear cannot hang.
        let _ = release_rx.recv_timeout(deadline * 2);
        drop(guard);
    });
    held_rx.recv().unwrap();
    // The snapshot request is queued while the close's step is running, so it waits behind it.
    let snapshot = Rc::new(RefCell::new(None));
    let (slot, mailbox) = (
        snapshot.clone(),
        close.shared.mailbox_peek(close.channel()).unwrap(),
    );
    step::observe(Some(Box::new(move |stage| {
        if stage == "before_clear" {
            let mailbox = mailbox.clone();
            *slot.borrow_mut() = Some(queue_now(async move { mailbox.snapshot().await }));
        }
    })));
    let started = std::time::Instant::now();
    let outcome = close.submit(close.intent(acked.clone())).await;
    let snapshot = snapshot.borrow_mut().take();
    let snapshot = snapshot.expect("queued behind the close").await;
    let elapsed = started.elapsed();
    step::observe(None);
    // The holder may already have let go on its own deadline if the clear waited for it.
    let _ = release_tx.send(());
    holder.join().unwrap();
    assert!(
        elapsed < deadline,
        "the step never waits on the row lock: {elapsed:?}"
    );
    assert!(
        matches!(
            outcome,
            PolicyCloseOutcome::Retained(PolicyRetain::Clear(PolicyClear::LockBusy))
        ),
        "{outcome:?}"
    );
    assert!(
        snapshot
            .cancel_token
            .is_some_and(|t| Arc::ptr_eq(&t, &close.actor))
    );
    assert_held(&close, &before, &queued, "lock busy").await;
    let outcome = close.submit(close.intent(acked)).await;
    assert!(
        matches!(outcome, PolicyCloseOutcome::Settled),
        "{outcome:?}"
    );
    assert_released(&close, &queued, "after the lock frees").await;
}

/// The same close asked again after its commit is confirmed by its witness: a successor started
/// since keeps its slot, row and +1, and nothing is debited or announced again.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_duplicate_exact_commit() {
    let close = Close::open(5_340_978, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    assert!(matches!(
        close.submit(close.intent(acked.clone())).await,
        PolicyCloseOutcome::Settled
    ));
    let successor = Arc::new(CancelToken::new());
    assert!(
        discord::mailbox_try_start_turn_kinded(
            &close.shared,
            close.channel(),
            successor.clone(),
            UserId::new(1),
            MessageId::new(close.row.user_msg_id + 7),
            crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
        )
        .await
    );
    discord::increment_global_active(&close.shared, "successor");
    let mut row = close.row.clone();
    row.user_msg_id += 7;
    row.turn_nonce = successor.turn_nonce().map(str::to_owned);
    discord::inflight::save_inflight_state(&row).unwrap();
    let bytes = close.bytes();
    let mut events = close.events();
    let again = close.submit(close.intent(acked)).await;
    assert!(
        matches!(again, PolicyCloseOutcome::DuplicateConfirmed),
        "{again:?}"
    );
    assert!(
        close
            .slot()
            .await
            .is_some_and(|slot| Arc::ptr_eq(&slot, &successor))
    );
    assert_eq!(close.bytes(), bytes, "the successor's row stays");
    assert_eq!(close.active(), 2, "the successor's +1 stays");
    assert_eq!(close.queued().await, queued, "the queued input stays");
    assert_eq!(close.released(&mut events), 0);
    assert!(!successor.cancelled.load(Ordering::Relaxed));
}

/// A finalized ledger entry is not a release: when the exact turn still holds its slot and its
/// delivered row, the close commits through its own step; a refused clear leaves it held.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_duplicate_retained_row_repair() {
    for refuse in [true, false] {
        let close = Close::open(5_340_979, |_| {}).await;
        let queued = close.other_turn_and_queue().await;
        // An ordinary terminal for this key while another episode holds the slot finalizes the
        // ledger without releasing anything; the exact turn then holds the slot again.
        let channel = close.channel();
        let message = MessageId::new(close.row.user_msg_id);
        let provider = close.shared.provider.clone();
        discord::mailbox_finish_turn(&close.shared, &provider, channel).await;
        let other = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
            "other-episode".into(),
        )));
        let mailbox = close.shared.mailbox(channel);
        mailbox
            .restore_active_turn(other, UserId::new(1), message)
            .await;
        let generation = close.shared.restart.current_generation;
        let key = TurnKey::new(channel, message.get(), generation)
            .with_episode_nonce(close.row.turn_nonce.as_deref());
        let ordinary = close
            .fin
            .submit_terminal(
                key,
                provider.clone(),
                crate::services::discord::turn_finalizer::TerminalEvent::Cancel,
                crate::services::discord::turn_finalizer::FinalizeContext::bridge(),
                close.shared.clone(),
            )
            .await;
        assert!(matches!(
            ordinary,
            crate::services::discord::turn_finalizer::FinalizeOutcome::AlreadyFinalized
        ));
        discord::mailbox_finish_turn(&close.shared, &provider, channel).await;
        mailbox
            .restore_active_turn(close.actor.clone(), UserId::new(1), message)
            .await;
        assert!(close.actor_holds().await);
        assert_eq!(close.active(), 2);
        let acked = close.acked().await;
        if refuse {
            let mut moved = acked.clone();
            moved.save_generation += 1;
            discord::inflight::save_inflight_state(&moved).unwrap();
        }
        let bytes = close.bytes();
        let outcome = close.submit(close.intent(acked)).await;
        if refuse {
            assert!(
                matches!(outcome, PolicyCloseOutcome::Retained(_)),
                "{outcome:?}"
            );
            assert_held(&close, &bytes, &queued, "refused repair").await;
        } else {
            assert!(
                matches!(outcome, PolicyCloseOutcome::Settled),
                "{outcome:?}"
            );
            assert_released(&close, &queued, "repair").await;
        }
    }
}

/// A successor in the slot, or the same message and nonce under another token, is not this
/// close's turn: nothing clears, and the successor keeps its slot, row and +1.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_successor_or_actor_mismatch() {
    for same_episode in [false, true] {
        let close = Close::open(5_340_980, |_| {}).await;
        let queued = close.other_turn_and_queue().await;
        let acked = close.acked().await;
        let intent = close.intent(acked);
        let (channel, provider) = (close.channel(), close.shared.provider.clone());
        discord::mailbox_finish_turn(&close.shared, &provider, channel).await;
        let (successor, message) = if same_episode {
            let nonce = close.row.turn_nonce.clone();
            (
                Arc::new(CancelToken::from_persisted_turn_nonce(nonce)),
                close.row.user_msg_id,
            )
        } else {
            (Arc::new(CancelToken::new()), close.row.user_msg_id + 3)
        };
        let mailbox = close.shared.mailbox(channel);
        mailbox
            .restore_active_turn(successor.clone(), UserId::new(1), MessageId::new(message))
            .await;
        let bytes = close.bytes();
        let seen = stages(None);
        let outcome = close.submit(intent).await;
        step::observe(None);
        assert!(
            matches!(outcome, PolicyCloseOutcome::Superseded),
            "{outcome:?}"
        );
        assert_eq!(*seen.borrow(), ["before_claim"], "no clear was tried");
        assert!(
            close
                .slot()
                .await
                .is_some_and(|slot| Arc::ptr_eq(&slot, &successor))
        );
        assert_eq!(close.bytes(), bytes);
        assert_eq!(close.active(), 2);
        assert_eq!(close.queued().await, queued);
    }
}

/// A panic before the clear commits nothing: the step contains it, the turn stays held, the
/// actor answers its next message, and a later close still commits.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_panic_before_clear() {
    for at in ["before_claim", "before_clear", "clear_refused"] {
        let close = Close::open(5_340_981, |_| {}).await;
        let queued = close.other_turn_and_queue().await;
        let acked = close.acked().await;
        let mut intent = close.intent(acked.clone());
        if at == "clear_refused" {
            intent.request.receipt = {
                let mut moved = acked.clone();
                moved.save_generation += 1;
                moved.into_policy_receipt(close.shared.restart.current_generation)
            };
        }
        let before = close.bytes();
        let _seen = stages(Some(at));
        let outcome = close.submit(intent).await;
        step::observe(None);
        assert!(
            matches!(
                outcome,
                PolicyCloseOutcome::Retained(PolicyRetain::Panicked)
            ),
            "{at}: {outcome:?}"
        );
        assert_held(&close, &before, &queued, at).await;
        let outcome = close.submit(close.intent(acked)).await;
        assert!(
            matches!(outcome, PolicyCloseOutcome::Settled),
            "{at}: {outcome:?}"
        );
        assert_released(&close, &queued, at).await;
    }
}

/// A panic anywhere after the clear still finishes the release, the one debit, the fence and
/// the witness before the actor reads on; the close settles and asking again is a duplicate.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_panic_after_clear_commits() {
    for at in ["after_clear", "after_detach", "after_fence", "after_commit"] {
        let close = Close::open(5_340_982, |_| {}).await;
        let queued = close.other_turn_and_queue().await;
        let acked = close.acked().await;
        let _seen = stages(Some(at));
        let outcome = close.submit(close.intent(acked.clone())).await;
        step::observe(None);
        assert!(
            matches!(outcome, PolicyCloseOutcome::Settled),
            "{at}: {outcome:?}"
        );
        assert_released(&close, &queued, at).await;
        let again = close.submit(close.intent(acked)).await;
        assert!(
            matches!(again, PolicyCloseOutcome::DuplicateConfirmed),
            "{at}: {again:?}"
        );
        assert_eq!(close.active(), 1, "{at}");
    }
}

/// A panic after the debit and before the fence still raises the fence: a recovery re-mint of
/// the closed episode is refused, the actor lives and the turn was debited exactly once.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_panic_after_debit_before_fence() {
    let close = Close::open(5_340_983, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    let _seen = stages(Some("after_debit"));
    let outcome = close.submit(close.intent(acked)).await;
    step::observe(None);
    assert!(
        matches!(outcome, PolicyCloseOutcome::Settled),
        "{outcome:?}"
    );
    assert_released(&close, &queued, "after debit").await;
    let mailbox = close.shared.mailbox_peek(close.channel()).unwrap();
    let remint = Arc::new(CancelToken::from_persisted_turn_nonce(
        close.row.turn_nonce.clone(),
    ));
    let provider = close.shared.provider.clone();
    let persistence = discord::queue_persistence_context(&close.shared, &provider, close.channel());
    let started = mailbox
        .try_start_turn_unless_released(
            remint,
            UserId::new(1),
            MessageId::new(close.row.user_msg_id),
            persistence,
        )
        .await;
    assert!(
        !started.started && started.refused_released_episode,
        "the fence refuses it"
    );
    assert!(close.slot().await.is_none());
    assert_eq!(close.active(), 1);
}

/// A panic in the finalizer's trailing cleanup after the commit keeps the commit: the turn is
/// released once, and asking again is a duplicate with no further debit or event.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_after_commit_feedback_panic() {
    let close = Close::open(5_340_984, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    let mut events = close.events();
    TurnFinalizer::arm_finalize_panic_for_test();
    let outcome = close.submit(close.intent(acked.clone())).await;
    assert!(
        matches!(
            outcome,
            PolicyCloseOutcome::Unconfirmed(PolicyStage::FinalizerPanic)
        ),
        "{outcome:?}"
    );
    assert_released(&close, &queued, "cleanup panic").await;
    let _ = close.released(&mut events);
    let again = close.submit(close.intent(acked)).await;
    assert!(
        matches!(again, PolicyCloseOutcome::DuplicateConfirmed),
        "{again:?}"
    );
    assert_released(&close, &queued, "after the duplicate").await;
    assert_eq!(close.released(&mut events), 0);
}

/// Polls `work` once so its mailbox request is queued now, keeping it to await later.
fn queue_now<T>(
    work: impl std::future::Future<Output = T> + 'static,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = T>>> {
    let mut work: std::pin::Pin<Box<dyn std::future::Future<Output = T>>> = Box::pin(work);
    let waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    assert!(
        work.as_mut().poll(&mut cx).is_pending(),
        "the request waits on the actor"
    );
    work
}

/// The mailbox actor serializes the close against an exact guarded finish: behind the close the
/// finish finds the slot free; ahead of it the finish releases and the close claims nothing.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_actor_serialization() {
    for close_first in [true, false] {
        let close = Close::open(5_340_985, |_| {}).await;
        let queued = close.other_turn_and_queue().await;
        let acked = close.acked().await;
        let intent = close.intent(acked);
        let (shared, actor) = (close.shared.clone(), close.actor.clone());
        let (channel, message) = (close.channel(), MessageId::new(close.row.user_msg_id));
        let nonce = close.row.turn_nonce.clone();
        let finish = move || {
            queue_now(async move {
                let provider = shared.provider.clone();
                discord::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion(
                    &shared, &provider, channel, message, nonce, std::time::Instant::now(), Some(actor),
                )
                .await
            })
        };
        let trace = Seen::default();
        let competitor = Rc::new(RefCell::new(None));
        if close_first {
            let (slot, trace) = (competitor.clone(), trace.clone());
            let mut finish = Some(finish);
            step::observe(Some(Box::new(move |stage| {
                trace.borrow_mut().push(stage);
                if stage == "before_clear" {
                    *slot.borrow_mut() = finish.take().map(|finish| finish());
                }
            })));
        } else {
            *competitor.borrow_mut() = Some(finish());
            let trace = trace.clone();
            step::observe(Some(Box::new(move |stage| trace.borrow_mut().push(stage))));
        }
        let bytes = close.bytes();
        let outcome = close.submit(intent).await;
        step::observe(None);
        let finished = competitor.borrow_mut().take();
        let finished = finished.unwrap().await;
        if close_first {
            assert!(
                matches!(outcome, PolicyCloseOutcome::Settled),
                "{outcome:?}"
            );
            assert!(
                finished.removed_token.is_none(),
                "the finish ran after the release"
            );
            assert_eq!(trace.borrow().last(), Some(&"after_commit"));
            assert_released(&close, &queued, "close first").await;
        } else {
            assert!(
                matches!(outcome, PolicyCloseOutcome::Superseded),
                "{outcome:?}"
            );
            assert!(
                finished
                    .removed_token
                    .is_some_and(|t| Arc::ptr_eq(&t, &close.actor))
            );
            assert_eq!(*trace.borrow(), ["before_claim"]);
            assert_eq!(close.bytes(), bytes, "the receipt row was not cleared");
            assert_eq!(
                close.active(),
                2,
                "the competitor's owner debits, not the close"
            );
        }
    }
}

/// Ordinary clears never take a raw receipt row, pending or delivered, whatever exact identity
/// they carry: only the policy close's own step may clear it.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_ordinary_identity_clear_preserves_receipt() {
    use discord::inflight::{GuardedClearOutcome, InflightTurnIdentity};
    let close = Close::open(5_340_986, |_| {}).await;
    let generation = close.shared.restart.current_generation;
    let admitted = close
        .row
        .admit_before_start_receipt(&close.proof.turn_nonce, close.proof.basis, generation)
        .await
        .unwrap();
    let pending = close.bytes();
    let acked = admitted
        .ack_before_start_notice(&shown().await)
        .await
        .unwrap();
    let delivered = close.bytes();
    for (row, bytes) in [(&admitted, &pending), (&acked, &delivered)] {
        std::fs::write(close.path(), bytes.as_ref().unwrap()).unwrap();
        let provider = &close.shared.provider;
        let channel = close.row.channel_id;
        let identity = InflightTurnIdentity::from_state(row);
        let outcomes = [
            discord::inflight::clear_inflight_state_for_captured_episode(
                provider,
                channel,
                &identity,
                row.turn_nonce.as_deref(),
            ),
            discord::inflight::clear_inflight_state_if_matches_identity_generation(
                provider,
                channel,
                &identity,
                row.effective_finalizer_turn_id(),
                &row.updated_at,
                row.save_generation,
            ),
            discord::inflight::clear_inflight_state_for_snapshot(provider, row),
        ];
        for outcome in outcomes {
            assert_ne!(outcome, GuardedClearOutcome::Cleared);
            assert_eq!(close.bytes(), *bytes, "the receipt row stays");
        }
    }
    assert!(matches!(
        close.submit(close.intent(acked)).await,
        PolicyCloseOutcome::Settled
    ));
    assert!(close.bytes().is_none(), "the policy step clears it");
}

/// The input fence refuses the close by type before any clear: closing gate, foreign permit, broken
/// population. On an open protected gate the clear stops at the lock its own scope holds.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_input_fence_typed_refusals() {
    use crate::services::discord::input_runtime::fence::{self, Failure, Gate, Mode};
    for (n, case) in ["closing", "stale permit", "persistence", "open gate"]
        .into_iter()
        .enumerate()
    {
        let close = Close::open(5_340_990 + n as u64, |_| {}).await;
        let queued = close.other_turn_and_queue().await;
        let acked = close.acked().await;
        let intent = close.intent(acked);
        let provider = close.shared.provider.clone();
        let gate = Gate::protect(provider.clone(), close.row.channel_id).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let mut permit = None;
        let _closing = match case {
            "closing" => Some(gate.close().unwrap()),
            "stale permit" => {
                let other = Gate::protect(provider.clone(), 5_340_994 + n as u64).unwrap();
                permit = Some(other.admit().unwrap());
                None
            }
            "persistence" => {
                let lock = close.path().with_extension("json.lock");
                std::fs::remove_file(&lock).unwrap();
                std::fs::create_dir_all(&lock).unwrap();
                None
            }
            _ => None,
        };
        let bytes = close.bytes();
        let seen = stages(None);
        let (fin, shared) = (close.fin.clone(), close.shared.clone());
        let work = async move { fin.submit_policy_close(intent, shared).await };
        let outcome = fence::effect::run(permit, work).await;
        step::observe(None);
        let expected = match case {
            "closing" => Some(Failure::Mode(Mode::Closing)),
            "stale permit" => Some(Failure::StalePermit),
            "persistence" => Some(Failure::Persistence),
            _ => None,
        };
        match (expected, &outcome) {
            (Some(failure), PolicyCloseOutcome::Retained(PolicyRetain::InputFence(seen))) => {
                assert_eq!(*seen, failure, "{case}")
            }
            (None, PolicyCloseOutcome::Retained(PolicyRetain::Clear(PolicyClear::Io))) => {}
            _ => panic!("{case}: {outcome:?}"),
        }
        // The fenced step runs on a blocking worker: this thread's seam sees none of it.
        assert!(seen.borrow().is_empty(), "{case}");
        assert!(close.actor_holds().await, "{case}");
        assert_eq!(close.bytes(), bytes, "{case}");
        assert_eq!(close.active(), 2, "{case}");
        assert_eq!(close.queued().await, queued, "{case}");
    }
}

/// Each exact-episode axis alone (cutoff, nonce, token, interrupt, message, process, receipt and
/// save generation, birth pin) commits nothing when it differs; the exact close commits.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_strict_boundaries() {
    type Tamper = fn(&Close, &InflightTurnState, &Axes, &mut PolicyCloseIntent);
    struct Axes {
        early: std::time::Instant,
        twin: Arc<CancelToken>,
    }
    let early = std::time::Instant::now();
    let close = Close::open(5_341_000, |_| {}).await;
    // A live twin with the same nonce; every token has its own interrupt generation too.
    let twin = CancelToken::from_persisted_turn_nonce(close.row.turn_nonce.clone());
    let axes = Axes {
        early,
        twin: Arc::new(twin),
    };
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    let tampers: [(&str, Tamper); 9] = [
        ("cutoff before start", |_, _, axes, i| {
            i.request.observed_before = axes.early
        }),
        ("nonce", |_, _, _, i| {
            i.request.turn_nonce = "other-nonce".into()
        }),
        ("token", |_, _, axes, i| {
            i.request.actor = Arc::downgrade(&axes.twin)
        }),
        ("interrupt generation", |_, _, _, i| {
            i.request.interrupt_generation += 1
        }),
        ("message", |_, _, _, i| {
            i.request.user_message_id = MessageId::new(i.request.user_message_id.get() + 1)
        }),
        ("process generation", |_, _, _, i| i.key.generation += 1),
        ("receipt generation", |close, acked, _, i| {
            let generation = close.shared.restart.current_generation + 1;
            i.request.receipt = acked.clone().into_policy_receipt(generation);
        }),
        ("birth pin", |close, acked, _, i| {
            let mut row = acked.clone();
            row.born_generation += 1;
            i.request.receipt = row.into_policy_receipt(close.shared.restart.current_generation);
        }),
        ("save generation", |close, acked, _, i| {
            let mut row = acked.clone();
            row.save_generation += 1;
            i.request.receipt = row.into_policy_receipt(close.shared.restart.current_generation);
        }),
    ];
    let bytes = close.bytes();
    for (case, tamper) in tampers {
        let mut intent = close.intent(acked.clone());
        tamper(&close, &acked, &axes, &mut intent);
        let outcome = close.submit(intent).await;
        assert!(
            matches!(
                outcome,
                PolicyCloseOutcome::Superseded
                    | PolicyCloseOutcome::Retained(PolicyRetain::Clear(PolicyClear::Mismatch))
            ),
            "{case}: {outcome:?}"
        );
        assert_held(&close, &bytes, &queued, case).await;
    }
    let outcome = close.submit(close.intent(acked)).await;
    assert!(
        matches!(outcome, PolicyCloseOutcome::Settled),
        "{outcome:?}"
    );
    assert_released(&close, &queued, "exact").await;
}

/// With settlement off on the step's thread the receipt clear refuses, so even a submitted close
/// releases nothing; a receipt that is not this close's acked one is never cleared either.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_off_and_legacy() {
    let close = Close::open(5_341_001, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    let bytes = close.bytes();
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    let outcome = close.submit(close.intent(acked.clone())).await;
    HERDR_SETTLEMENT_OVERRIDE.set(true);
    assert!(
        matches!(
            outcome,
            PolicyCloseOutcome::Retained(PolicyRetain::Clear(PolicyClear::Mismatch))
        ),
        "off: {outcome:?}"
    );
    assert_held(&close, &bytes, &queued, "off").await;
    // No receipt on the acked row, and an unknown receipt on disk.
    let mut bare = acked.clone();
    bare.herdr_before_start_stop = None;
    let mut intent = close.intent(acked.clone());
    intent.request.receipt = bare.into_policy_receipt(close.shared.restart.current_generation);
    let outcome = close.submit(intent).await;
    assert!(
        matches!(outcome, PolicyCloseOutcome::Retained(_)),
        "bare: {outcome:?}"
    );
    assert_held(&close, &bytes, &queued, "bare").await;
    let mut raw: serde_json::Value = serde_json::from_slice(bytes.as_ref().unwrap()).unwrap();
    raw["herdr_before_start_stop"]["protocol"] = serde_json::json!("unknown-protocol");
    let unknown = serde_json::to_vec(&raw).unwrap();
    std::fs::write(close.path(), &unknown).unwrap();
    let outcome = close.submit(close.intent(acked)).await;
    assert!(
        matches!(outcome, PolicyCloseOutcome::Retained(_)),
        "unknown: {outcome:?}"
    );
    assert_held(&close, &Some(unknown), &queued, "unknown").await;
}
