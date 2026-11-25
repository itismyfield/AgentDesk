//! #8398: the ⏳ an idle owner puts on a human input when it accepts it (RED stub).

use super::*;

impl TurnViewReconciler {
    pub(in crate::services::discord) async fn note_accept_pending(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        identity: TurnViewIdentity,
        source: &'static str,
    ) -> Option<TurnStartAttempt> {
        let _ = (shared, target, identity, source, &self.accept_pending);
        None
    }

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
        let _ = (shared, target, still_queued, source);
        false
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn accept_pending_recorded(
        &self,
        target: TurnViewTarget,
    ) -> Option<TurnStartAttempt> {
        self.accept_pending.get(&target).map(|entry| *entry)
    }
}
