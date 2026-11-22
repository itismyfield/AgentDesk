//! The saved inflight turn fields replay decisions read; the inflight store implements it.

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider::ProviderKind;

pub(crate) trait ReplayRow: Sync {
    fn provider(&self) -> &str;
    fn channel_id(&self) -> u64;
    fn user_msg_id(&self) -> u64;
    fn request_owner_user_id(&self) -> u64;
    fn source_message_ids(&self) -> &[u64];
    fn user_text(&self) -> &str;
    fn turn_nonce(&self) -> Option<&str>;
    fn session_key(&self) -> Option<&str>;
    fn replay_receipt_id(&self) -> Option<i64>;
    fn replay_rerun_blocked(&self) -> bool;
    fn version(&self) -> u32;
    fn runtime_kind(&self) -> Option<RuntimeHandoffKind>;
    fn runtime_kind_unknown_on_disk(&self) -> bool;
    /// The whole row as the hold preserves it.
    fn capture_json(&self) -> serde_json::Result<serde_json::Value>;
    /// The row saved on disk for this provider and channel.
    fn load_saved(provider: &ProviderKind, channel_id: u64) -> Option<Self>
    where
        Self: Sized;
    /// The row version this build writes.
    fn current_version() -> u32
    where
        Self: Sized;
}
