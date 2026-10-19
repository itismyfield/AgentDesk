//! Read-only inflight observations for absent Codex TUI launch checks.

use super::inflight::{load_inflight_state_read_only_result, read_only_owner_snapshot};
use crate::services::provider::{CancelToken, ProviderKind, cancel_requested};
use crate::services::provider_hosting::{
    ProviderSessionDriver, resolve_provider_session_selection_with_channel,
};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::TurnEpisodeRef;
use std::sync::Arc;

pub(crate) struct InflightObs {
    channel: u64,
    tmux: String,
    kind: ObservationKind,
}

enum ObservationKind {
    NoPriorRow,
    ExactSelf {
        episode: TurnEpisodeRef,
        token: Arc<CancelToken>,
    },
}

fn local_tui(channel: u64, tmux: &str) -> bool {
    channel != 0
        && !tmux.trim().is_empty()
        && !crate::services::herdr_launch::herdr_configured_for_tui_launch(Some(channel))
        && {
            #[cfg(unix)]
            {
                crate::services::claude::is_tmux_available()
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
        && resolve_provider_session_selection_with_channel(
            &ProviderKind::Codex,
            true,
            Some(channel),
        )
        .driver
            == ProviderSessionDriver::TuiHosting
}

pub(crate) fn readiness(channel: u64, tmux: &str) -> Option<InflightObs> {
    if !local_tui(channel, tmux)
        || !matches!(
            load_inflight_state_read_only_result(&ProviderKind::Codex, channel),
            Ok(None)
        )
    {
        return None;
    }
    Some(InflightObs {
        channel,
        tmux: tmux.to_owned(),
        kind: ObservationKind::NoPriorRow,
    })
}

/// The async caller checks the mailbox once, before entering the blocking provider.
pub(crate) async fn observe_turn(
    channel: u64,
    tmux: &str,
    token: &Arc<CancelToken>,
) -> Option<InflightObs> {
    if !local_tui(channel, tmux) {
        return None;
    }
    let snapshot = read_only_owner_snapshot::codex_episode_snapshot(channel, tmux, token)
        .await
        .ok()?;
    Some(InflightObs {
        channel,
        tmux: tmux.to_owned(),
        kind: ObservationKind::ExactSelf {
            episode: snapshot.episode().clone(),
            token: Arc::clone(token),
        },
    })
}

impl InflightObs {
    pub(crate) fn matches_token(&self, current: &Arc<CancelToken>) -> bool {
        matches!(&self.kind, ObservationKind::ExactSelf { token, .. } if Arc::ptr_eq(token, current))
    }

    /// Late checks read only the row and token; they do not re-observe the mailbox actor.
    pub(crate) fn recheck(&self, channel: u64, tmux: &str) -> bool {
        if self.channel != channel || self.tmux != tmux || !local_tui(channel, tmux) {
            return false;
        }
        let Ok(row) = load_inflight_state_read_only_result(&ProviderKind::Codex, channel) else {
            return false;
        };
        match &self.kind {
            ObservationKind::NoPriorRow => row.is_none(),
            ObservationKind::ExactSelf { episode, token } => {
                !cancel_requested(Some(token.as_ref()))
                    && episode.channel_id == channel
                    && episode.user_message_id != 0
                    && episode.request_owner_id != 0
                    && !episode.turn_nonce.is_empty()
                    && token.turn_nonce() == Some(episode.turn_nonce.as_str())
                    && row.is_some_and(|row| {
                        row.provider == "codex"
                            && row.channel_id == channel
                            && row.tmux_session_name.as_deref() == Some(tmux)
                            && row.user_msg_id == episode.user_message_id
                            && row.request_owner_user_id == episode.request_owner_id
                            && row.turn_nonce.as_deref() == Some(episode.turn_nonce.as_str())
                    })
            }
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod test_setup;
