use serde::Serialize;

use crate::services::discord::relay_health::{RelayHealthSnapshot, RelayStallState};

#[derive(Debug, Serialize)]
pub(super) struct MailboxHealthSnapshot {
    pub(super) provider: String,
    pub(super) channel_id: u64,
    pub(super) has_cancel_token: bool,
    pub(super) queue_depth: usize,
    pub(super) recovery_started: bool,
    pub(super) active_request_owner: Option<u64>,
    pub(super) active_user_message_id: Option<u64>,
    pub(super) agent_turn_status: &'static str,
    pub(super) watcher_attached: bool,
    pub(super) inflight_state_present: bool,
    pub(super) tmux_present: bool,
    pub(super) process_present: bool,
    pub(super) active_dispatch_present: bool,
    pub(super) relay_stall_state: RelayStallState,
    pub(super) relay_health: RelayHealthSnapshot,
}
