//! A two-message panel opens before O posts the turn's body, so for a bounded window after the
//! turn ends its completed panel is sent again below O's newest post and the old one deleted.

use std::time::Duration;

use super::super::terminal_controller_cutover::bridge_o_body_peek_decision;
use super::super::*;
use crate::services::discord::status_panel_orphan_store as orphans;
use crate::services::discord::{status_panel_singleton_store as singleton, task_supervisor};

/// How long after completion a late O post still moves the panel below it.
const FOLLOW_WINDOW: Duration = if cfg!(test) {
    Duration::from_secs(3)
} else {
    Duration::from_secs(30)
};
const POLL: Duration = Duration::from_millis(250);
/// A move waits until O has posted nothing new for this long, so a split body moves it once.
const QUIET: Duration = Duration::from_millis(if cfg!(test) { 500 } else { 2_000 });
/// Each move is one POST and one DELETE; the last one is kept for the window's final check.
const MAX_MOVES: usize = 3;

type Owner<'a> = (
    &'a Arc<SharedData>,
    &'a Arc<dyn TurnGateway>,
    &'a ProviderKind,
    ChannelId,
);

/// Starts the follow when this completed turn's channel is O's and keeps a two-message panel.
pub(in crate::services::discord::turn_bridge) fn follow(
    (shared, gateway, provider, channel_id): Owner<'_>,
    inflight_state: &InflightTurnState,
    completed_text: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    let o_owned = || {
        bridge_o_body_peek_decision(channel_id, inflight_state, gateway.can_deliver_directly())
            == Ok(true)
    };
    if !(shared.ui.two_message_panel_enabled && shared.ui.status_panel_v2_enabled && o_owned()) {
        return None;
    }
    let (shared, gateway, provider) = (Arc::clone(shared), Arc::clone(gateway), provider.clone());
    let (user_msg_id, text) = (inflight_state.user_msg_id, completed_text.to_string());
    Some(task_supervisor::spawn_observed(
        "turn_bridge_o_completed_panel_follow",
        async move {
            follow_posts(
                &shared,
                gateway.as_ref(),
                &provider,
                channel_id,
                user_msg_id,
                &text,
            )
            .await
        },
    ))
}

async fn follow_posts(
    shared: &SharedData,
    gateway: &dyn TurnGateway,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: u64,
    text: &str,
) {
    let channel = channel_id.get();
    let Some(mut panel) = singleton::load(provider, &shared.token_hash, channel)
        .map(|b| MessageId::new(b.panel_message_id))
    else {
        return;
    };
    let deadline = tokio::time::Instant::now() + FOLLOW_WINDOW;
    let (mut moves, mut seen, mut quiet_since) = (0, None, tokio::time::Instant::now());
    loop {
        let last_check = tokio::time::Instant::now() >= deadline;
        if !last_check {
            tokio::time::sleep(POLL).await;
        }
        let posted = crate::services::tui_o::writer::deliver::last_posted(channel);
        if posted != seen {
            (seen, quiet_since) = (posted, tokio::time::Instant::now());
        }
        let settled = moves + 1 < MAX_MOVES && quiet_since.elapsed() >= QUIET;
        if posted.is_some_and(|posted| posted > panel.get()) && (settled || last_check) {
            let target = (shared, gateway, provider, channel_id);
            match move_below(target, user_msg_id, panel, text).await {
                Some(Ok(next)) => (panel, moves) = (next, moves + 1),
                Some(Err(())) => return,
                None => {}
            }
        }
        if last_check {
            return;
        }
    }
}

/// One move: None while this turn's row is still closing, `Err` once the panel is no longer ours.
async fn move_below(
    (shared, gateway, provider, channel_id): (
        &SharedData,
        &dyn TurnGateway,
        &ProviderKind,
        ChannelId,
    ),
    user_msg_id: u64,
    panel: MessageId,
    text: &str,
) -> Option<Result<MessageId, ()>> {
    let (channel, token) = (channel_id.get(), shared.token_hash.as_str());
    match inflight::load_inflight_state_read_only(provider, channel) {
        Some(row) if row.user_msg_id == user_msg_id => return None,
        Some(_) => return Some(Err(())),
        None => {}
    }
    if singleton::load(provider, token, channel).map(|b| b.panel_message_id) != Some(panel.get()) {
        return Some(Err(()));
    }
    let Ok(next) = TurnGateway::send_message(gateway, channel_id, text).await else {
        return Some(Err(()));
    };
    orphans::enqueue_pending_bind(provider, token, channel, next.get(), None);
    if let Err(error) =
        singleton::move_completed_if_current(provider, token, channel, panel.get(), next.get())
    {
        tracing::info!(channel, panel = panel.get(), %error, "completed O status panel stays put");
        if gateway.delete_message(channel_id, next).await.is_ok() {
            orphans::remove(provider, token, channel, next.get());
        }
        return Some(Err(()));
    }
    orphans::remove(provider, token, channel, next.get());
    if gateway.delete_message(channel_id, panel).await.is_err() {
        orphans::enqueue(provider, token, channel, panel.get());
    }
    Some(Ok(next))
}

#[cfg(test)]
#[path = "o_panel_below_tests.rs"]
mod tests;
