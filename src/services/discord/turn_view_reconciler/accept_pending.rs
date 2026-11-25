//! The ⏳ an idle owner puts on an input at acceptance. A start re-notes Pending over it with a
//! fresh attempt, so the attempt-scoped settle after the handoff never touches a started turn.

use super::*;

/// Tries one settle makes before it keeps the early record for a later settle.
const SETTLE_TRIES: u32 = 3;
const SETTLE_BACKOFF: time::Duration = time::Duration::from_millis(250);

/// What a settle did with an early ⏳.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum AcceptSettle {
    /// No untaken early ⏳ on the target.
    Nothing,
    /// Rolled back to 📬, cleared, or found gone or taken over.
    Settled,
    /// Every try failed recoverably; the record stays for the next settle.
    Kept,
}

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
        // Persist the early mark so a restart can still tell this ⏳ from a started turn's.
        let lock = self.target_lock(target);
        let _guard = lock.lock().await;
        if let Some(current) = self.targets.get(&target).map(|entry| entry.clone()) {
            self.persist_target(target, &current, shared, source);
        }
        Some(attempt)
    }

    /// Whether this process still records `attempt` as `target`'s early ⏳.
    pub(super) fn accept_pending_recorded_is(
        &self,
        target: TurnViewTarget,
        attempt: TurnStartAttempt,
    ) -> bool {
        self.accept_pending
            .get(&target)
            .is_some_and(|entry| *entry == attempt)
    }

    /// The early attempt a persisted record still names; a restart lost the in-memory one.
    fn persisted_accept_attempt(&self, target: TurnViewTarget) -> Option<TurnStartAttempt> {
        let text = fs::read_to_string(Self::persisted_target_path(target)?).ok()?;
        let record = serde_json::from_str::<PersistedTargetState>(&text).ok()?;
        let early = record.accept_attempt_id?;
        (record.start_attempt_id == Some(early)).then_some(TurnStartAttempt(early))
    }

    /// The messages of `channel` whose persisted view is still an early ⏳, for the boot sweep.
    pub(in crate::services::discord) fn persisted_accept_pending(
        &self,
        shared: &SharedData,
        channel: ChannelId,
    ) -> Vec<MessageId> {
        let Some(root) = runtime_store::discord_turn_view_reconciler_root() else {
            return Vec::new();
        };
        let dir = root.join(TurnViewTargetKind::IntakeUserMessage.as_str());
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let prefix = format!("{}-", channel.get());
        let early = |entry: fs::DirEntry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(&prefix) || !name.ends_with(".json") {
                return None;
            }
            let text = fs::read_to_string(entry.path()).ok()?;
            let record = serde_json::from_str::<PersistedTargetState>(&text).ok()?;
            let attempt = record.accept_attempt_id?;
            let ours = record.provider == shared.provider.as_str()
                && record.channel_id == channel.get()
                && record.start_attempt_id == Some(attempt);
            (ours && record.message_id != 0).then(|| MessageId::new(record.message_id))
        };
        entries.flatten().filter_map(early).collect()
    }

    /// The early attempt no start took over and its owner. A gone, finished or taken-over target
    /// drops the record; an untaken one keeps it until its settle succeeds.
    fn untaken_accept_pending(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        source: &'static str,
    ) -> Option<(TurnViewOwner, TurnStartAttempt)> {
        let recorded = self.accept_pending.get(&target).map(|entry| *entry);
        let current = self
            .targets
            .get(&target)
            .map(|entry| entry.clone())
            .or_else(|| self.load_persisted_target(target, shared, source));
        let early = recorded.or_else(|| self.persisted_accept_attempt(target));
        match (current, early) {
            (Some(current), Some(early))
                if current.applied == TurnViewState::Pending
                    && current.start_attempt == Some(early) =>
            {
                Some((current.owner, early))
            }
            _ => {
                self.accept_pending.remove(&target);
                None
            }
        }
    }

    fn forget_accept_pending(&self, target: TurnViewTarget, attempt: TurnStartAttempt) {
        self.accept_pending
            .remove_if(&target, |_, recorded| *recorded == attempt);
    }

    /// After the handoff or a queue exit: an untaken early ⏳ goes back to 📬 while the owner holds
    /// the input, else it is cleared. `still_queued` is read only when one is untaken.
    pub(in crate::services::discord) async fn settle_accept_pending<Q>(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        still_queued: Q,
        source: &'static str,
    ) -> AcceptSettle
    where
        Q: std::future::Future<Output = bool>,
    {
        let Some((owner, attempt)) = self.untaken_accept_pending(shared, target, source) else {
            return AcceptSettle::Nothing;
        };
        let queued = still_queued.await;
        for tried in 1..=SETTLE_TRIES {
            let delivery = if queued {
                self.note_start_rolled_back_to_queued_delivery(
                    shared,
                    target,
                    owner.clone(),
                    attempt,
                    source,
                )
                .await
            } else {
                self.note_state_delivery_with_clear_attempt_guard(
                    shared,
                    target,
                    owner.clone(),
                    TurnViewIdentity::IntakeShared,
                    TurnViewState::None,
                    Some(attempt),
                    source,
                )
                .await
                .0
            };
            if delivery != TurnViewDelivery::Failed {
                self.forget_accept_pending(target, attempt);
                return AcceptSettle::Settled;
            }
            if tried < SETTLE_TRIES {
                tokio::time::sleep(SETTLE_BACKOFF * tried).await;
            }
        }
        AcceptSettle::Kept
    }

    /// The persisted (applied, early attempt) of `target`, for tests outside this module.
    #[cfg(test)]
    pub(in crate::services::discord) fn persisted_for_test(
        target: TurnViewTarget,
    ) -> Option<(String, Option<u64>)> {
        let text = fs::read_to_string(Self::persisted_target_path(target)?).ok()?;
        let record = serde_json::from_str::<PersistedTargetState>(&text).ok()?;
        Some((record.applied, record.accept_attempt_id))
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn accept_pending_recorded(
        &self,
        target: TurnViewTarget,
    ) -> Option<TurnStartAttempt> {
        self.accept_pending.get(&target).map(|entry| *entry)
    }
}
