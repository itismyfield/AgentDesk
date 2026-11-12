//! Once O's adopted writer turns ready, the recovery row kept for the adoption is judged again.
//! A row a live turn here holds is left alone: its bridge or watcher reads ownership again itself.

use super::*;
use crate::services::discord::inflight::{self, InflightTurnState};
use std::sync::atomic::Ordering;

/// Judgements after the first, when a claim held the row or its send failed transiently.
const RETRIES: u32 = 3;
const BACKOFF: Duration = Duration::from_secs(1);

/// Why a live turn here keeps the wake off the row.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Held {
    /// An active mailbox turn, possibly a queued input's claim that is about to let go.
    Claimed,
    /// A watcher on the row's tmux closes that turn itself.
    Watched,
}

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
        wake(&http, &shared, &provider, channel).await;
    });
}

/// Judges the kept row once, then again up to `RETRIES` times with a growing backoff while a
/// claim holds it or its recovery send failed transiently; never longer than that.
async fn wake(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: u64,
) {
    use inflight::InflightTurnIdentity as Identity;
    let mut claimed = false;
    for attempt in 0..=RETRIES {
        if attempt > 0 {
            tokio::time::sleep(BACKOFF * attempt).await;
        }
        let judged = kept_rows(shared, provider, channel).await;
        #[cfg(test)]
        tests::judged(&judged);
        let row = match judged {
            Ok(rows) => rows.into_iter().next(),
            Err(Held::Claimed) => {
                claimed = true;
                continue;
            }
            Err(Held::Watched) => return,
        };
        let Some(row) = row else { break };
        let restore = crate::services::discord::recovery_engine::restore_serially;
        restore(http, shared, provider, vec![row.clone()]).await;
        let after = inflight::load_inflight_state(provider, channel);
        let transient = after.is_some_and(|now| {
            Identity::from_state(&now) == Identity::from_state(&row)
                && now.recovery_relay_attempts > row.recovery_relay_attempts
        });
        if !transient {
            break;
        }
    }
    // A queued input that gave its claim back while the row stood runs once the row is gone.
    if claimed && inflight::load_inflight_state(provider, channel).is_none() {
        let kick = crate::services::discord::queue_io::schedule_writer_ready_queue_kick;
        kick(
            Arc::clone(shared),
            provider.clone(),
            ChannelId::new(channel),
        );
    }
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
) -> Result<Vec<InflightTurnState>, Held> {
    let id = ChannelId::new(channel);
    if crate::services::discord::mailbox_probe::mailbox_has_active_turn(shared, id).await {
        return Err(Held::Claimed);
    }
    let watched = |row: &InflightTurnState| {
        shared.tmux_watchers.get(&id).is_some_and(|watcher| {
            row.tmux_session_name.as_deref() == Some(watcher.tmux_session_name.as_str())
                && !watcher.cancel.load(Ordering::Acquire)
        })
    };
    match inflight::load_inflight_state(provider, channel) {
        Some(row) if watched(&row) => Err(Held::Watched),
        row => Ok(row.into_iter().collect()),
    }
}

#[cfg(test)]
#[path = "adoption_wake_tests.rs"]
mod tests;
