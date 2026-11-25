//! #8398: the ⏳ an idle owner puts on a human input when it accepts it, before its start is
//! decided, and the settle that retracts that ⏳ when no start took it over.
//!
//! A start site later re-notes Pending over the early ⏳; that only takes a fresh attempt
//! (`update_matching_pending_attempt` or the generation transfer), never another add. Every
//! rollback or clear here is scoped to the early attempt, so a start that took over makes the
//! settle a no-op.

use super::*;

impl TurnViewReconciler {
    /// Puts ⏳ on an accepted input that has no turn view yet and records its start attempt.
    pub(in crate::services::discord) async fn note_accept_pending(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        identity: TurnViewIdentity,
        source: &'static str,
    ) -> Option<TurnStartAttempt> {
        if target.kind != TurnViewTargetKind::IntakeUserMessage
            || self.targets.contains_key(&target)
        {
            return None;
        }
        let owner = TurnViewOwner::for_message(
            target.channel_id,
            target.message_id,
            shared.restart.current_generation,
        );
        let record = self
            .note_turn_started_with_attempt(shared, target, owner, identity, source)
            .await;
        let attempt = record.attempt().filter(|_| record.delivered())?;
        self.accept_pending.insert(target, attempt);
        Some(attempt)
    }

    /// The early attempt on `target` that no start took over, with the owner it was noted under:
    /// this generation's record, or a Pending that an older generation persisted (its record was
    /// lost with that process). Consumes this generation's record either way.
    fn untaken_accept_pending(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        source: &'static str,
    ) -> Option<(TurnViewOwner, TurnStartAttempt)> {
        let recorded = self
            .accept_pending
            .remove(&target)
            .map(|(_, attempt)| attempt);
        let current = self
            .targets
            .get(&target)
            .map(|entry| entry.clone())
            .or_else(|| self.load_persisted_target(target, shared, source))?;
        if current.applied != TurnViewState::Pending {
            return None;
        }
        let attempt = current.start_attempt?;
        let untaken = match recorded {
            Some(recorded) => recorded == attempt,
            None => current.owner.generation < shared.restart.current_generation,
        };
        untaken.then_some((current.owner, attempt))
    }

    /// Settles an early ⏳ once its owner handoff returned: back to 📬 while the owner still holds
    /// the original, cleared when it left the queue without a start. `still_queued` is read only
    /// when an untaken attempt exists. Returns whether it acted.
    pub(in crate::services::discord) async fn settle_accept_pending<Q>(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        still_queued: Q,
        source: &'static str,
    ) -> bool
    where
        Q: std::future::Future<Output = bool>,
    {
        let Some((owner, attempt)) = self.untaken_accept_pending(shared, target, source) else {
            return false;
        };
        if still_queued.await {
            self.note_start_rolled_back_to_queued(shared, target, owner, attempt, source)
                .await
        } else {
            self.note_turn_cleared_if_attempt_matches(
                shared,
                target,
                owner,
                TurnViewIdentity::IntakeShared,
                attempt,
                source,
            )
            .await
        }
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn accept_pending_recorded(
        &self,
        target: TurnViewTarget,
    ) -> Option<TurnStartAttempt> {
        self.accept_pending.get(&target).map(|entry| *entry)
    }
}
