use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use uuid::Uuid;

use super::{
    ChannelMailboxHandle, ChannelMailboxMsg, ChannelMailboxState, InterventionMode,
    PurgeQueueResult, RecoveryKickoffResult, TakeNextSoftResult, TryStartTurnResult,
};

pub(crate) const RESUME_TRANSITION_LEASE_DURATION: Duration = Duration::from_secs(120);
pub(crate) const RESUME_TRANSITION_HISTORY_LIMIT: usize = 32;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ResumeTransitionKey {
    pub(crate) transition_id: Uuid,
    pub(crate) fence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResumeTransitionTerminalKind {
    Completed,
    Aborted,
    LeaseExpired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResumeTransitionTerminalReceipt {
    pub(crate) key: ResumeTransitionKey,
    pub(crate) kind: ResumeTransitionTerminalKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResumeTransitionAdvanceReceipt {
    pub(crate) previous: ResumeTransitionKey,
    pub(crate) current: ResumeTransitionKey,
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
    Advanced(ResumeTransitionAdvanceReceipt),
    AlreadyAdvancedCurrent(ResumeTransitionAdvanceReceipt),
    Refused(ResumeTransitionMutationRefusal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EndResumeTransitionResult {
    Applied(ResumeTransitionTerminalReceipt),
    AlreadyAppliedInactive(ResumeTransitionTerminalReceipt),
    Refused(ResumeTransitionMutationRefusal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResumeTransitionLeaseResult {
    Renewed(ResumeTransitionKey),
    Refused(ResumeTransitionMutationRefusal),
}

#[derive(Clone, Copy)]
struct ActiveResumeTransition {
    key: ResumeTransitionKey,
    lease_expires_at: Instant,
}

#[derive(Default)]
pub(super) struct ResumeTransitionState {
    active: Option<ActiveResumeTransition>,
    last_issued_fence: u64,
    advance_history: VecDeque<ResumeTransitionAdvanceReceipt>,
    advance_by_previous: HashMap<ResumeTransitionKey, ResumeTransitionAdvanceReceipt>,
    terminal_history: VecDeque<ResumeTransitionTerminalReceipt>,
    terminal_by_transition: HashMap<Uuid, ResumeTransitionTerminalReceipt>,
}

impl ResumeTransitionState {
    pub(super) fn is_reserved(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn recover_expired(
        &mut self,
        now: Instant,
    ) -> Option<ResumeTransitionTerminalReceipt> {
        let active = self
            .active
            .filter(|active| active.lease_expires_at <= now)?;
        self.active = None;
        let receipt = ResumeTransitionTerminalReceipt {
            key: active.key,
            kind: ResumeTransitionTerminalKind::LeaseExpired,
        };
        self.push_terminal(receipt);
        Some(receipt)
    }

    pub(super) fn begin(
        &mut self,
        transition_id: Uuid,
        now: Instant,
    ) -> BeginResumeTransitionResult {
        self.recover_expired(now);
        if let Some(active) = self.active {
            return if active.key.transition_id == transition_id {
                BeginResumeTransitionResult::AlreadyReserved(active.key)
            } else {
                BeginResumeTransitionResult::Occupied(active.key)
            };
        }
        if let Some(terminal) = self.terminal_by_transition.get(&transition_id).copied() {
            return BeginResumeTransitionResult::AlreadyTerminal(terminal);
        }
        // B1 is a reservation primitive, not the quiescence protocol itself.
        // A future B2 caller must establish an idle mailbox before `begin`:
        // no active turn, recovery marker, or pending dispatch handoff.
        let Some(key) = self.issue_key(transition_id) else {
            return BeginResumeTransitionResult::FenceExhausted;
        };
        self.active = Some(ActiveResumeTransition {
            key,
            lease_expires_at: now + RESUME_TRANSITION_LEASE_DURATION,
        });
        BeginResumeTransitionResult::Begun(key)
    }

    pub(super) fn advance(
        &mut self,
        key: ResumeTransitionKey,
        now: Instant,
    ) -> AdvanceResumeTransitionResult {
        self.recover_expired(now);
        let active = self.active.map(|active| active.key);
        if active != Some(key) {
            if let Some(receipt) = self.advance_by_previous.get(&key).copied()
                && active == Some(receipt.current)
            {
                return AdvanceResumeTransitionResult::AlreadyAdvancedCurrent(receipt);
            }
            return AdvanceResumeTransitionResult::Refused(self.stale_refusal(key));
        }
        let Some(current) = self.issue_key(key.transition_id) else {
            return AdvanceResumeTransitionResult::Refused(
                ResumeTransitionMutationRefusal::FenceExhausted,
            );
        };
        let receipt = ResumeTransitionAdvanceReceipt {
            previous: key,
            current,
        };
        self.active = Some(ActiveResumeTransition {
            key: current,
            lease_expires_at: now + RESUME_TRANSITION_LEASE_DURATION,
        });
        self.push_advance(receipt);
        AdvanceResumeTransitionResult::Advanced(receipt)
    }

    pub(super) fn renew(
        &mut self,
        key: ResumeTransitionKey,
        now: Instant,
    ) -> ResumeTransitionLeaseResult {
        self.recover_expired(now);
        if self.active.map(|active| active.key) != Some(key) {
            return ResumeTransitionLeaseResult::Refused(self.stale_refusal(key));
        }
        let active = self.active.as_mut().expect("exact active key checked");
        active.lease_expires_at = now + RESUME_TRANSITION_LEASE_DURATION;
        ResumeTransitionLeaseResult::Renewed(key)
    }

    pub(super) fn complete(
        &mut self,
        key: ResumeTransitionKey,
        now: Instant,
    ) -> EndResumeTransitionResult {
        self.end(key, ResumeTransitionTerminalKind::Completed, now)
    }

    pub(super) fn abort(
        &mut self,
        key: ResumeTransitionKey,
        now: Instant,
    ) -> EndResumeTransitionResult {
        self.end(key, ResumeTransitionTerminalKind::Aborted, now)
    }

    fn end(
        &mut self,
        key: ResumeTransitionKey,
        kind: ResumeTransitionTerminalKind,
        now: Instant,
    ) -> EndResumeTransitionResult {
        self.recover_expired(now);
        if self.active.map(|active| active.key) != Some(key) {
            if self.active.is_none()
                && let Some(terminal) = self.terminal_for_key(key)
            {
                return if terminal.kind == kind {
                    EndResumeTransitionResult::AlreadyAppliedInactive(terminal)
                } else {
                    EndResumeTransitionResult::Refused(
                        ResumeTransitionMutationRefusal::TerminalConflict(terminal),
                    )
                };
            }
            return EndResumeTransitionResult::Refused(self.stale_refusal(key));
        }
        let receipt = ResumeTransitionTerminalReceipt { key, kind };
        self.active = None;
        self.push_terminal(receipt);
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

    fn terminal_for_key(
        &self,
        key: ResumeTransitionKey,
    ) -> Option<ResumeTransitionTerminalReceipt> {
        self.terminal_by_transition
            .get(&key.transition_id)
            .copied()
            .filter(|receipt| receipt.key == key)
    }

    fn stale_refusal(&self, key: ResumeTransitionKey) -> ResumeTransitionMutationRefusal {
        ResumeTransitionMutationRefusal::Stale {
            current: self.active.map(|active| active.key),
            terminal: self.terminal_for_key(key),
        }
    }

    fn push_advance(&mut self, receipt: ResumeTransitionAdvanceReceipt) {
        self.advance_by_previous.insert(receipt.previous, receipt);
        if self.advance_history.len() == RESUME_TRANSITION_HISTORY_LIMIT
            && let Some(evicted) = self.advance_history.pop_front()
        {
            self.advance_by_previous.remove(&evicted.previous);
        }
        self.advance_history.push_back(receipt);
    }

    fn push_terminal(&mut self, receipt: ResumeTransitionTerminalReceipt) {
        self.terminal_by_transition
            .insert(receipt.key.transition_id, receipt);
        if self.terminal_history.len() == RESUME_TRANSITION_HISTORY_LIMIT
            && let Some(evicted) = self.terminal_history.pop_front()
        {
            self.terminal_by_transition
                .remove(&evicted.key.transition_id);
        }
        self.terminal_history.push_back(receipt);
    }
}

pub(super) fn gate_reserved_arm(
    state: &mut ChannelMailboxState,
    msg: ChannelMailboxMsg,
    now: Instant,
) -> Option<ChannelMailboxMsg> {
    state.resume_transition.recover_expired(now);
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
                has_more: state
                    .intervention_queue
                    .iter()
                    .any(|item| item.mode == InterventionMode::Soft),
                queue_len_after: state.intervention_queue.len(),
                queue_exit_events: Vec::new(),
                persistence_error: None,
                held_for_resume_transition: true,
            });
            None
        }
        ChannelMailboxMsg::Clear { reply, .. } => {
            let _ = reply.send(super::ClearChannelResult {
                removed_token: None,
                queue_exit_events: Vec::new(),
                persistence_error: None,
                refused_resume_transition: true,
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

    pub(crate) async fn renew_resume_transition(
        &self,
        key: ResumeTransitionKey,
    ) -> ResumeTransitionLeaseResult {
        self.request(
            |reply| ChannelMailboxMsg::RenewResumeTransition { key, reply },
            ResumeTransitionLeaseResult::Refused(ResumeTransitionMutationRefusal::ActorUnreachable),
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

    use super::*;
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::turn_orchestrator::{
        ActiveTurnKind, ChannelMailboxRegistry, EnqueueRefusalReason, Intervention,
        QueuePersistenceContext, SourceMessageTextSegment,
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

    async fn begin(handle: &ChannelMailboxHandle, transition_id: Uuid) -> ResumeTransitionKey {
        match handle.begin_resume_transition(transition_id).await {
            BeginResumeTransitionResult::Begun(key) => key,
            other => panic!("unexpected begin result: {other:?}"),
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
        let key = begin(&handle, Uuid::new_v4()).await;
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
        assert!(
            handle
                .enqueue(intervention(101), persistence.clone())
                .await
                .enqueued
        );
        let requeued = handle
            .requeue_front(intervention(100), persistence.clone())
            .await;
        assert!(requeued.enqueued);
        assert_ne!(
            requeued.refusal_reason,
            Some(EnqueueRefusalReason::MailboxClosed)
        );
        let before = handle.snapshot().await;
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
        assert_eq!(
            handle.purge_queue(persistence.clone(), true).await,
            PurgeQueueResult {
                refused_resume_transition: true,
                ..PurgeQueueResult::default()
            }
        );
        assert_eq!(
            close_if_idle(&handle).await,
            Err("resume_transition_reserved")
        );
        assert!(matches!(
            handle.complete_resume_transition(key).await,
            EndResumeTransitionResult::Applied(_)
        ));
        let taken = handle.take_next_soft(persistence).await;
        assert_eq!(
            taken.intervention.as_ref().map(|item| item.message_id),
            Some(MessageId::new(100))
        );
        assert!(!taken.held_for_resume_transition);
    }

    #[derive(Clone, Copy)]
    enum ClearReservationRelease {
        Complete,
        Abort,
    }

    async fn assert_clear_refused_then_succeeds(channel_id: u64, release: ClearReservationRelease) {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(channel_id));
        let persistence = persistence("resume-transition-clear");
        let token = Arc::new(CancelToken::new());
        assert!(
            handle
                .try_start_turn(Arc::clone(&token), UserId::new(11), MessageId::new(110),)
                .await
        );
        assert!(
            handle
                .enqueue(intervention(111), persistence.clone())
                .await
                .enqueued
        );
        let key = begin(&handle, Uuid::new_v4()).await;
        let before = handle.snapshot().await;

        let refused = handle.clear(persistence.clone()).await;
        assert!(refused.refused_resume_transition);
        assert!(refused.removed_token.is_none());
        assert!(refused.queue_exit_events.is_empty());
        let after_refusal = handle.snapshot().await;
        assert!(
            after_refusal
                .cancel_token
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &token))
        );
        assert_eq!(
            after_refusal.active_request_owner,
            before.active_request_owner
        );
        assert_eq!(
            after_refusal.active_user_message_id,
            before.active_user_message_id
        );
        assert_eq!(
            after_refusal
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

        match release {
            ClearReservationRelease::Complete => assert!(matches!(
                handle.complete_resume_transition(key).await,
                EndResumeTransitionResult::Applied(_)
            )),
            ClearReservationRelease::Abort => assert!(matches!(
                handle.abort_resume_transition(key).await,
                EndResumeTransitionResult::Applied(_)
            )),
        }
        let cleared = handle.clear(persistence).await;
        assert!(!cleared.refused_resume_transition);
        assert!(
            cleared
                .removed_token
                .as_ref()
                .is_some_and(|removed| Arc::ptr_eq(removed, &token))
        );
        assert_eq!(cleared.queue_exit_events.len(), 1);
        let after_clear = handle.snapshot().await;
        assert!(after_clear.cancel_token.is_none());
        assert!(after_clear.intervention_queue.is_empty());
    }

    #[tokio::test]
    async fn clear_is_refused_without_state_loss_until_complete_or_abort() {
        assert_clear_refused_then_succeeds(4_916_010, ClearReservationRelease::Complete).await;
        assert_clear_refused_then_succeeds(4_916_011, ClearReservationRelease::Abort).await;
    }

    #[tokio::test]
    async fn stale_t1_receipts_do_not_authorize_while_t2_is_active() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(4_916_002));
        let t1 = begin(&handle, Uuid::new_v4()).await;
        let t1_current = match handle.advance_resume_transition(t1).await {
            AdvanceResumeTransitionResult::Advanced(receipt) => receipt.current,
            other => panic!("unexpected advance result: {other:?}"),
        };
        assert_eq!(
            handle.advance_resume_transition(t1).await,
            AdvanceResumeTransitionResult::AlreadyAdvancedCurrent(ResumeTransitionAdvanceReceipt {
                previous: t1,
                current: t1_current,
            })
        );
        assert!(matches!(
            handle.complete_resume_transition(t1_current).await,
            EndResumeTransitionResult::Applied(_)
        ));
        let t2 = begin(&handle, Uuid::new_v4()).await;
        assert!(matches!(
            handle.advance_resume_transition(t1).await,
            AdvanceResumeTransitionResult::Refused(
                ResumeTransitionMutationRefusal::Stale {
                    current: Some(current),
                    ..
                }
            ) if current == t2
        ));
        assert!(matches!(
            handle.complete_resume_transition(t1_current).await,
            EndResumeTransitionResult::Refused(
                ResumeTransitionMutationRefusal::Stale {
                    current: Some(current),
                    ..
                }
            ) if current == t2
        ));
        assert!(matches!(
            handle.abort_resume_transition(t1_current).await,
            EndResumeTransitionResult::Refused(
                ResumeTransitionMutationRefusal::Stale {
                    current: Some(current),
                    ..
                }
            ) if current == t2
        ));
        let wrong_fence = ResumeTransitionKey {
            transition_id: t2.transition_id,
            fence: t2.fence + 1,
        };
        assert!(matches!(
            handle.advance_resume_transition(wrong_fence).await,
            AdvanceResumeTransitionResult::Refused(
                ResumeTransitionMutationRefusal::Stale {
                    current: Some(current),
                    ..
                }
            ) if current == t2
        ));
        assert!(matches!(
            handle.complete_resume_transition(wrong_fence).await,
            EndResumeTransitionResult::Refused(
                ResumeTransitionMutationRefusal::Stale {
                    current: Some(current),
                    ..
                }
            ) if current == t2
        ));
        assert!(matches!(
            handle.abort_resume_transition(wrong_fence).await,
            EndResumeTransitionResult::Refused(
                ResumeTransitionMutationRefusal::Stale {
                    current: Some(current),
                    ..
                }
            ) if current == t2
        ));
        assert!(matches!(
            handle.renew_resume_transition(wrong_fence).await,
            ResumeTransitionLeaseResult::Refused(
                ResumeTransitionMutationRefusal::Stale {
                    current: Some(current),
                    ..
                }
            ) if current == t2
        ));
    }

    #[tokio::test]
    async fn completed_transition_replay_stays_terminal_after_later_completion() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(4_916_003));
        let t1_id = Uuid::new_v4();
        let t1 = begin(&handle, t1_id).await;
        let t1_receipt = match handle.complete_resume_transition(t1).await {
            EndResumeTransitionResult::Applied(receipt) => receipt,
            other => panic!("unexpected completion result: {other:?}"),
        };
        let t2 = begin(&handle, Uuid::new_v4()).await;
        assert!(matches!(
            handle.complete_resume_transition(t2).await,
            EndResumeTransitionResult::Applied(_)
        ));
        assert_eq!(
            handle.begin_resume_transition(t1_id).await,
            BeginResumeTransitionResult::AlreadyTerminal(t1_receipt)
        );
        assert_eq!(
            handle.advance_resume_transition(t1).await,
            AdvanceResumeTransitionResult::Refused(ResumeTransitionMutationRefusal::Stale {
                current: None,
                terminal: Some(t1_receipt),
            })
        );
        assert_eq!(
            handle.complete_resume_transition(t1).await,
            EndResumeTransitionResult::AlreadyAppliedInactive(t1_receipt)
        );
        assert_eq!(
            handle.abort_resume_transition(t1).await,
            EndResumeTransitionResult::Refused(ResumeTransitionMutationRefusal::TerminalConflict(
                t1_receipt
            ))
        );
    }

    #[test]
    fn expired_orphan_recovers_without_losing_queue_and_live_owner_can_renew() {
        let mut state = ChannelMailboxState::default();
        state.intervention_queue.push(intervention(401));
        let started_at = Instant::now();
        let key = match state.resume_transition.begin(Uuid::new_v4(), started_at) {
            BeginResumeTransitionResult::Begun(key) => key,
            other => panic!("unexpected begin result: {other:?}"),
        };
        let near_expiry = started_at + RESUME_TRANSITION_LEASE_DURATION - Duration::from_secs(1);
        assert_eq!(
            state.resume_transition.renew(key, near_expiry),
            ResumeTransitionLeaseResult::Renewed(key)
        );
        assert!(state.resume_transition.is_reserved());
        assert!(
            state
                .resume_transition
                .recover_expired(
                    near_expiry + RESUME_TRANSITION_LEASE_DURATION - Duration::from_secs(1)
                )
                .is_none()
        );
        let terminal = state
            .resume_transition
            .recover_expired(near_expiry + RESUME_TRANSITION_LEASE_DURATION)
            .expect("orphan lease should expire at the renewed deadline");
        assert_eq!(terminal.key, key);
        assert_eq!(terminal.kind, ResumeTransitionTerminalKind::LeaseExpired);
        assert_eq!(state.intervention_queue[0].message_id, MessageId::new(401));
        assert_eq!(
            state.resume_transition.complete(
                key,
                near_expiry + RESUME_TRANSITION_LEASE_DURATION + Duration::from_secs(1),
            ),
            EndResumeTransitionResult::Refused(ResumeTransitionMutationRefusal::TerminalConflict(
                terminal
            ))
        );
    }

    #[test]
    fn expired_clear_gate_releases_message_and_preserves_queue() {
        let mut state = ChannelMailboxState::default();
        state.intervention_queue.push(intervention(402));
        let started_at = Instant::now();
        assert!(matches!(
            state.resume_transition.begin(Uuid::new_v4(), started_at),
            BeginResumeTransitionResult::Begun(_)
        ));
        let (reply, receive) = oneshot::channel();
        let clear = gate_reserved_arm(
            &mut state,
            ChannelMailboxMsg::Clear {
                persistence: persistence("resume-transition-expired-clear"),
                reply,
            },
            started_at + RESUME_TRANSITION_LEASE_DURATION,
        );
        assert!(matches!(clear, Some(ChannelMailboxMsg::Clear { .. })));
        assert_eq!(state.intervention_queue[0].message_id, MessageId::new(402));
        drop(clear);
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn indexed_histories_are_bounded() {
        let mut state = ResumeTransitionState::default();
        let mut now = Instant::now();
        for _ in 0..(RESUME_TRANSITION_HISTORY_LIMIT + 5) {
            let transition_id = Uuid::new_v4();
            let mut key = match state.begin(transition_id, now) {
                BeginResumeTransitionResult::Begun(key) => key,
                other => panic!("unexpected begin result: {other:?}"),
            };
            for _ in 0..2 {
                now += Duration::from_millis(1);
                key = match state.advance(key, now) {
                    AdvanceResumeTransitionResult::Advanced(receipt) => receipt.current,
                    other => panic!("unexpected advance result: {other:?}"),
                };
            }
            now += Duration::from_millis(1);
            assert!(matches!(
                state.complete(key, now),
                EndResumeTransitionResult::Applied(_)
            ));
        }
        assert_eq!(state.advance_history.len(), RESUME_TRANSITION_HISTORY_LIMIT);
        assert_eq!(
            state.advance_by_previous.len(),
            RESUME_TRANSITION_HISTORY_LIMIT
        );
        assert_eq!(
            state.terminal_history.len(),
            RESUME_TRANSITION_HISTORY_LIMIT
        );
        assert_eq!(
            state.terminal_by_transition.len(),
            RESUME_TRANSITION_HISTORY_LIMIT
        );
    }

    #[tokio::test]
    async fn gated_take_reports_no_soft_work_for_non_soft_only_queue() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(4_916_009));
        let key = begin(&handle, Uuid::new_v4()).await;
        let persistence = persistence("resume-transition-non-soft");
        let mut item = intervention(901);
        item.mode = InterventionMode::TestNonSoft;
        assert!(handle.enqueue(item, persistence.clone()).await.enqueued);
        let held = handle.take_next_soft(persistence).await;
        assert!(held.intervention.is_none());
        assert!(held.held_for_resume_transition);
        assert!(!held.has_more);
        assert_eq!(held.queue_len_after, 1);
        assert!(matches!(
            handle.abort_resume_transition(key).await,
            EndResumeTransitionResult::Applied(_)
        ));
    }

    #[tokio::test]
    async fn terminal_idempotency_is_only_authoritative_while_inactive() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(4_916_006));
        let t1 = begin(&handle, Uuid::new_v4()).await;
        let terminal = match handle.complete_resume_transition(t1).await {
            EndResumeTransitionResult::Applied(receipt) => receipt,
            other => panic!("unexpected completion result: {other:?}"),
        };
        assert_eq!(
            handle.complete_resume_transition(t1).await,
            EndResumeTransitionResult::AlreadyAppliedInactive(terminal)
        );
        let t2 = begin(&handle, Uuid::new_v4()).await;
        assert!(matches!(
            handle.complete_resume_transition(t1).await,
            EndResumeTransitionResult::Refused(
                ResumeTransitionMutationRefusal::Stale {
                    current: Some(current),
                    ..
                }
            ) if current == t2
        ));
    }

    #[tokio::test]
    async fn caller_cancellation_does_not_clear_committed_reservation() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(4_916_007));
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
        assert!(
            !handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(10),
                    MessageId::new(100),
                )
                .await
        );
        assert!(matches!(
            handle.abort_resume_transition(key).await,
            EndResumeTransitionResult::Applied(_)
        ));
    }

    #[tokio::test]
    async fn abort_cleanup_allows_idle_close_again() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(4_916_008));
        let key = begin(&handle, Uuid::new_v4()).await;
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
