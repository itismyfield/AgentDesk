//! Closed values shared by replay observation and its sealed evidence.
#[path = "replay_evidence/live.rs"]
pub(crate) mod live;
#[path = "replay_evidence/recovery.rs"]
pub(crate) mod recovery;

/// A terminal the provider itself reported, not one a wrapper synthesised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalKind {
    Done,
    Error,
}

/// Borrowed identity observed from the saved local snapshot, without recovery authority.
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
