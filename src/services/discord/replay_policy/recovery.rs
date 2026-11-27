//! Only a snapshot read under the held boot root can justify local restart settlement.
use super::super::runtime_store;
use super::row::ReplayRow;
use crate::domain::replay_evidence::recovery::{root::OwnedRuntimeRoot, snapshot::issuer};
pub use crate::domain::replay_evidence::{SnapshotIdentity, recovery::LocalSnapshot};

pub(crate) mod issuer_key;

impl LocalSnapshot {
    pub fn load<S: ReplayRow>(root: Option<&OwnedRuntimeRoot>, state: &S) -> Option<Self> {
        let root = root?;
        if !root.covers(&runtime_store::agentdesk_root()?) {
            return None;
        }
        let provider = crate::services::provider::ProviderKind::from_str(state.provider())?;
        let saved = S::load_saved(&provider, state.channel_id())?;
        if saved.user_msg_id() != state.user_msg_id() || saved.turn_nonce() != state.turn_nonce() {
            return None;
        }
        Some(issuer::loaded(
            issuer_key::LoadedSnapshotKey(()),
            root.clone(),
            SnapshotIdentity {
                replay_receipt_id: saved.replay_receipt_id(),
                provider: saved.provider(),
                channel_id: saved.channel_id(),
                user_msg_id: saved.user_msg_id(),
                request_owner_user_id: saved.request_owner_user_id(),
                user_text: saved.user_text(),
                source_message_ids: saved.source_message_ids(),
                turn_nonce: saved.turn_nonce(),
            },
            (
                saved.version(),
                S::current_version(),
                saved.runtime_kind_unknown_on_disk(),
                matches!(
                    saved.runtime_kind(),
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

#[cfg(any(test, feature = "test-support"))]
pub fn assert_snapshot_identity_for_test(
    snapshot: &LocalSnapshot,
    user_text: &str,
    incarnation: &str,
) {
    assert_eq!(snapshot.state().user_text, user_text);
    assert_eq!(snapshot.incarnation(), incarnation);
}
