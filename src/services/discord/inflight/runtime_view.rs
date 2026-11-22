//! The inflight row as runtime replay and Codex absence checks read it, without naming this store.

use super::{InflightTurnIdentity, InflightTurnState};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::codex_absence::row::{self, CodexRow, TurnIdentity};
use crate::services::discord::replay_policy::row::ReplayRow;
use crate::services::provider::ProviderKind;

/// Installs the read-only Codex row load before any command can reach an absence check.
pub(crate) fn install_codex_absence_reader() {
    row::install(read_codex_row);
}

fn read_codex_row(channel: u64) -> Result<Option<CodexRow>, String> {
    let row = super::load_inflight_state_read_only_result(&ProviderKind::Codex, channel)?;
    Ok(row.map(|state| {
        let InflightTurnIdentity {
            user_msg_id,
            started_at,
            tmux_session_name,
            turn_start_offset,
        } = InflightTurnIdentity::from_state(&state);
        CodexRow {
            provider: state.provider,
            channel_id: state.channel_id,
            request_owner_user_id: state.request_owner_user_id,
            turn_nonce: state.turn_nonce,
            identity: TurnIdentity {
                user_msg_id,
                started_at,
                tmux_session_name,
                turn_start_offset,
            },
        }
    }))
}

impl ReplayRow for InflightTurnState {
    fn provider(&self) -> &str {
        &self.provider
    }
    fn channel_id(&self) -> u64 {
        self.channel_id
    }
    fn user_msg_id(&self) -> u64 {
        self.user_msg_id
    }
    fn request_owner_user_id(&self) -> u64 {
        self.request_owner_user_id
    }
    fn source_message_ids(&self) -> &[u64] {
        &self.source_message_ids
    }
    fn user_text(&self) -> &str {
        &self.user_text
    }
    fn turn_nonce(&self) -> Option<&str> {
        self.turn_nonce.as_deref()
    }
    fn session_key(&self) -> Option<&str> {
        self.session_key.as_deref()
    }
    fn replay_receipt_id(&self) -> Option<i64> {
        self.replay_receipt_id
    }
    fn replay_rerun_blocked(&self) -> bool {
        InflightTurnState::replay_rerun_blocked(self)
    }
    fn version(&self) -> u32 {
        self.version
    }
    fn runtime_kind(&self) -> Option<RuntimeHandoffKind> {
        self.runtime_kind
    }
    fn runtime_kind_unknown_on_disk(&self) -> bool {
        self.runtime_kind_unknown_on_disk
    }
    fn capture_json(&self) -> serde_json::Result<serde_json::Value> {
        serde_json::to_value(self)
    }
    fn load_saved(provider: &ProviderKind, channel_id: u64) -> Option<Self> {
        super::load_inflight_state(provider, channel_id)
    }
    fn current_version() -> u32 {
        super::inflight_state_version()
    }
}
