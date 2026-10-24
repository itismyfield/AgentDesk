#![cfg(test)]

pub(crate) use super::pending_queue_persistence::{
    load_channel_pending_dispatch_marker_checked, load_channel_pending_queue_checked,
};

use std::sync::{MutexGuard, PoisonError};

pub(crate) const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

/// Uses the crate-wide environment lock so root-mutating tests share one guard.
/// Retains `.lock()` for existing callers.
pub(crate) struct SharedEnvLock;

impl SharedEnvLock {
    pub(crate) fn lock(
        &self,
    ) -> Result<MutexGuard<'static, ()>, PoisonError<MutexGuard<'static, ()>>> {
        crate::config::shared_test_env_lock().lock()
    }
}

pub(crate) static TEST_ENV_LOCK: SharedEnvLock = SharedEnvLock;

pub(crate) fn lock_test_env() -> MutexGuard<'static, ()> {
    TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Fails the channel's next `count` queue saves.
pub(crate) fn fail_queue_saves(channel: super::ChannelId, count: usize) {
    for _ in 0..count {
        super::pending_queue_persistence::save_fault::fail_next(channel);
    }
}

/// Queue-save faults not yet consumed; disarms them when `clear`.
pub(crate) fn queue_save_faults(channel: super::ChannelId, clear: bool) -> usize {
    super::pending_queue_persistence::save_fault::armed(channel, clear)
}

static INJECTION_ABANDONS: std::sync::Mutex<Vec<super::ChannelId>> =
    std::sync::Mutex::new(Vec::new());

pub(crate) fn note_injection_abandon(channel: super::ChannelId) {
    let mut abandons = INJECTION_ABANDONS.lock().unwrap_or_else(|e| e.into_inner());
    abandons.push(channel);
}

/// Injection reservations the channel abandoned after its handback attempts ran out.
pub(crate) fn injection_abandons(channel: super::ChannelId) -> usize {
    let abandons = INJECTION_ABANDONS.lock().unwrap_or_else(|e| e.into_inner());
    abandons
        .iter()
        .filter(|abandoned| **abandoned == channel)
        .count()
}
