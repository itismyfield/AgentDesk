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
/// Each move is one POST and one DELETE; a turn never moves its panel more often than this.
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
    let (channel, token) = (channel_id.get(), shared.token_hash.as_str());
    let Some(mut panel) = singleton::load(provider, token, channel).map(|b| b.panel_message_id)
    else {
        return;
    };
    let deadline = tokio::time::Instant::now() + FOLLOW_WINDOW;
    let mut moves = 0;
    while moves < MAX_MOVES && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(POLL).await;
        let posted = crate::services::tui_o::writer::deliver::last_posted(channel);
        if posted.is_none_or(|posted| posted <= panel) {
            continue;
        }
        // The turn's own row is still closing; a newer turn owns the next panel.
        match inflight::load_inflight_state_read_only(provider, channel) {
            Some(row) if row.user_msg_id == user_msg_id => continue,
            Some(_) => return,
            None => {}
        }
        if singleton::load(provider, token, channel).map(|b| b.panel_message_id) != Some(panel) {
            return;
        }
        let Ok(next) = TurnGateway::send_message(gateway, channel_id, text).await else {
            return;
        };
        orphans::enqueue_pending_bind(provider, token, channel, next.get(), None);
        if let Err(error) =
            singleton::move_completed_if_current(provider, token, channel, panel, next.get())
        {
            tracing::info!(channel, panel, %error, "completed O status panel stays put");
            if gateway.delete_message(channel_id, next).await.is_ok() {
                orphans::remove(provider, token, channel, next.get());
            }
            return;
        }
        orphans::remove(provider, token, channel, next.get());
        if gateway
            .delete_message(channel_id, MessageId::new(panel))
            .await
            .is_err()
        {
            orphans::enqueue(provider, token, channel, panel);
        }
        panel = next.get();
        moves += 1;
    }
}

#[cfg(test)]
#[path = "o_panel_below_tests.rs"]
mod tests;
