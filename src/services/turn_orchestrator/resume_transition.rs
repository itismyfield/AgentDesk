use uuid::Uuid;

use super::{
    ChannelMailboxHandle, ChannelMailboxMsg, ChannelMailboxState, PurgeQueueResult,
    RecoveryKickoffResult, TakeNextSoftResult, TryStartTurnResult,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ResumeTransitionKey {
    pub(crate) transition_id: Uuid,
    pub(crate) fence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResumeTransitionTerminalKind {
    Completed,
    Aborted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResumeTransitionTerminalReceipt {
    pub(crate) key: ResumeTransitionKey,
    pub(crate) kind: ResumeTransitionTerminalKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BeginResumeTransitionResult {
    Begun(ResumeTransitionKey),
    AlreadyReserved(ResumeTransitionKey),
    Occupied(ResumeTransitionKey),
    AlreadyTerminal(ResumeTransitionTerminalReceipt),
    FenceExhausted,
    MailboxClosed,
    ActorUnreachable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResumeTransitionMutationRefusal {
    Stale {
        current: Option<ResumeTransitionKey>,
        terminal: Option<ResumeTransitionTerminalReceipt>,
    },
    TerminalConflict(ResumeTransitionTerminalReceipt),
    FenceExhausted,
    MailboxClosed,
    ActorUnreachable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdvanceResumeTransitionResult {
    Advanced {
        previous: ResumeTransitionKey,
        current: ResumeTransitionKey,
    },
    AlreadyAdvanced {
        previous: ResumeTransitionKey,
        current: ResumeTransitionKey,
    },
    Refused(ResumeTransitionMutationRefusal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EndResumeTransitionResult {
    Applied(ResumeTransitionTerminalReceipt),
    AlreadyApplied(ResumeTransitionTerminalReceipt),
    Refused(ResumeTransitionMutationRefusal),
}

#[derive(Default)]
pub(super) struct ResumeTransitionState {
    active: Option<ResumeTransitionKey>,
    last_issued_fence: u64,
    advance_receipts: Vec<(ResumeTransitionKey, ResumeTransitionKey)>,
    terminal: Option<ResumeTransitionTerminalReceipt>,
}

impl ResumeTransitionState {
    pub(super) fn is_reserved(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn begin(&mut self, transition_id: Uuid) -> BeginResumeTransitionResult {
        if let Some(active) = self.active {
            return if active.transition_id == transition_id {
                BeginResumeTransitionResult::AlreadyReserved(active)
            } else {
                BeginResumeTransitionResult::Occupied(active)
            };
        }
        if let Some(terminal) = self
            .terminal
            .filter(|receipt| receipt.key.transition_id == transition_id)
        {
            return BeginResumeTransitionResult::AlreadyTerminal(terminal);
        }
        let Some(key) = self.issue_key(transition_id) else {
            return BeginResumeTransitionResult::FenceExhausted;
        };
        self.active = Some(key);
        BeginResumeTransitionResult::Begun(key)
    }

    pub(super) fn advance(&mut self, key: ResumeTransitionKey) -> AdvanceResumeTransitionResult {
        if let Some((previous, current)) = self
            .advance_receipts
            .iter()
            .copied()
            .find(|(previous, _)| *previous == key)
        {
            return AdvanceResumeTransitionResult::AlreadyAdvanced { previous, current };
        }
        if self.active != Some(key) {
            return AdvanceResumeTransitionResult::Refused(self.stale_refusal(key));
        }
        let Some(current) = self.issue_key(key.transition_id) else {
            return AdvanceResumeTransitionResult::Refused(
                ResumeTransitionMutationRefusal::FenceExhausted,
            );
        };
        self.active = Some(current);
        self.advance_receipts.push((key, current));
        AdvanceResumeTransitionResult::Advanced {
            previous: key,
            current,
        }
    }

    pub(super) fn complete(&mut self, key: ResumeTransitionKey) -> EndResumeTransitionResult {
        self.end(key, ResumeTransitionTerminalKind::Completed)
    }

    pub(super) fn abort(&mut self, key: ResumeTransitionKey) -> EndResumeTransitionResult {
        self.end(key, ResumeTransitionTerminalKind::Aborted)
    }

    fn end(
        &mut self,
        key: ResumeTransitionKey,
        kind: ResumeTransitionTerminalKind,
    ) -> EndResumeTransitionResult {
        if let Some(terminal) = self.terminal.filter(|receipt| receipt.key == key) {
            return if terminal.kind == kind {
                EndResumeTransitionResult::AlreadyApplied(terminal)
            } else {
                EndResumeTransitionResult::Refused(
                    ResumeTransitionMutationRefusal::TerminalConflict(terminal),
                )
            };
        }
        if self.active != Some(key) {
            return EndResumeTransitionResult::Refused(self.stale_refusal(key));
        }
        let receipt = ResumeTransitionTerminalReceipt { key, kind };
        self.active = None;
        self.terminal = Some(receipt);
        EndResumeTransitionResult::Applied(receipt)
    }

    fn issue_key(&mut self, transition_id: Uuid) -> Option<ResumeTransitionKey> {
        let fence = self.last_issued_fence.checked_add(1)?;
        self.last_issued_fence = fence;
        Some(ResumeTransitionKey {
            transition_id,
            fence,
        })
    }

    fn stale_refusal(&self, key: ResumeTransitionKey) -> ResumeTransitionMutationRefusal {
        ResumeTransitionMutationRefusal::Stale {
            current: self.active,
            terminal: self
                .terminal
                .filter(|receipt| receipt.key.transition_id == key.transition_id),
        }
    }
}

pub(super) fn gate_reserved_arm(
    state: &ChannelMailboxState,
    msg: ChannelMailboxMsg,
) -> Option<ChannelMailboxMsg> {
    if !state.resume_transition.is_reserved() {
        return Some(msg);
    }
    match msg {
        ChannelMailboxMsg::TryStartTurn { reply, .. } => {
            let _ = reply.send(TryStartTurnResult {
                refused_resume_transition: true,
                ..TryStartTurnResult::default()
            });
            None
        }
        ChannelMailboxMsg::RestoreActiveTurn { reply, .. } => {
            let _ = reply.send(());
            None
        }
        ChannelMailboxMsg::RecoveryKickoff { reply, .. } => {
            let _ = reply.send(RecoveryKickoffResult {
                activated_turn: false,
                refused_closed: false,
                refused_resume_transition: true,
            });
            None
        }
        ChannelMailboxMsg::TakeNextSoft { reply, .. } => {
            let _ = reply.send(TakeNextSoftResult {
                intervention: None,
                dispatch_lease: None,
                has_more: !state.intervention_queue.is_empty(),
                queue_len_after: state.intervention_queue.len(),
                queue_exit_events: Vec::new(),
                persistence_error: None,
                held_for_resume_transition: true,
            });
            None
        }
        ChannelMailboxMsg::PurgeQueue { reply, .. } => {
            let _ = reply.send(PurgeQueueResult {
                refused_resume_transition: true,
                ..PurgeQueueResult::default()
            });
            None
        }
        ChannelMailboxMsg::CloseIfIdle { reply } => {
            let _ = reply.send(Err("resume_transition_reserved"));
            None
        }
        other => Some(other),
    }
}

impl ChannelMailboxHandle {
    pub(crate) async fn begin_resume_transition(
        &self,
        transition_id: Uuid,
    ) -> BeginResumeTransitionResult {
        self.request(
            |reply| ChannelMailboxMsg::BeginResumeTransition {
                transition_id,
                reply,
            },
            BeginResumeTransitionResult::ActorUnreachable,
        )
        .await
    }

    pub(crate) async fn advance_resume_transition(
        &self,
        key: ResumeTransitionKey,
    ) -> AdvanceResumeTransitionResult {
        self.request(
            |reply| ChannelMailboxMsg::AdvanceResumeTransition { key, reply },
            AdvanceResumeTransitionResult::Refused(
                ResumeTransitionMutationRefusal::ActorUnreachable,
            ),
        )
        .await
    }

    pub(crate) async fn complete_resume_transition(
        &self,
        key: ResumeTransitionKey,
    ) -> EndResumeTransitionResult {
        self.request(
            |reply| ChannelMailboxMsg::CompleteResumeTransition { key, reply },
            EndResumeTransitionResult::Refused(ResumeTransitionMutationRefusal::ActorUnreachable),
        )
        .await
    }

    pub(crate) async fn abort_resume_transition(
        &self,
        key: ResumeTransitionKey,
    ) -> EndResumeTransitionResult {
        self.request(
            |reply| ChannelMailboxMsg::AbortResumeTransition { key, reply },
            EndResumeTransitionResult::Refused(ResumeTransitionMutationRefusal::ActorUnreachable),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use tokio::sync::oneshot;
    use uuid::Uuid;

    use super::*;
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::turn_orchestrator::{
        ActiveTurnKind, ChannelMailboxHandle, ChannelMailboxRegistry, EnqueueRefusalReason,
        Intervention, InterventionMode, QueuePersistenceContext, SourceMessageTextSegment,
    };

    fn persistence(name: &str) -> QueuePersistenceContext {
        QueuePersistenceContext::new(&ProviderKind::Claude, name, None)
    }

    fn intervention(message_id: u64) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: vec![SourceMessageTextSegment::new(
                MessageId::new(message_id),
                format!("resume-{message_id}"),
            )],
            text: format!("resume-{message_id}"),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    async fn close_if_idle(handle: &ChannelMailboxHandle) -> Result<(), &'static str> {
        let (reply, receive) = oneshot::channel();
        handle
            .sender
            .send(ChannelMailboxMsg::CloseIfIdle { reply })
            .expect("mailbox actor should accept close verdict request");
        receive
            .await
            .expect("mailbox actor should answer close verdict")
    }

    #[tokio::test]
    async fn reservation_gates_admission_dequeue_purge_and_close() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(4_916_001));
        let transition_id = Uuid::new_v4();
        let key = match handle.begin_resume_transition(transition_id).await {
            BeginResumeTransitionResult::Begun(key) => key,
            other => panic!("unexpected begin result: {other:?}"),
        };

        let start = handle
            .try_start_turn_kinded_result(
                Arc::new(CancelToken::new()),
                UserId::new(7),
                MessageId::new(70),
                ActiveTurnKind::UserOrAgent,
                None,
            )
            .await;
        assert!(!start.started);
        assert!(start.refused_resume_transition);

        handle
            .restore_active_turn_kinded(
                Arc::new(CancelToken::new()),
                UserId::new(8),
                MessageId::new(80),
                ActiveTurnKind::UserOrAgent,
            )
            .await;

        let recovery = handle
            .recovery_kickoff(
                Arc::new(CancelToken::new()),
                UserId::new(9),
                Some(MessageId::new(90)),
            )
            .await;
        assert!(!recovery.activated_turn);
        assert!(recovery.refused_resume_transition);
        assert!(!handle.has_active_turn().await);

        let persistence = persistence("resume-transition-gates");
        let enqueued = handle.enqueue(intervention(101), persistence.clone()).await;
        assert!(enqueued.enqueued);
        assert_eq!(enqueued.refusal_reason, None);
        let requeued = handle
            .requeue_front(intervention(100), persistence.clone())
            .await;
        assert!(requeued.enqueued);
        assert_ne!(
            requeued.refusal_reason,
            Some(EnqueueRefusalReason::MailboxClosed)
        );

        let before = handle.snapshot().await;
        assert_eq!(
            before
                .intervention_queue
                .iter()
                .map(|item| item.message_id)
                .collect::<Vec<_>>(),
            vec![MessageId::new(100), MessageId::new(101)]
        );
        let held = handle.take_next_soft(persistence.clone()).await;
        assert!(held.intervention.is_none());
        assert!(held.held_for_resume_transition);
        assert!(held.has_more);
        assert_eq!(held.queue_len_after, 2);
        let after_hold = handle.snapshot().await;
        assert_eq!(
            after_hold
                .intervention_queue
                .iter()
                .map(|item| item.message_id)
                .collect::<Vec<_>>(),
            before
                .intervention_queue
                .iter()
                .map(|item| item.message_id)
                .collect::<Vec<_>>()
        );
        assert_eq!(after_hold.pending_user_dispatch, None);

        let purge = handle.purge_queue(persistence.clone(), true).await;
        assert_eq!(
            purge,
            PurgeQueueResult {
                refused_resume_transition: true,
                ..PurgeQueueResult::default()
            }
        );
        let after_purge = handle.snapshot().await;
        assert_eq!(
            after_purge
                .intervention_queue
                .iter()
                .map(|item| item.message_id)
                .collect::<Vec<_>>(),
            vec![MessageId::new(100), MessageId::new(101)]
        );
        assert_eq!(
            close_if_idle(&handle).await,
            Err("resume_transition_reserved")
        );

        assert!(matches!(
            handle.complete_resume_transition(key).await,
            EndResumeTransitionResult::Applied(ResumeTransitionTerminalReceipt {
                kind: ResumeTransitionTerminalKind::Completed,
                ..
            })
        ));
        let taken = handle.take_next_soft(persistence).await;
        assert_eq!(
            taken.intervention.as_ref().map(|item| item.message_id),
            Some(MessageId::new(100))
        );
        assert!(!taken.held_for_resume_transition);
    }

    #[tokio::test]
    async fn exact_keys_are_monotonic_stale_safe_and_idempotent() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(4_916_002));
        let first_id = Uuid::new_v4();
        let first = match handle.begin_resume_transition(first_id).await {
            BeginResumeTransitionResult::Begun(key) => key,
            other => panic!("unexpected begin result: {other:?}"),
        };
        assert_eq!(first.fence, 1);
        assert_eq!(
            handle.begin_resume_transition(first_id).await,
            BeginResumeTransitionResult::AlreadyReserved(first)
        );
        assert!(matches!(
            handle.begin_resume_transition(Uuid::new_v4()).await,
            BeginResumeTransitionResult::Occupied(active) if active == first
        ));

        let foreign = ResumeTransitionKey {
            transition_id: Uuid::new_v4(),
            fence: first.fence,
        };
        assert!(matches!(
            handle.advance_resume_transition(foreign).await,
            AdvanceResumeTransitionResult::Refused(
                ResumeTransitionMutationRefusal::Stale { current: Some(active), .. }
            ) if active == first
        ));
        let stale_fence = ResumeTransitionKey {
            transition_id: first.transition_id,
            fence: first.fence + 10,
        };
        assert!(matches!(
            handle.complete_resume_transition(stale_fence).await,
            EndResumeTransitionResult::Refused(
                ResumeTransitionMutationRefusal::Stale { current: Some(active), .. }
            ) if active == first
        ));

        let second = match handle.advance_resume_transition(first).await {
            AdvanceResumeTransitionResult::Advanced { previous, current } => {
                assert_eq!(previous, first);
                current
            }
            other => panic!("unexpected advance result: {other:?}"),
        };
        assert_eq!(second.fence, 2);
        assert_eq!(
            handle.advance_resume_transition(first).await,
            AdvanceResumeTransitionResult::AlreadyAdvanced {
                previous: first,
                current: second,
            }
        );
        assert!(matches!(
            handle.abort_resume_transition(first).await,
            EndResumeTransitionResult::Refused(
                ResumeTransitionMutationRefusal::Stale { current: Some(active), .. }
            ) if active == second
        ));

        let completed = ResumeTransitionTerminalReceipt {
            key: second,
            kind: ResumeTransitionTerminalKind::Completed,
        };
        assert_eq!(
            handle.complete_resume_transition(second).await,
            EndResumeTransitionResult::Applied(completed)
        );
        assert_eq!(
            handle.complete_resume_transition(second).await,
            EndResumeTransitionResult::AlreadyApplied(completed)
        );
        assert_eq!(
            handle.abort_resume_transition(second).await,
            EndResumeTransitionResult::Refused(ResumeTransitionMutationRefusal::TerminalConflict(
                completed
            ))
        );

        let next_id = Uuid::new_v4();
        let third = match handle.begin_resume_transition(next_id).await {
            BeginResumeTransitionResult::Begun(key) => key,
            other => panic!("unexpected second begin result: {other:?}"),
        };
        assert_eq!(third.fence, 3);
        let aborted = ResumeTransitionTerminalReceipt {
            key: third,
            kind: ResumeTransitionTerminalKind::Aborted,
        };
        assert_eq!(
            handle.abort_resume_transition(third).await,
            EndResumeTransitionResult::Applied(aborted)
        );
        assert_eq!(
            handle.abort_resume_transition(third).await,
            EndResumeTransitionResult::AlreadyApplied(aborted)
        );
    }

    #[tokio::test]
    async fn caller_cancellation_does_not_clear_committed_reservation() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(4_916_003));
        let transition_id = Uuid::new_v4();
        let (reply, receive) = oneshot::channel();
        handle
            .sender
            .send(ChannelMailboxMsg::BeginResumeTransition {
                transition_id,
                reply,
            })
            .expect("mailbox actor should accept begin request");
        drop(receive);

        let key = match handle.begin_resume_transition(transition_id).await {
            BeginResumeTransitionResult::AlreadyReserved(key) => key,
            other => panic!("cancelled caller reservation was not retained: {other:?}"),
        };
        assert_eq!(key.fence, 1);
        let blocked = handle
            .try_start_turn(
                Arc::new(CancelToken::new()),
                UserId::new(10),
                MessageId::new(100),
            )
            .await;
        assert!(!blocked);
        assert!(matches!(
            handle.abort_resume_transition(key).await,
            EndResumeTransitionResult::Applied(ResumeTransitionTerminalReceipt {
                kind: ResumeTransitionTerminalKind::Aborted,
                ..
            })
        ));
        assert!(
            handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(10),
                    MessageId::new(100),
                )
                .await
        );
    }

    #[tokio::test]
    async fn abort_cleanup_allows_idle_close_again() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(4_916_004));
        let key = match handle.begin_resume_transition(Uuid::new_v4()).await {
            BeginResumeTransitionResult::Begun(key) => key,
            other => panic!("unexpected begin result: {other:?}"),
        };
        assert_eq!(
            close_if_idle(&handle).await,
            Err("resume_transition_reserved")
        );
        assert!(matches!(
            handle.abort_resume_transition(key).await,
            EndResumeTransitionResult::Applied(_)
        ));
        assert_eq!(close_if_idle(&handle).await, Ok(()));
    }
}
