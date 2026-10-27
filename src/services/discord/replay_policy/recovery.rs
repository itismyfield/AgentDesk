//! Only a snapshot read under the held boot root can justify local restart settlement.
use super::super::{inflight, runtime_store};
use crate::cli::dcserver::replay_recovery::OwnedRuntimeRoot;

pub(crate) struct LocalSnapshot {
    _root: OwnedRuntimeRoot,
    state: inflight::InflightTurnState,
    instance: String,
}

pub(crate) struct SnapshotIdentity<'a> {
    pub(crate) replay_receipt_id: Option<i64>,
    pub(crate) provider: &'a str,
    pub(crate) channel_id: u64,
    pub(crate) user_msg_id: u64,
    pub(crate) request_owner_user_id: u64,
    pub(crate) user_text: &'a str,
    pub(crate) source_message_ids: &'a [u64],
    pub(crate) turn_nonce: Option<&'a str>,
}

impl LocalSnapshot {
    pub(in crate::services::discord) fn load(
        root: Option<&OwnedRuntimeRoot>,
        state: &inflight::InflightTurnState,
    ) -> Option<Self> {
        let root = root?;
        if !root.covers(&runtime_store::agentdesk_root()?) {
            return None;
        }
        let provider = crate::services::provider::ProviderKind::from_str(&state.provider)?;
        let saved = inflight::load_inflight_state(&provider, state.channel_id)?;
        if saved.user_msg_id != state.user_msg_id || saved.turn_nonce != state.turn_nonce {
            return None;
        }
        Some(Self {
            _root: root.clone(),
            state: saved,
            instance:
                crate::services::cluster::node_registry::resolve_self_instance_id_without_config(),
        })
    }

    pub(crate) fn understood(&self) -> bool {
        !self.state.runtime_kind_unknown_on_disk
            && self.state.version <= inflight::inflight_state_version()
            && !matches!(
                self.state.runtime_kind,
                Some(
                    crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
                        | crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
                )
            )
    }
    pub(crate) fn state(&self) -> SnapshotIdentity<'_> {
        SnapshotIdentity {
            replay_receipt_id: self.state.replay_receipt_id,
            provider: &self.state.provider,
            channel_id: self.state.channel_id,
            user_msg_id: self.state.user_msg_id,
            request_owner_user_id: self.state.request_owner_user_id,
            user_text: &self.state.user_text,
            source_message_ids: &self.state.source_message_ids,
            turn_nonce: self.state.turn_nonce.as_deref(),
        }
    }
    pub(crate) fn instance(&self) -> &str {
        &self.instance
    }
}

#[cfg(all(test, unix))]
#[path = "recovery_tests.rs"]
mod tests;
