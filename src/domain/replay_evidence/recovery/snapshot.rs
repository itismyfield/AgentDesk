//! Only a completed local observation can issue a snapshot with retained root custody.
use super::root::OwnedRuntimeRoot;
use crate::domain::replay_evidence::SnapshotIdentity;

#[path = "../../../services/replay_evidence/recovery_issuer.rs"]
pub(crate) mod issuer;

struct OwnedIdentity {
    replay_receipt_id: Option<i64>,
    provider: String,
    channel_id: u64,
    user_msg_id: u64,
    request_owner_user_id: u64,
    user_text: String,
    source_message_ids: Vec<u64>,
    turn_nonce: Option<String>,
}

pub(crate) struct LocalSnapshot {
    _root: OwnedRuntimeRoot,
    identity: OwnedIdentity,
    format: (u32, u32, bool, bool),
    instance: String,
    incarnation: String,
}

impl LocalSnapshot {
    pub(crate) fn understood(&self) -> bool {
        let (version, current_version, unknown_runtime, native_runtime) = self.format;
        !unknown_runtime && version <= current_version && !native_runtime
    }

    pub(crate) fn state(&self) -> SnapshotIdentity<'_> {
        SnapshotIdentity {
            replay_receipt_id: self.identity.replay_receipt_id,
            provider: &self.identity.provider,
            channel_id: self.identity.channel_id,
            user_msg_id: self.identity.user_msg_id,
            request_owner_user_id: self.identity.request_owner_user_id,
            user_text: &self.identity.user_text,
            source_message_ids: &self.identity.source_message_ids,
            turn_nonce: self.identity.turn_nonce.as_deref(),
        }
    }

    pub(crate) fn instance(&self) -> &str {
        &self.instance
    }

    pub(crate) fn incarnation(&self) -> &str {
        &self.incarnation
    }
}
