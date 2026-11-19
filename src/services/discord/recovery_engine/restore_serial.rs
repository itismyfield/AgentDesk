//! One recovery at a time per channel: a boot restore and an adoption wake may both reach the
//! same kept row, and only the first may send for it and settle it.

use super::*;

/// Restores each row under its channel's restore lock, once its disk copy is still that turn.
pub(in crate::services::discord) async fn restore_serially(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    states: Vec<inflight::InflightTurnState>,
) {
    use inflight::InflightTurnIdentity as Identity;
    for state in states {
        // The map's entry guard ends with this statement, so no shard lock is held across the await.
        let lock = Arc::clone(
            &shared
                .restart
                .restore_locks
                .entry(state.channel_id)
                .or_default(),
        );
        let _held = lock.lock_owned().await;
        // A run that settled or replaced the row first leaves nothing for this one; an unreadable
        // row that is still on disk keeps today's recovery.
        let gone = !inflight::inflight_state_file_exists(provider, state.channel_id);
        let current = inflight::load_inflight_state(provider, state.channel_id);
        let replaced =
            current.is_some_and(|row| Identity::from_state(&row) != Identity::from_state(&state));
        if gone || replaced {
            continue;
        }
        let restore = super::restore_inflight::restore_inflight_states;
        Box::pin(restore(http, shared, provider, vec![state])).await;
    }
}

#[cfg(test)]
#[path = "restore_serial_tests.rs"]
mod tests;
