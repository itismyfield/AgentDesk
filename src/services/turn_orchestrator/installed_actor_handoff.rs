#![cfg(test)]

use super::*;
use crate::services::cluster::channel_home_port::RestoreFailure;
use std::collections::HashMap;
use std::sync::{Mutex, Weak, atomic::AtomicU64};

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
struct Pending {
    token: Weak<CancelToken>,
    episode: String,
    birth: String,
    owner: UserId,
    message: MessageId,
    generation: u64,
    current: Arc<AtomicU64>,
    driven: bool,
    events: Vec<QueueExitEvent>,
}
// Pending responsibilities are metadata; the mailbox and fresh row still prove the actor.
type PendingTable = HashMap<(ChannelId, usize), Pending>;
static PENDING: LazyLock<Mutex<PendingTable>> = LazyLock::new(|| Mutex::new(HashMap::new()));
fn key(proof: &Installation) -> (ChannelId, usize) {
    (proof.channel, Arc::as_ptr(&proof.token) as usize)
}
fn same(record: &Pending, proof: &Installation) -> bool {
    record.birth == proof.birth
        && record.owner == proof.owner
        && record.message == proof.message
        && record
            .token
            .upgrade()
            .is_some_and(|token| Arc::ptr_eq(&token, &proof.token))
}
pub(crate) fn remember(
    proof: &Installation,
    claimed: bool,
    events: Vec<QueueExitEvent>,
) -> Result<bool, String> {
    let mut records = PENDING.lock().unwrap();
    records.retain(|_, record| record.token.strong_count() != 0);
    if let Some(record) = records.get_mut(&key(proof)) {
        if !same(record, proof)
            || (proof.episode != record.episode && proof.generation <= record.generation)
            || proof.generation < record.generation
            || proof.current.load(Ordering::Acquire) != proof.generation
        {
            return Err("installed actor identity/generation changed".into());
        }
        if record.driven || mutant("pending-reentry") {
            return Ok(false);
        }
        record.episode = proof.episode.clone();
        record.generation = proof.generation;
        record.current = proof.current.clone();
        return Ok(true);
    }
    if !claimed {
        return Ok(false);
    }
    records.insert(
        key(proof),
        Pending {
            token: Arc::downgrade(&proof.token),
            episode: proof.episode.clone(),
            birth: proof.birth.clone(),
            owner: proof.owner,
            message: proof.message,
            generation: proof.generation,
            current: proof.current.clone(),
            driven: false,
            events,
        },
    );
    Ok(true)
}
pub(crate) fn belongs(channel: ChannelId, token: &Arc<CancelToken>, episode: &str) -> bool {
    PENDING
        .lock()
        .unwrap()
        .get(&(channel, Arc::as_ptr(token) as usize))
        .is_some_and(|record| {
            record.birth == episode
                && record
                    .token
                    .upgrade()
                    .is_some_and(|installed| Arc::ptr_eq(&installed, token))
        })
}
pub(crate) enum HandoffOutcome {
    Ready(Vec<QueueExitEvent>),
    AlreadyDriven,
    Refused(RestoreFailure),
}
type RowCheck = Box<dyn FnOnce() -> Result<Box<dyn Send>, String> + Send>;
pub(super) struct Request {
    proof: Installation,
    drive: bool,
    check: RowCheck,
    reply: oneshot::Sender<HandoffOutcome>,
}
impl Request {
    pub(super) fn refuse(self, reason: RestoreFailure) {
        let _ = self.reply.send(HandoffOutcome::Refused(reason));
    }
}
impl ChannelMailboxHandle {
    pub(crate) async fn handoff_installed(
        &self,
        proof: Installation,
        drive: bool,
        check: impl FnOnce() -> Result<Box<dyn Send>, String> + Send + 'static,
    ) -> HandoffOutcome {
        self.request(|reply| {
            ChannelMailboxMsg::InstalledActorHandoff(Request {
                proof,
                drive,
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
        drive,
        check,
        reply,
    } = request;
    if reply.is_closed() {
        return;
    }
    let outcome = (|| {
        let mut records = PENDING.lock().unwrap();
        let record = records
            .get_mut(&key(&proof))
            .ok_or("installation responsibility absent")?;
        if channel != proof.channel
            || !same(record, &proof)
            || record.episode != proof.episode
            || (!mutant("handoff-generation")
                && (record.generation != proof.generation
                    || record.current.load(Ordering::Acquire) != proof.generation))
            || (!mutant("handoff-token")
                && !state
                    .cancel_token
                    .as_ref()
                    .is_some_and(|token| Arc::ptr_eq(token, &proof.token)))
            || state.active_request_owner != Some(proof.owner)
            || state.active_user_message_id != Some(proof.message)
            || proof.token.cancelled.load(Ordering::Acquire)
        {
            return Err("handoff actor/episode/generation changed".to_string());
        }
        let _row = if !mutant("handoff-row") {
            Some(check()?)
        } else {
            None
        };
        if !mutant("handoff-generation")
            && record.current.load(Ordering::Acquire) != proof.generation
        {
            return Err("handoff generation changed during row check".into());
        }
        if reply.is_closed() {
            return Err("handoff caller left before acceptance".into());
        }
        if record.driven {
            return Ok(HandoffOutcome::AlreadyDriven);
        }
        if drive {
            record.driven = true;
            GLOBAL_RECOVERY_DONE_SIGNALS.insert(channel, own_done.clone());
            recovery_kickoff::reset_activation_signals(channel);
            state.recovery_started_at = Some(Instant::now());
        }
        Ok(HandoffOutcome::Ready(std::mem::take(&mut record.events)))
    })()
    .unwrap_or_else(|reason| HandoffOutcome::Refused(reason.into()));
    let _ = reply.send(outcome);
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
    #[tokio::test]
    async fn s3act_b3a1_generation_change_inside_row_check_preserves_actor() {
        let proof = proof(5340420);
        remember(&proof, true, Vec::new()).unwrap();
        let current = proof.current.clone();
        let mut state = ChannelMailboxState::default();
        state.cancel_token = Some(proof.token.clone());
        state.active_request_owner = Some(proof.owner);
        state.active_user_message_id = Some(proof.message);
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
                drive: true,
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
    }
    #[tokio::test]
    async fn s3act_b3a1_checked_handoff_closed_and_input_refusals_are_typed() {
        for closed in [false, true] {
            let proof = proof(5340421 + u64::from(closed));
            let (reply, rx) = oneshot::channel();
            let channel = proof.channel;
            let msg = ChannelMailboxMsg::InstalledActorHandoff(Request {
                proof,
                drive: true,
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
        remember(&proof, true, Vec::new()).unwrap();
        let mut state = ChannelMailboxState::default();
        state.cancel_token = Some(proof.token.clone());
        state.active_request_owner = Some(proof.owner);
        state.active_user_message_id = Some(proof.message);
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
                drive: true,
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
        assert!(remember(&proof, false, Vec::new()).unwrap());
    }
}
