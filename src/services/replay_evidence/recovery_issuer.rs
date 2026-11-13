//! The saved snapshot loader's private key authorizes this owned observation.
use super::{LocalSnapshot, OwnedIdentity};
use crate::domain::replay_evidence::{SnapshotIdentity, recovery::root::OwnedRuntimeRoot};
use crate::services::discord::replay_policy::recovery::issuer_key::LoadedSnapshotKey;

pub(crate) fn loaded(
    _key: LoadedSnapshotKey,
    root: OwnedRuntimeRoot,
    identity: SnapshotIdentity<'_>,
    format: (u32, u32, bool, bool),
    instance: String,
    incarnation: &str,
) -> LocalSnapshot {
    LocalSnapshot {
        _root: root,
        identity: OwnedIdentity {
            replay_receipt_id: identity.replay_receipt_id,
            provider: identity.provider.to_owned(),
            channel_id: identity.channel_id,
            user_msg_id: identity.user_msg_id,
            request_owner_user_id: identity.request_owner_user_id,
            user_text: identity.user_text.to_owned(),
            source_message_ids: identity.source_message_ids.to_vec(),
            turn_nonce: identity.turn_nonce.map(str::to_owned),
        },
        format,
        instance,
        incarnation: incarnation.to_owned(),
    }
}
