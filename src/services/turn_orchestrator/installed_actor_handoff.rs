#![cfg(test)]
//! The installed actor's own checks before what its scoped claim owes is reserved: feedback and
//! driver reservations keep every exit, and only the driver's ready confirm activates recovery.

use super::scoped_claim::{InstalledPending, ScopedClaimOutcome, ScopedClaimRefusal, ScopedMsg};
use super::*;
use crate::services::cluster::channel_home_port::RestoreFailure;
use std::sync::atomic::AtomicU64;

pub(crate) use super::scoped_claim::{PendingDriver, PendingFeedback};

#[derive(Clone)]
pub(crate) struct Installation {
    pub(crate) channel: ChannelId,
    pub(crate) token: Arc<CancelToken>,
    pub(crate) episode: String,
    pub(crate) birth: String,
    pub(crate) owner: UserId,
    pub(crate) message: MessageId,
    pub(crate) generation: u64,
    pub(crate) current: Arc<AtomicU64>,
}

/// What a scoped install learned from its claim.
pub(crate) enum ClaimResult {
    /// The actor and its record were installed with their debts recorded; its driver is owed.
    Installed,
    /// The exact actor already holds its record; whether its driver is still owed.
    Inherited {
        owes_driver: bool,
    },
    /// The exact actor holds the slot without a scoped record; its existing driver stays.
    Observed,
    Refused(RestoreFailure),
}

/// What a handoff asks of the installed actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Intent {
    /// Reserve the owed exit feedback; the exits stay recorded until a durable acknowledgement.
    Feedback,
    /// Reserve the driver; nothing is activated before its ready confirm.
    Driver,
    /// The reserved driver is running; only this activates recovery.
    Ready,
}

/// One exit the actor still owes, cloned from the record that keeps it.
#[derive(Clone)]
pub(crate) struct OwedExit {
    pub(crate) event_id: String,
    pub(crate) episode: String,
    pub(crate) event: QueueExitEvent,
}

pub(crate) enum HandoffOutcome {
    /// Feedback or driver reserved; the record keeps every exit and the debts stay unacknowledged.
    Reserved(Vec<OwedExit>),
    /// The reserved driver is confirmed ready and recovery activated.
    Ready,
    /// The driver was already confirmed ready.
    AlreadyDriven,
    Refused(RestoreFailure),
}

type RowCheck = Box<dyn FnOnce() -> Result<Box<dyn Send>, String> + Send>;
pub(super) struct Request {
    proof: Installation,
    intent: Intent,
    check: RowCheck,
    reply: oneshot::Sender<HandoffOutcome>,
}
impl Request {
    pub(super) fn refuse(self, reason: RestoreFailure) {
        let _ = self.reply.send(HandoffOutcome::Refused(reason));
    }
}

/// A reserved driver is owed until its ready confirm.
fn owes_driver(driver: PendingDriver) -> bool {
    match driver {
        PendingDriver::NotScheduled => true,
        PendingDriver::Reserved => !mutant("pending-reentry"),
        PendingDriver::Ready => false,
    }
}

fn refusal(refusal: ScopedClaimRefusal) -> RestoreFailure {
    use ScopedClaimRefusal as R;
    let reason = format!("scoped claim refused: {refusal:?}");
    match refusal {
        R::ActorClosed
        | R::InputFence(_)
        | R::Adopt(_)
        | R::HeadBlocks
        | R::Yields
        | R::CallerLeft
        | R::NotCommitted(_) => RestoreFailure::retryable(reason),
        _ => RestoreFailure::Rejected(reason),
    }
}

impl ChannelMailboxHandle {
    /// Claims `proof`'s slot in one actor step; a refusal changed nothing.
    pub(crate) async fn claim_installation(
        &self,
        proof: Installation,
        persistence: QueuePersistenceContext,
    ) -> ClaimResult {
        match self.claim_scoped(proof, persistence).await {
            None => ClaimResult::Refused(RestoreFailure::retryable("claim actor unavailable")),
            Some(ScopedClaimOutcome::Installed(_)) => ClaimResult::Installed,
            Some(ScopedClaimOutcome::Inherited(driver)) => ClaimResult::Inherited {
                owes_driver: owes_driver(driver),
            },
            Some(ScopedClaimOutcome::Observed) => ClaimResult::Observed,
            Some(ScopedClaimOutcome::Refused(why)) => ClaimResult::Refused(refusal(why)),
        }
    }

    /// Whether `token` is the live actor of the scoped record of `birth`; an actor that cannot be
    /// asked is an error, never a no.
    pub(crate) async fn holds_installation(
        &self,
        token: &Arc<CancelToken>,
        birth: &str,
    ) -> Result<bool, MailboxUnreachable> {
        let build = |reply| ChannelMailboxMsg::ScopedClaim(ScopedMsg::Pending { reply });
        let pending: Option<InstalledPending> = self.request(build).await?;
        Ok(pending.is_some_and(|pending| {
            let live = pending.token.upgrade();
            pending.birth == birth && live.is_some_and(|live| Arc::ptr_eq(&live, token))
        }))
    }

    pub(crate) async fn handoff_installed(
        &self,
        proof: Installation,
        intent: Intent,
        check: impl FnOnce() -> Result<Box<dyn Send>, String> + Send + 'static,
    ) -> HandoffOutcome {
        self.request(|reply| {
            ChannelMailboxMsg::InstalledActorHandoff(Request {
                proof,
                intent,
                check: Box::new(check),
                reply,
            })
        })
        .await
        .unwrap_or_else(|_| {
            HandoffOutcome::Refused(RestoreFailure::retryable("handoff actor unavailable"))
        })
    }
}

pub(super) fn step(
    state: &mut ChannelMailboxState,
    channel: ChannelId,
    own_done: &Arc<RecoveryDoneSignal>,
    request: Request,
) {
    let Request {
        proof,
        intent,
        check,
        reply,
    } = request;
    if reply.is_closed() {
        return;
    }
    let outcome = handoff(state, channel, own_done, &proof, intent, check, &reply)
        .unwrap_or_else(|reason| HandoffOutcome::Refused(reason.into()));
    let _ = reply.send(outcome);
}

/// The record, slot and fresh row must all still be `proof`'s before anything is reserved.
fn handoff(
    state: &mut ChannelMailboxState,
    channel: ChannelId,
    own_done: &Arc<RecoveryDoneSignal>,
    proof: &Installation,
    intent: Intent,
    check: RowCheck,
    reply: &oneshot::Sender<HandoffOutcome>,
) -> Result<HandoffOutcome, String> {
    let generation = !mutant("handoff-generation");
    let pending = (state.installed_pending.as_ref()).ok_or("installation responsibility absent")?;
    let live = pending.token.upgrade();
    if channel != proof.channel
        || !live.is_some_and(|live| Arc::ptr_eq(&live, &proof.token))
        || (&pending.birth, &pending.episode) != (&proof.birth, &proof.episode)
        || (pending.owner, pending.message) != (proof.owner, proof.message)
        || (generation
            && (pending.generation != proof.generation
                || pending.current.load(Ordering::Acquire) != proof.generation))
        || (!mutant("handoff-token")
            && !(state.cancel_token.as_ref()).is_some_and(|token| Arc::ptr_eq(token, &proof.token)))
        || state.active_request_owner != Some(proof.owner)
        || state.active_user_message_id != Some(proof.message)
        || proof.token.cancelled.load(Ordering::Acquire)
    {
        return Err("handoff actor/episode/generation changed".into());
    }
    let _row = (!mutant("handoff-row")).then(check).transpose()?;
    if generation && pending.current.load(Ordering::Acquire) != proof.generation {
        return Err("handoff generation changed during row check".into());
    }
    if reply.is_closed() {
        return Err("handoff caller left before acceptance".into());
    }
    let pending = state.installed_pending.as_mut().expect("checked above");
    let owed = |pending: &InstalledPending| -> Vec<OwedExit> {
        let exits = pending.exits.iter();
        let owed = exits.map(|exit| OwedExit {
            event_id: exit.debt.event_id.clone(),
            episode: exit.debt.episode.clone(),
            event: exit.event.clone(),
        });
        owed.collect()
    };
    Ok(match (intent, pending.driver) {
        (Intent::Feedback | Intent::Driver, driver) if !owes_driver(driver) => {
            HandoffOutcome::AlreadyDriven
        }
        (Intent::Feedback, _) => {
            pending.feedback = PendingFeedback::Reserved;
            HandoffOutcome::Reserved(owed(pending))
        }
        (Intent::Driver, _) => {
            pending.driver = PendingDriver::Reserved;
            HandoffOutcome::Reserved(owed(pending))
        }
        (Intent::Ready, PendingDriver::NotScheduled) => {
            return Err("driver ready before its reservation".into());
        }
        (Intent::Ready, PendingDriver::Ready) => HandoffOutcome::AlreadyDriven,
        (Intent::Ready, PendingDriver::Reserved) => {
            pending.driver = PendingDriver::Ready;
            GLOBAL_RECOVERY_DONE_SIGNALS.insert(channel, own_done.clone());
            recovery_kickoff::reset_activation_signals(channel);
            state.recovery_started_at = Some(Instant::now());
            HandoffOutcome::Ready
        }
    })
}

/// The actor's record as a restore reads it; each exit is its event id, issuing episode and
/// message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwedRecord {
    pub(crate) actor: usize,
    pub(crate) episode: String,
    pub(crate) generation: u64,
    pub(crate) feedback: PendingFeedback,
    pub(crate) driver: PendingDriver,
    pub(crate) exits: Vec<(String, String, u64)>,
}

/// The canonical queue file as the snapshot reader parses it, debts written as in `OwedRecord`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RecordedQueue {
    pub(crate) versioned: bool,
    pub(crate) items: Vec<u64>,
    pub(crate) debts: Vec<(String, String, u64)>,
}

impl ChannelMailboxHandle {
    pub(crate) async fn owed_record(&self) -> Option<OwedRecord> {
        let pending = self.installed_pending().await?;
        let exits = pending.exits.iter().map(|exit| {
            let debt = &exit.debt;
            (
                debt.event_id.clone(),
                debt.episode.clone(),
                debt.intervention.message_id,
            )
        });
        Some(OwedRecord {
            actor: pending.token.as_ptr() as usize,
            episode: pending.episode,
            generation: pending.generation,
            feedback: pending.feedback,
            driver: pending.driver,
            exits: exits.collect(),
        })
    }
}

pub(crate) fn queue_file(
    persistence: &QueuePersistenceContext,
    channel: ChannelId,
) -> std::path::PathBuf {
    let (provider, token_hash) = (&persistence.provider, &persistence.token_hash);
    pending_queue_persistence::pending_queue_file_path(provider, token_hash, channel)
        .expect("pending queue root")
}

/// `None` for a missing file; one the reader refuses is an error.
pub(crate) fn recorded_queue(
    persistence: &QueuePersistenceContext,
    channel: ChannelId,
) -> Result<Option<RecordedQueue>, String> {
    let content = match std::fs::read_to_string(queue_file(persistence, channel)) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let (items, meta) = queued_head::parse_snapshot(&content)?;
    let debts = meta.map(|meta| meta.queue_exit_debts).unwrap_or_default();
    let debts = debts.into_iter().map(|debt| {
        let message = debt.intervention.message_id;
        (debt.event_id, debt.episode, message)
    });
    Ok(Some(RecordedQueue {
        versioned: content.trim_start().starts_with('{'),
        items: items.iter().map(|item| item.message_id).collect(),
        debts: debts.collect(),
    }))
}

/// Fails `channel`'s next queue save after it published, running `change` before the claim
/// re-reads the file.
pub(crate) fn fail_after_publish(channel: ChannelId, change: impl FnOnce() + Send + 'static) {
    pending_queue_persistence::post_save_fault::fail_next(channel);
    pending_queue_persistence::read_back_fault::before_next(channel, change);
}

pub(crate) fn mutant(name: &str) -> bool {
    std::env::var("ADK_S3ACT_B3A1_MUTANT").ok().as_deref() == Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn proof(channel: u64) -> Installation {
        Installation {
            channel: ChannelId::new(channel),
            token: Arc::new(CancelToken::from_persisted_turn_nonce(None)),
            episode: "exact".into(),
            birth: "birth".into(),
            owner: UserId::new(7),
            message: MessageId::new(71),
            generation: 1,
            current: Arc::new(AtomicU64::new(1)),
        }
    }
    /// A slot `proof`'s actor holds, with its record's driver at `driver`.
    fn installed(proof: &Installation, driver: PendingDriver) -> ChannelMailboxState {
        let mut state = ChannelMailboxState::default();
        state.cancel_token = Some(proof.token.clone());
        state.active_request_owner = Some(proof.owner);
        state.active_user_message_id = Some(proof.message);
        state.installed_pending = Some(InstalledPending {
            token: Arc::downgrade(&proof.token),
            episode: proof.episode.clone(),
            birth: proof.birth.clone(),
            owner: proof.owner,
            message: proof.message,
            generation: proof.generation,
            current: proof.current.clone(),
            feedback: PendingFeedback::Owed,
            driver,
            exits: Vec::new(),
        });
        state
    }
    fn driver(state: &ChannelMailboxState) -> Option<PendingDriver> {
        state
            .installed_pending
            .as_ref()
            .map(|pending| pending.driver)
    }
    #[tokio::test]
    async fn s3act_b3a1_generation_change_inside_row_check_preserves_actor() {
        let proof = proof(5340420);
        let mut state = installed(&proof, PendingDriver::Reserved);
        let current = proof.current.clone();
        let done = Arc::new(RecoveryDoneSignal::new());
        done.mark_done();
        let (reply, rx) = oneshot::channel();
        let channel = proof.channel;
        step(
            &mut state,
            channel,
            &done,
            Request {
                proof,
                intent: Intent::Ready,
                check: Box::new(move || {
                    current.store(2, Ordering::Release);
                    Ok(Box::new(()) as Box<dyn Send>)
                }),
                reply,
            },
        );
        let outcome = rx.await.unwrap();
        assert!(state.recovery_started_at.is_none());
        assert!(done.latched.load(Ordering::Acquire));
        assert!(matches!(outcome, HandoffOutcome::Refused(_)));
        assert_eq!(driver(&state), Some(PendingDriver::Reserved));
    }
    #[tokio::test]
    async fn s3act_b3a1_checked_handoff_closed_and_input_refusals_are_typed() {
        for closed in [false, true] {
            let proof = proof(5340421 + u64::from(closed));
            let (reply, rx) = oneshot::channel();
            let channel = proof.channel;
            let msg = ChannelMailboxMsg::InstalledActorHandoff(Request {
                proof,
                intent: Intent::Driver,
                check: Box::new(|| panic!("a refused message must not read the row")),
                reply,
            });
            let mut state = ChannelMailboxState::default();
            if closed {
                state.closed = true;
                assert!(registry_purge::gate_closed_arm(&state, channel, msg).is_none());
            } else {
                input_fence::refuse(
                    &state,
                    msg,
                    crate::services::discord::input_runtime::fence::Failure::Persistence,
                );
            }
            assert!(matches!(
                rx.await.unwrap(),
                HandoffOutcome::Refused(RestoreFailure::Rejected(_))
            ));
            assert!(state.recovery_started_at.is_none());
            assert!(state.cancel_token.is_none());
        }
    }
    #[tokio::test]
    async fn s3act_b3a1_dropped_reply_preserves_pending_actor_and_signals() {
        let proof = proof(5340427);
        let mut state = installed(&proof, PendingDriver::Reserved);
        let done = Arc::new(RecoveryDoneSignal::new());
        done.mark_done();
        let (reply, rx) = oneshot::channel();
        let checked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called = checked.clone();
        step(
            &mut state,
            proof.channel,
            &done,
            Request {
                proof: proof.clone(),
                intent: Intent::Ready,
                check: Box::new(move || {
                    called.store(true, Ordering::Release);
                    drop(rx);
                    Ok(Box::new(()) as Box<dyn Send>)
                }),
                reply,
            },
        );
        assert!(checked.load(Ordering::Acquire));
        assert!(state.recovery_started_at.is_none());
        assert!(done.latched.load(Ordering::Acquire));
        assert_eq!(driver(&state), Some(PendingDriver::Reserved));
        assert!(Arc::ptr_eq(
            state.cancel_token.as_ref().unwrap(),
            &proof.token
        ));
    }
}
