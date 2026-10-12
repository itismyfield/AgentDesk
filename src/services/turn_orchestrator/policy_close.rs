//! The mailbox half of a before-start policy close: one actor step claims the exact episode, clears
//! its receipt without waiting, then releases, debits, fences and witnesses before reading on.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use poise::serenity_prelude::{ChannelId, MessageId};

use super::{ChannelMailboxState, FinishTurnResult, InterventionMode, QueuePersistenceContext};
use crate::services::discord::input_runtime::fence::Failure;
use crate::services::provider::CancelToken;

/// The close's one durable effect: the non-blocking exact clear of its acked receipt row.
pub(crate) trait PolicyReceipt: Send {
    fn channel(&self) -> u64;
    fn try_clear(&self) -> PolicyClear;
}

/// How the receipt clear ended; only `Cleared` lets the step release anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyClear {
    Cleared,
    LockBusy,
    Missing,
    Mismatch,
    Io,
    Skipped,
}

/// Why a close released nothing; the turn stays held with its receipt.
#[derive(Debug)]
pub(crate) enum PolicyRetain {
    Clear(PolicyClear),
    InputFence(Failure),
    ActorClosed,
    Panicked,
}

/// Where a close lost sight of its result; nothing after that point is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyStage {
    FinalizerSend,
    FinalizerAck,
    MailboxSend,
    MailboxReply,
    FinalizerPanic,
}

/// The step's answer. `Committed` carries the removed token and its debit is already done.
pub(crate) enum PolicyMailboxOutcome {
    Committed(FinishTurnResult),
    Duplicate,
    Superseded,
    Retained(PolicyRetain),
}

pub(crate) struct PolicyCloseRequest {
    pub(crate) actor: Weak<CancelToken>,
    pub(crate) user_message_id: MessageId,
    pub(crate) turn_nonce: String,
    pub(crate) interrupt_generation: u64,
    pub(crate) observed_before: Instant,
    pub(crate) receipt: Box<dyn PolicyReceipt>,
    pub(crate) active: Arc<AtomicUsize>,
    pub(crate) persistence: QueuePersistenceContext,
}

/// The latest close this actor incarnation committed; one entry, so it never grows.
pub(super) struct PolicyWitness {
    user_message_id: MessageId,
    turn_nonce: String,
    actor: Weak<CancelToken>,
}

impl PolicyCloseRequest {
    fn witnessed(&self, witness: Option<&PolicyWitness>) -> bool {
        witness.is_some_and(|seen| {
            seen.user_message_id == self.user_message_id
                && seen.turn_nonce == self.turn_nonce
                && Weak::ptr_eq(&seen.actor, &self.actor)
        })
    }

    /// The exact episode still holds the slot: its token, message, nonce and interrupt
    /// generation, started before the bridge observed it.
    fn claims(&self, state: &ChannelMailboxState, channel_id: ChannelId) -> bool {
        let Some(actor) = self.actor.upgrade() else {
            return false;
        };
        !self.turn_nonce.is_empty()
            && self.receipt.channel() == channel_id.get()
            && state
                .cancel_token
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &actor))
            && state.active_user_message_id == Some(self.user_message_id)
            && state.active_turn_nonce.as_deref() == Some(self.turn_nonce.as_str())
            && actor.claude_interrupt_generation() == self.interrupt_generation
            && state
                .turn_started_instant
                .is_some_and(|started| started < self.observed_before)
    }
}

/// Runs one close inside the actor. A panic stops here; one after the receipt clear still
/// answers `Committed`, because the completion guard finished the release while unwinding.
pub(super) fn step(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    request: PolicyCloseRequest,
) -> PolicyMailboxOutcome {
    let mut removed = None;
    let run = std::panic::catch_unwind(AssertUnwindSafe(|| {
        close(state, channel_id, &request, &mut removed)
    }));
    match (run, removed) {
        (Ok(outcome), _) => outcome,
        (Err(_), Some(token)) => {
            super::mark_turn_finished_signal_done(channel_id);
            PolicyMailboxOutcome::Committed(FinishTurnResult {
                removed_token: Some(token),
                has_pending: state
                    .intervention_queue
                    .iter()
                    .any(|item| item.mode == InterventionMode::Soft),
                mailbox_online: true,
                queue_exit_events: Vec::new(),
                persistence_error: Some("policy close panicked after its commit".into()),
            })
        }
        (Err(_), None) => PolicyMailboxOutcome::Retained(PolicyRetain::Panicked),
    }
}

fn close(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    request: &PolicyCloseRequest,
    removed: &mut Option<Arc<CancelToken>>,
) -> PolicyMailboxOutcome {
    if request.witnessed(state.policy_witness.as_ref()) {
        return PolicyMailboxOutcome::Duplicate;
    }
    #[cfg(test)]
    test_hooks::note_started(state.turn_started_instant);
    at("before_claim");
    if !request.claims(state, channel_id) {
        return PolicyMailboxOutcome::Superseded;
    }
    // Built before the clear, so after it the guard only moves this value into the state.
    let witness = PolicyWitness {
        user_message_id: request.user_message_id,
        turn_nonce: request.turn_nonce.clone(),
        actor: request.actor.clone(),
    };
    at("before_clear");
    let cleared = request.receipt.try_clear();
    if cleared != PolicyClear::Cleared {
        at("clear_refused");
        return PolicyMailboxOutcome::Retained(PolicyRetain::Clear(cleared));
    }
    let mut commit = Commit {
        state: &mut *state,
        removed: &mut *removed,
        request,
        witness: Some(witness),
        held: None,
        detached: false,
        debited: false,
        fenced: false,
    };
    at("after_clear");
    commit.detach();
    at("after_detach");
    commit.debit();
    at("after_debit");
    commit.fence();
    at("after_fence");
    commit.witness();
    let held = commit.held;
    drop(commit);
    // After the commit: the queue, marker and latch work a matching finish does.
    super::pause_inbound_stall_for_turn(state, held);
    state.last_persistence = Some(request.persistence.clone());
    at("after_commit");
    let mut finish =
        super::finalize_turn_state(state, channel_id, Some(&request.persistence), false);
    finish.removed_token = removed.clone();
    super::consume_pending_dispatch_marker_if_matches(
        state,
        channel_id,
        request.user_message_id,
        "policy_close",
    );
    super::mark_turn_finished_signal_done(channel_id);
    PolicyMailboxOutcome::Committed(finish)
}

/// Armed once the receipt row is gone: whichever stage a panic interrupts, dropping it finishes
/// the release, the one debit, the remint fence and the witness before the actor reads on.
struct Commit<'a> {
    state: &'a mut ChannelMailboxState,
    removed: &'a mut Option<Arc<CancelToken>>,
    request: &'a PolicyCloseRequest,
    witness: Option<PolicyWitness>,
    held: Option<Instant>,
    detached: bool,
    debited: bool,
    fenced: bool,
}

impl Commit<'_> {
    fn detach(&mut self) {
        if !std::mem::replace(&mut self.detached, true) {
            (*self.removed, self.held) =
                super::lease_release::detach_active_turn_anchor(self.state);
        }
    }

    fn debit(&mut self) {
        if !std::mem::replace(&mut self.debited, true) {
            let _ = self
                .request
                .active
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
        }
    }

    fn fence(&mut self) {
        if !std::mem::replace(&mut self.fenced, true) {
            let request = self.request;
            (self.state.remint_fence)
                .note_exact_release(request.user_message_id, &request.turn_nonce);
        }
    }

    fn witness(&mut self) {
        if let Some(witness) = self.witness.take() {
            self.state.policy_witness = Some(witness);
        }
    }
}

impl Drop for Commit<'_> {
    fn drop(&mut self) {
        self.detach();
        self.debit();
        self.fence();
        self.witness();
    }
}

/// Test seam at each stage of the close; nothing outside tests.
fn at(_stage: &'static str) {
    #[cfg(test)]
    test_hooks::fire(_stage);
}

#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::{Cell, RefCell};
    use std::time::Instant;

    type Observer = Box<dyn FnMut(&'static str)>;

    thread_local! {
        static OBSERVER: RefCell<Option<Observer>> = const { RefCell::new(None) };
        static STARTED: Cell<Option<Instant>> = const { Cell::new(None) };
    }

    /// The slot's start instant the last close on this thread compared its cutoff with.
    pub(crate) fn started() -> Option<Instant> {
        STARTED.get()
    }

    pub(super) fn note_started(started: Option<Instant>) {
        STARTED.set(started);
    }

    /// A captured handle whose actor is gone, so every send to it fails.
    pub(crate) fn closed_mailbox() -> super::super::ChannelMailboxHandle {
        super::super::mailbox_unreachable_tests::closed_handle()
    }

    /// Sees, and may panic at, every stage a close reaches on this thread.
    pub(crate) fn observe(observer: Option<Observer>) {
        OBSERVER.with(|slot| *slot.borrow_mut() = observer);
    }

    pub(super) fn fire(stage: &'static str) {
        OBSERVER.with(|slot| {
            if let Some(observer) = slot.borrow_mut().as_mut() {
                observer(stage);
            }
        });
    }
}
