//! Once O's adopted writer turns ready, the recovery row kept for the adoption is judged again.
//! A row a live turn here holds is left alone: its bridge or watcher reads ownership again itself.

use super::*;
use crate::services::discord::inflight::{self, InflightTurnState};
use crate::services::discord::recovery_paths::shared::RecoveryRelayOutcome;
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

/// Retired rowless debts still wake this runtime's existing queue; custody is rechecked there.
pub(super) async fn boot(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    shadow: ShadowProvider,
) -> Vec<u64> {
    #[cfg(test)]
    tests::boot_scanned();
    if &shared.provider != provider || shared.restart.shutting_down.load(Ordering::Acquire) {
        return Vec::new();
    }
    let Some(root) = crate::config::runtime_root() else {
        return Vec::new();
    };
    let Some(snapshot) = crate::services::tui_o::cutover::boot_snapshot() else {
        return Vec::new();
    };
    let gate = crate::services::tui_o::ownership::gate(provider.as_str());
    let gateway = if let Some(registry) = shared.health_registry.upgrade() {
        let entries = registry.provider_entries_snapshot().await;
        let mut owners = entries.iter().filter(|entry| {
            entry.role == health::ProviderRuntimeRole::Gateway
                && entry.name.eq_ignore_ascii_case(provider.as_str())
        });
        owners
            .next()
            .is_some_and(|entry| Arc::ptr_eq(&entry.shared, shared))
            && owners.next().is_none()
            && matches!(
                gate.current(),
                crate::services::tui_o::ownership::GatewayOwnership::Owned { .. }
            )
    } else {
        false
    };
    let kind = match shadow {
        ShadowProvider::Claude => crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        ShadowProvider::Codex => crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
    };
    let mut channels = Vec::new();
    crate::services::tui_o::writer::herdr_body_debt::wake_channels(&root, shadow, |channel| {
        channels.push(channel)
    });
    let mut woken = Vec::new();
    for channel in channels {
        let authorized = match snapshot.execution(channel) {
            Some(view) => match view.permit {
                Ok(permit) => {
                    let local = super::local_id(snapshot.configured_id(), Duration::ZERO).await;
                    let observed = permit.gate().current();
                    let owns = matches!(observed, crate::services::tui_o::ownership::GatewayOwnership::ExecutionOwned { generation, .. } if generation == permit.generation);
                    let current = if owns && local.as_deref() == Ok(permit.executor.as_str()) {
                        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND owner_instance_id=$3 AND generation=$4 AND status='active' AND routing_cleanup IS NULL)")
                            .bind(provider.as_str()).bind(channel.to_string()).bind(&permit.executor).bind(permit.generation)
                            .fetch_one(&permit.pool).await.unwrap_or(false)
                    } else {
                        false
                    };
                    current && permit.gate().current() == observed
                }
                Err(_) => false,
            },
            None => {
                gateway
                    && matches!(
                        gate.current(),
                        crate::services::tui_o::ownership::GatewayOwnership::Owned { .. }
                    )
            }
        };
        if authorized
            && crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
                channel,
                Some(kind),
            ) == Ok(true)
        {
            crate::services::discord::queue_io::schedule_writer_ready_queue_kick(
                Arc::clone(shared),
                provider.clone(),
                ChannelId::new(channel),
            );
            woken.push(channel);
        }
    }
    woken
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
        // A wake's failed send spends no restart budget: the row goes in uncounted, the recovery's
        // own outcome decides a retry, and a count it persisted for this run is put back.
        let mut uncounted = row.clone();
        uncounted.recovery_relay_attempts = 0;
        shared.restart.wake_outcomes.insert(channel, None);
        let restore = crate::services::discord::recovery_engine::restore_serially;
        restore(http, shared, provider, vec![uncounted]).await;
        let outcome = shared
            .restart
            .wake_outcomes
            .remove(&channel)
            .and_then(|(_, o)| o);
        let transient = outcome == Some(RecoveryRelayOutcome::TransientFailure);
        let same =
            |now: &InflightTurnState| Identity::from_state(now) == Identity::from_state(&row);
        let Some(mut now) = inflight::load_inflight_state(provider, channel).filter(same) else {
            break;
        };
        if now.recovery_relay_attempts != row.recovery_relay_attempts {
            now.recovery_relay_attempts = row.recovery_relay_attempts;
            let reason = "o_adoption_wake_uncounted";
            let _ = inflight::save_inflight_state_if_identity_unchanged(&mut now, reason);
        }
        if !transient {
            break;
        }
    }
    // A queued input that gave its claim back while the row stood runs once the row is gone.
    if claimed && inflight::load_inflight_state(provider, channel).is_none() {
        #[cfg(test)]
        tests::kicked();
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
