//! Once O commits a channel's adoption, the recovery row kept for that wait is judged once more.
//! A row a live turn here holds is left alone: its bridge or watcher reads ownership again itself.

use super::*;
use crate::services::discord::inflight::{self, InflightTurnState};
use std::sync::atomic::Ordering;

/// Judges `channel`'s kept row on its own task, through the gateway's client once it is cached.
pub(super) fn spawn(
    shared: &Arc<SharedData>,
    rest: Option<Arc<serenity::Http>>,
    provider: ProviderKind,
    channel: u64,
) {
    if inflight::load_inflight_state(&provider, channel).is_none() {
        return;
    }
    let shared = Arc::clone(shared);
    crate::services::discord::task_supervisor::spawn_observed("o_adoption_wake", async move {
        let http = match rest {
            Some(http) => http,
            None => gateway_http(&shared).await,
        };
        let rows = kept_rows(&shared, &provider, channel).await;
        if !rows.is_empty() {
            let restore = crate::services::discord::recovery_engine::restore_inflight_states;
            restore(&http, &shared, &provider, rows).await;
        }
    });
}

async fn gateway_http(shared: &SharedData) -> Arc<serenity::Http> {
    loop {
        if let Some(ctx) = shared.http.cached_serenity_ctx.get() {
            return Arc::clone(&ctx.http);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// The channel's row unless a live turn holds it: an active mailbox turn or a watcher on its tmux.
pub(super) async fn kept_rows(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: u64,
) -> Vec<InflightTurnState> {
    let id = ChannelId::new(channel);
    if crate::services::discord::mailbox_probe::mailbox_has_active_turn(shared, id).await {
        return Vec::new();
    }
    let watched = |row: &InflightTurnState| {
        shared.tmux_watchers.get(&id).is_some_and(|watcher| {
            row.tmux_session_name.as_deref() == Some(watcher.tmux_session_name.as_str())
                && !watcher.cancel.load(Ordering::Acquire)
        })
    };
    let row = inflight::load_inflight_state(provider, channel);
    row.into_iter().filter(|row| !watched(row)).collect()
}

#[cfg(test)]
#[path = "adoption_wake_tests.rs"]
mod tests;
