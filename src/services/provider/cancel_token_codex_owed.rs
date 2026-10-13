//! A Codex token's owed-delivery record: cancellation takes the record lock after the
//! publication lock, so it never lands between the exact check and the first poll.

use std::sync::MutexGuard;

use super::CancelToken;
pub(super) use crate::services::tui_prompt_dedupe::codex_verified::provenance::admission::EpisodePermissionRecord as Record;

impl CancelToken {
    /// The attached record's lock, taken by every cancellation publication; none is attached
    /// before P4-2 issues records.
    pub(super) fn codex_owed_publication(&self) -> Option<MutexGuard<'_, Option<&'static str>>> {
        self.codex_owed.get().map(|record| record.lock())
    }

    /// Binds the token's one record under the publication lock; a token already cancelled
    /// revokes it at once.
    pub(crate) fn attach_codex_owed(
        &self,
        record: std::sync::Arc<Record>,
    ) -> Result<(), &'static str> {
        let _publication = (self.cancellation_publication)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err("cancelled_before_attach");
        }
        if let Some(existing) = self.codex_owed.get() {
            return std::sync::Arc::ptr_eq(existing, &record)
                .then_some(())
                .ok_or("episode_permit_conflict");
        }
        self.codex_owed
            .set(record)
            .map_err(|_| "episode_permit_conflict")
    }
}
