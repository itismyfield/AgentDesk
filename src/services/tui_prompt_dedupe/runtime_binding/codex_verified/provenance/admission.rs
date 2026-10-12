//! Exact per-episode permits for verified owed work. Nothing issues one before P4-2, and the
//! readiness gate refuses every lookup and admission until P5, so no new POST permission exists.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use super::dormant::OwedOrigin;
use super::{ExecutionProofRef, TurnEpisodeRef};
use crate::services::provider::CancelToken;

/// The one non-serialized permission state of an exact episode in this process boot.
pub(crate) struct EpisodePermissionRecord {
    execution: ExecutionProofRef,
    episode: TurnEpisodeRef,
    token: Arc<CancelToken>,
    /// Held from the exact check through `Prepared` and the first poll; cancellation takes it too.
    revoked: Mutex<Option<&'static str>>,
}

type EpisodeKey = (u64, u64, String);

fn key(episode: &TurnEpisodeRef) -> EpisodeKey {
    let nonce = episode.turn_nonce.clone();
    (episode.channel_id, episode.user_message_id, nonce)
}

static REGISTRY: LazyLock<Mutex<HashMap<EpisodeKey, Arc<EpisodePermissionRecord>>>> =
    LazyLock::new(Mutex::default);

fn registry() -> MutexGuard<'static, HashMap<EpisodeKey, Arc<EpisodePermissionRecord>>> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
thread_local! {
    static READY: std::cell::RefCell<std::collections::HashSet<u64>> = Default::default();
}

/// Verified owed delivery stays off until P5; Legacy and Shadow channels never reach it.
pub(crate) fn owed_delivery_ready(channel: u64) -> bool {
    #[cfg(test)]
    if READY.with(|ready| ready.borrow().contains(&channel)) {
        return true;
    }
    production_default_owed_readiness(channel)
}

fn production_default_owed_readiness(channel: u64) -> bool {
    let _ = channel;
    false
}

/// The record for `origin`'s exact episode; a missing, foreign or not-ready one is Unknown.
pub(crate) fn lookup(origin: &OwedOrigin) -> Result<Arc<EpisodePermissionRecord>, &'static str> {
    if !owed_delivery_ready(origin.span.episode.channel_id) {
        return Err("verified_owed_not_ready");
    }
    // The registry guard drops here, so no record lock is ever taken under it.
    let record = registry().get(&key(&origin.span.episode)).cloned();
    let record = record.ok_or("episode_permit_unavailable")?;
    record.matches(origin)?;
    Ok(record)
}

impl EpisodePermissionRecord {
    fn matches(&self, origin: &OwedOrigin) -> Result<(), &'static str> {
        let span = &origin.span;
        if self.execution != span.execution
            || self.episode != span.episode
            || span.delivery_channel_id != self.episode.channel_id
        {
            return Err("episode_permit_mismatch");
        }
        Ok(())
    }

    /// Runs `admitted` under the record lock only while the exact permit still holds, so a
    /// cancellation either lands before the check or waits until the first poll returned.
    pub(crate) fn admit<T>(
        &self,
        origin: &OwedOrigin,
        admitted: impl FnOnce() -> T,
    ) -> Result<T, &'static str> {
        let revoked = self.lock();
        if !owed_delivery_ready(self.episode.channel_id) {
            return Err("verified_owed_not_ready");
        }
        if let Some(reason) = *revoked {
            return Err(reason);
        }
        self.matches(origin)?;
        if self.token.cancelled.load(Ordering::Acquire)
            || self.token.turn_nonce() != Some(self.episode.turn_nonce.as_str())
        {
            return Err("episode_cancelled");
        }
        Ok(admitted())
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Option<&'static str>> {
        self.revoked.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A writer that stops for good leaves each of its channel's records Unknown, never settled.
pub(crate) fn revoke_channel(channel: u64, reason: &'static str) {
    let records: Vec<_> = (registry().iter())
        .filter(|(key, _)| key.0 == channel)
        .map(|(_, record)| Arc::clone(record))
        .collect();
    for record in records {
        record.lock().get_or_insert(reason);
    }
}

#[cfg(test)]
pub(crate) fn set_ready_for_tests(channel: u64, ready: bool) {
    READY.with(|set| match ready {
        true => set.borrow_mut().insert(channel),
        false => set.borrow_mut().remove(&channel),
    });
}

/// Test seam for P4-2's issuance: one Arc per exact episode, a conflict never replaces it.
#[cfg(test)]
pub(crate) fn issue_for_tests(
    execution: ExecutionProofRef,
    episode: TurnEpisodeRef,
    token: &Arc<CancelToken>,
) -> Result<Arc<EpisodePermissionRecord>, &'static str> {
    let mut registry = registry();
    if let Some(existing) = registry.get(&key(&episode)) {
        let same = Arc::ptr_eq(&existing.token, token)
            && existing.execution == execution
            && existing.episode == episode;
        return same
            .then(|| Arc::clone(existing))
            .ok_or("episode_permit_conflict");
    }
    let record = Arc::new(EpisodePermissionRecord {
        execution,
        episode: episode.clone(),
        token: Arc::clone(token),
        revoked: Mutex::new(None),
    });
    token.attach_codex_owed(Arc::clone(&record));
    registry.insert(key(&episode), Arc::clone(&record));
    Ok(record)
}

#[cfg(test)]
pub(crate) fn forget_channel_for_tests(channel: u64) {
    registry().retain(|key, _| key.0 != channel);
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
