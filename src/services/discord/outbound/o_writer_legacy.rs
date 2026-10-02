//! Legacy's Claude relay state in this process as the O writer's adoption reads it: cursors, the
//! delivered frontier and running response tails. Nothing here decides or writes.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::super::SharedData;
use super::delivery_record as dr;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::writer::adoption::{LegacyCursor, LegacyEpoch, LegacyView};

pub(in crate::services::discord) struct LegacyRelay {
    shared: Arc<SharedData>,
}

impl LegacyRelay {
    pub(in crate::services::discord) fn new(shared: Arc<SharedData>) -> Self {
        Self { shared }
    }
}

impl LegacyView for LegacyRelay {
    fn started(&self) -> bool {
        crate::services::claude_tui::hook_server::boot_discovery_done()
    }

    fn cursor(&self, tmux: &str) -> LegacyCursor {
        let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux);
        match binding.filter(|binding| binding.runtime_kind == RuntimeHandoffKind::ClaudeTui) {
            Some(binding) => LegacyCursor::Bound {
                path: binding.output_path.into(),
                offset: binding.last_offset,
            },
            None if crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux) => {
                LegacyCursor::Unbound
            }
            None => LegacyCursor::NoPane,
        }
    }

    /// The floor the idle response tail takes, over the same arguments.
    fn frontier(&self, channel: u64, tmux: &str, eof: u64) -> Option<u64> {
        let channel = ChannelId::new(channel);
        let claude = &ProviderKind::Claude;
        dr::delivery_record_authority_enabled()
            .then(|| dr::effective_committed_offset(&self.shared, claude, channel, tmux, Some(eof)))
    }

    fn tail_running(&self, tmux: &str) -> bool {
        super::super::tui_prompt_relay::claude_idle_tail_running(tmux)
    }

    /// What `relay_auto_heal` restarts a redrive episode on, read without writing the inflight row.
    fn epoch(&self, channel: u64) -> LegacyEpoch {
        let id = ChannelId::new(channel);
        let row =
            super::super::inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel);
        LegacyEpoch {
            reset_incarnation: self.shared.relay_frontier_token(id).reset_incarnation,
            reconnects: (self.shared.tmux_relay_coord(id).reconnect_count)
                .load(std::sync::atomic::Ordering::Acquire),
            turn: row.map(|row| {
                let nonce = row.turn_nonce.filter(|nonce| !nonce.is_empty());
                let start = row.turn_start_offset;
                (
                    row.user_msg_id,
                    row.started_at,
                    row.tmux_session_name,
                    start,
                    nonce,
                )
            }),
        }
    }
}
