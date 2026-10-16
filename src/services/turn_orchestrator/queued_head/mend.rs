//! A head left Unknown that the transcript proves Claude took: the paste record that names it, and
//! the actor commit that consumes it with no pane input. Finding that proof is the caller's job.
use super::*;

/// What names one paste in its transcript, recorded before the paste could start.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct HeadAttempt {
    pub(crate) nonce: String,
    pub(crate) transcript: String,
    pub(crate) session_id: Option<String>,
    /// Device and inode of the transcript, so a replaced file is never read as this one.
    pub(crate) file: Option<(u64, u64)>,
    /// The transcript length before the paste; its record can only come after.
    pub(crate) offset: u64,
}

impl HeadAttempt {
    /// The paste about to start under `nonce`; `None` when the transcript cannot be read.
    pub(super) fn before_paste(
        nonce: &str,
        transcript: &Path,
        session_id: Option<String>,
    ) -> Option<Self> {
        let meta = std::fs::metadata(transcript).ok()?;
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::MetadataExt;
            Some((meta.dev(), meta.ino()))
        };
        #[cfg(not(unix))]
        let file = None;
        Some(Self {
            nonce: nonce.to_string(),
            transcript: transcript.display().to_string(),
            session_id,
            file,
            offset: meta.len(),
        })
    }
}

/// The actor's Unknown head as the proof search reads it; consuming it needs all of it unchanged.
#[derive(Clone)]
pub(crate) struct UnknownHead {
    pub(crate) identity: QueueIdentity,
    pub(crate) original: PendingQueueItem,
    pub(crate) attempt: Option<HeadAttempt>,
}

type Gate = fn(ChannelId, Option<ChannelId>) -> bool;

pub(crate) enum MendMsg {
    Inspect {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<Option<UnknownHead>>,
    },
    Consume {
        head: Box<UnknownHead>,
        persistence: QueuePersistenceContext,
        admission: (ChannelId, Option<ChannelId>, Gate),
        reply: oneshot::Sender<Result<(), String>>,
    },
}

impl MendMsg {
    pub(super) fn persistence(&self) -> &QueuePersistenceContext {
        match self {
            Self::Inspect { persistence, .. } | Self::Consume { persistence, .. } => persistence,
        }
    }

    pub(super) fn refuse(self, error: String) {
        match self {
            Self::Inspect { reply, .. } => drop(reply.send(None)),
            Self::Consume { reply, .. } => drop(reply.send(Err(error))),
        }
    }
}

pub(super) fn step(state: &mut ChannelMailboxState, channel_id: ChannelId, msg: MendMsg) {
    match msg {
        MendMsg::Inspect { persistence, reply } => {
            let _ = reply.send(unknown_head(state, channel_id, &persistence));
        }
        MendMsg::Consume {
            head,
            persistence,
            admission: (channel, parent, gate),
            reply,
        } => {
            let open = channel == channel_id
                && crate::services::discord::health::InjectOrigin::queued_head_gate_enabled(
                    &persistence.provider,
                    channel,
                    parent,
                    gate,
                );
            if !open {
                let _ = reply.send(Err("gate_closed".to_string()));
                return;
            }
            state.last_persistence = Some(persistence.clone());
            let _ = reply.send(consume(state, channel_id, &head, &persistence));
        }
    }
}

/// The head this actor holds as Unknown; a record it cannot read names none.
fn unknown_head(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> Option<UnknownHead> {
    adopt(state, channel_id, persistence).ok()?;
    reap(&mut state.queued_head);
    let head = state.queued_head.meta.head_injection.as_ref()?;
    (head.phase == HeadPhase::Unknown).then(|| UnknownHead {
        identity: head.identity.clone(),
        original: head.original.clone(),
        attempt: head.attempt.clone(),
    })
}

/// Consumes the head only while this actor still holds exactly it as Unknown at the queue front;
/// `Err("stale")` changes nothing. A failed write restores the queue and record, still Unknown.
fn consume(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    expected: &UnknownHead,
    persistence: &QueuePersistenceContext,
) -> Result<(), String> {
    adopt(state, channel_id, persistence)?;
    reap(&mut state.queued_head);
    let payload = |item: &PendingQueueItem| serde_json::to_value(item).ok();
    let held = state
        .queued_head
        .meta
        .head_injection
        .as_ref()
        .filter(|head| {
            head.phase == HeadPhase::Unknown
                && head.identity == expected.identity
                && head.attempt == expected.attempt
                && payload(&head.original) == payload(&expected.original)
        });
    let first = state.intervention_queue.first();
    if !held
        .zip(first)
        .is_some_and(|(head, first)| is_head(head, first, channel_id))
    {
        return Err("stale".to_string());
    }
    let previous_queue = state.intervention_queue.clone();
    let previous_meta = state.queued_head.meta.clone();
    state.intervention_queue.remove(0);
    state.queued_head.meta.head_injection = None;
    let terminal = &mut state.queued_head.meta.terminal_identities;
    terminal.push(expected.identity.clone());
    persist_queue(state, channel_id, persistence).inspect_err(|error| {
        state.intervention_queue = previous_queue;
        state.queued_head.meta = previous_meta;
        log_queue_persistence_rollback("queued_head_mend", channel_id, persistence, error);
    })
}

impl ChannelMailboxHandle {
    /// The channel's Unknown head, if this actor holds one.
    pub(crate) async fn unknown_queued_head(
        &self,
        persistence: QueuePersistenceContext,
    ) -> Option<UnknownHead> {
        let msg = |reply| MendMsg::Inspect { persistence, reply };
        self.request(|reply| mend_msg(msg(reply))).await.ok()?
    }

    /// Consumes `head` on proof found outside the actor, if it is still this actor's Unknown head
    /// and the queued-head gate is open; any `Err` left the queue and the head as they were.
    pub(crate) async fn mend_queued_head(
        &self,
        head: UnknownHead,
        persistence: QueuePersistenceContext,
        admission: (ChannelId, Option<ChannelId>, Gate),
    ) -> Result<(), String> {
        let msg = |reply| MendMsg::Consume {
            head: Box::new(head),
            persistence,
            admission,
            reply,
        };
        let answer = self.request(|reply| mend_msg(msg(reply))).await;
        answer.unwrap_or_else(|_| Err("mailbox unreachable".to_string()))
    }
}

fn mend_msg(msg: MendMsg) -> ChannelMailboxMsg {
    let head = super::HeadMsg::Mend(msg);
    ChannelMailboxMsg::Injection(super::super::injected_inputs::InjectionMsg::Head(head))
}

#[cfg(all(test, unix))]
#[path = "mend_tests.rs"]
mod tests;
