//! The owner commits continuation custody and queue priority together; submission spends it.
use super::super::v2_types::effort_restart::{
    Continuation, EffortRestart, PROMPT, Request, SOURCE,
};
use super::*;
use sha2::{Digest, Sha256};

impl ChannelMailboxHandle {
    pub(crate) fn effort_input(context: Option<&serde_json::Value>) -> bool {
        context
            .is_some_and(|context| context["origin"] == "headless" && context["source"] == SOURCE)
    }
}

impl Owner {
    /// A verified caller updates the intent; ingress may only restore it with `None`.
    pub(crate) fn prepare_effort(
        &mut self,
        request: Option<Request>,
        channel: u64,
        generation: u64,
    ) -> Result<EffortRestart<PendingQueueItem>, String> {
        self.recover()?;
        let Some(request) = request else {
            return self
                .snapshot
                .effort_restart
                .clone()
                .ok_or("effort intent missing".into());
        };
        if request.operation.is_empty()
            || request.native_session.is_empty()
            || request
                .interrupted_episode
                .as_ref()
                .is_some_and(String::is_empty)
        {
            return Err("effort restart identity missing".into());
        }
        let current = self.snapshot.effort_restart.as_ref();
        if current.is_some_and(|old| old.cancelled && old.operation == request.operation) {
            return Err("effort continuation cancelled".into());
        }
        let current = current.filter(|old| !old.cancelled);
        let same_episode = current.filter(|old| {
            old.native_session == request.native_session
                && old.interrupted_episode == request.interrupted_episode
                && (old.interrupted_episode.is_some() || old.operation == request.operation)
        });
        if same_episode.is_none() && current.is_some_and(|old| old.pending().is_some()) {
            return Err("pending effort continuation belongs to another episode".into());
        }
        let intent = if let Some(old) = same_episode {
            let mut intent = old.clone();
            if request.cut.is_some() {
                intent.cut = request.cut.clone();
            }
            if request.desired_revision > intent.desired_revision {
                intent.desired_revision = request.desired_revision;
                intent.ready = request.ready;
            } else if request.desired_revision == intent.desired_revision {
                intent.ready |= request.ready;
            }
            intent
        } else {
            let continuation = request
                .interrupted_episode
                .as_ref()
                .map(|episode| {
                    let digest = Sha256::digest(
                        serde_json::to_vec(&(
                            "agentdesk-effort-continuation-v1",
                            channel,
                            &request.native_session,
                            episode,
                        ))
                        .map_err(|error| error.to_string())?,
                    );
                    let mut source = [0; 8];
                    source.copy_from_slice(&digest[..8]);
                    let message = u64::from_be_bytes(source) | (1 << 63);
                    let ticket = format!("effort-{}", hex::encode(&digest[..28]));
                    let original = serde_json::from_value(serde_json::json!({"author_id":1,
                    "author_is_bot":true,"message_id":message,"source_message_ids":[message],
                    "channel_id":channel,"queued_generation":generation,"text":PROMPT}))
                    .map_err(|e| e.to_string())?;
                    let original = self.retry_original(&ticket, &original)?;
                    Ok::<_, String>(Continuation {
                        ticket,
                        original,
                        submitted: false,
                    })
                })
                .transpose()?;
            EffortRestart {
                operation: request.operation,
                native_session: request.native_session,
                interrupted_episode: request.interrupted_episode,
                desired_revision: request.desired_revision,
                ready: request.ready,
                continuation,
                cancelled: false,
                cut: request.cut,
            }
        };
        let mut next = self.snapshot.clone();
        next.effort_restart = Some(intent.clone());
        self.commit(next)?;
        if intent.pending().is_some() {
            self.revoke_front();
            self.held = None;
        }
        Ok(intent)
    }

    /// Closes `operation`'s unsubmitted continuation and its barrier in one commit. `false` when
    /// the stored intent is another operation's or has nothing left to close.
    pub(crate) fn cancel_effort(&mut self, operation: &str) -> Result<bool, String> {
        self.recover()?;
        let mut next = self.snapshot.clone();
        let Some(intent) = next.effort_restart.as_mut() else {
            return Ok(false);
        };
        if intent.operation != operation || intent.pending().is_none() {
            return Ok(false);
        }
        intent.cancelled = true;
        self.commit(next)?;
        Ok(true)
    }

    /// The stored intent while it still owes a step: not ready yet, or ready with its continuation
    /// never admitted.
    pub(crate) fn effort_owed(&self) -> Option<EffortRestart<PendingQueueItem>> {
        let intent = self.snapshot.effort_restart.as_ref()?;
        let c = intent.pending()?;
        let ticket = Ticket::new(&c.ticket).ok()?;
        let admitted = matches!(
            self.admission_status(&ticket),
            source_admission::TicketStatus::Committed
        );
        (!intent.ready || !admitted).then(|| intent.clone())
    }

    /// Older records may omit the flag; only an exact delivered receipt can restore it.
    pub(super) fn restore_effort_receipt(&self, next: &mut SnapshotV2) -> Result<(), String> {
        let Some(c) = next
            .effort_restart
            .as_mut()
            .and_then(|intent| intent.continuation.as_mut())
            .filter(|c| !c.submitted)
        else {
            return Ok(());
        };
        if matches!(
            self.exact_turn(&Ticket::new(&c.ticket)?, &c.original)?,
            source_admission::TurnBind::Settled(_)
        ) {
            c.submitted = true;
        }
        Ok(())
    }

    pub(super) fn effort_blocks(&self, original: Option<&PendingQueueItem>) -> bool {
        std::iter::once(&self.snapshot)
            .chain(self.pending.iter().map(|(_, s)| s))
            .any(|snapshot| {
                snapshot.effort_restart.as_ref().is_some_and(|intent| {
                    intent.pending().is_some_and(|c| {
                        !intent.ready
                            || original.is_none_or(|item| {
                                !source_admission::same_original(item, &c.original)
                            })
                    })
                })
            })
    }

    pub(super) fn check_effort_admission(
        &self,
        original: &PendingQueueItem,
        ticket: &Ticket,
        context: &serde_json::Value,
    ) -> Result<Option<source_admission::Admitted>, source_admission::AdmitError> {
        use source_admission::{AdmitError, Admitted};
        let intent = self
            .snapshot
            .effort_restart
            .as_ref()
            .ok_or_else(|| AdmitError::NotCommitted("effort intent missing".into()))?;
        let c = intent
            .continuation
            .as_ref()
            .ok_or_else(|| AdmitError::NotCommitted("no interrupted episode".into()))?;
        if intent.cancelled {
            return Err(AdmitError::Conflict("effort continuation cancelled".into()));
        }
        if Ticket::new(&c.ticket).as_ref() != Ok(ticket)
            || !source_admission::same_original(original, &c.original)
            || *context != intent.context()
        {
            return Err(AdmitError::Conflict(
                "effort continuation identity differs".into(),
            ));
        }
        if c.submitted {
            return Ok(Some(Admitted::Duplicate));
        }
        if !intent.ready
            || self.snapshot.head_injection.is_some() && self.ticket_original(ticket).is_none()
        {
            return Err(AdmitError::NotCommitted(
                "effort resume not ready for continuation".into(),
            ));
        }
        Ok(None)
    }
}

/// What a queue-removing commit dropped: everything waiting, or only these originals.
pub(super) enum Dropped<'a> {
    All,
    Only(&'a [PendingQueueItem]),
}

/// Closes the unsubmitted continuation `next` no longer holds where it could still run, in the
/// commit that dropped it. One still queued (an attempted head stays queued) or in doubt stays owned.
pub(super) fn close_dropped(next: &mut SnapshotV2, dropped: Dropped<'_>) {
    let Some(intent) = next.effort_restart.as_ref() else {
        return;
    };
    let Some(c) = intent.pending() else {
        return;
    };
    let named = match dropped {
        Dropped::All => true,
        Dropped::Only(items) => items
            .iter()
            .any(|item| source_admission::same_original(item, &c.original)),
    };
    let identity = sources::identity(&c.original);
    let queued = next
        .items
        .iter()
        .any(|item| sources::identity(item) == identity);
    let in_doubt = (next.retained_inputs.iter()).any(|entry| {
        matches!(entry.reason, RetainedReason::Unconfirmed)
            && entry
                .input
                .members()
                .any(|item| sources::identity(item) == identity)
    });
    if named
        && !queued
        && !in_doubt
        && let Some(intent) = next.effort_restart.as_mut()
    {
        intent.cancelled = true;
    }
}

pub(super) fn prepend(next: &mut SnapshotV2) {
    let last = next.items.len() - 1;
    next.items.rotate_right(1);
    for bind in &mut next.admission_binds {
        if let BindAt::Queued { index, .. } = &mut bind.at {
            *index = if *index == last { 0 } else { *index + 1 };
        }
    }
}

pub(super) fn submitted(snapshot: &mut SnapshotV2, members: &[QueueIdentity]) {
    if let Some(c) = snapshot
        .effort_restart
        .as_mut()
        .and_then(|intent| intent.continuation.as_mut())
        && members.contains(&sources::identity(&c.original))
    {
        c.submitted = true;
    }
}

pub(super) fn queue_full(len: usize, continuation: bool) -> bool {
    len >= if continuation { 31 } else { 30 }
}
