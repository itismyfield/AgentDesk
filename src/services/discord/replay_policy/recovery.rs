//! Only a snapshot read under the held boot root can justify local restart settlement.
use super::super::{inflight, runtime_store};
use crate::domain::replay_evidence::recovery::{root::OwnedRuntimeRoot, snapshot::issuer};
pub(crate) use crate::domain::replay_evidence::{SnapshotIdentity, recovery::LocalSnapshot};

pub(crate) mod issuer_key;

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
        Some(issuer::loaded(
            issuer_key::LoadedSnapshotKey(()),
            root.clone(),
            SnapshotIdentity {
                replay_receipt_id: saved.replay_receipt_id,
                provider: &saved.provider,
                channel_id: saved.channel_id,
                user_msg_id: saved.user_msg_id,
                request_owner_user_id: saved.request_owner_user_id,
                user_text: &saved.user_text,
                source_message_ids: &saved.source_message_ids,
                turn_nonce: saved.turn_nonce.as_deref(),
            },
            (
                saved.version,
                inflight::inflight_state_version(),
                saved.runtime_kind_unknown_on_disk,
                matches!(
                    saved.runtime_kind,
                    Some(
                        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
                            | crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
                    )
                ),
            ),
            crate::services::cluster::node_registry::resolve_self_instance_id_without_config(),
            crate::domain::process_incarnation::incarnation(),
        ))
    }
}

#[cfg(all(test, unix))]
#[path = "recovery_tests.rs"]
mod tests;
