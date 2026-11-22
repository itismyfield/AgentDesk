//! The saved Codex inflight row fields absence checks read, through the inflight store's reader.

use std::sync::OnceLock;

/// The fields `InflightTurnIdentity` compares to name one turn.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TurnIdentity {
    pub(crate) user_msg_id: u64,
    pub(crate) started_at: String,
    pub(crate) tmux_session_name: Option<String>,
    pub(crate) turn_start_offset: Option<u64>,
}

pub(crate) struct CodexRow {
    pub(crate) provider: String,
    pub(crate) channel_id: u64,
    pub(crate) request_owner_user_id: u64,
    pub(crate) turn_nonce: Option<String>,
    pub(crate) identity: TurnIdentity,
}

type Reader = fn(u64) -> Result<Option<CodexRow>, String>;

static READER: OnceLock<Reader> = OnceLock::new();

/// Installs the inflight store's read-only load; the first install wins.
pub(crate) fn install(read: Reader) {
    let _ = READER.set(read);
}

/// The channel's saved Codex row, read without repair; an uninstalled reader reads as an error.
pub(super) fn load_read_only(channel: u64) -> Result<Option<CodexRow>, String> {
    #[cfg(test)]
    crate::services::discord::install_codex_absence_reader();
    let read = READER.get().ok_or("codex inflight reader not installed")?;
    read(channel)
}
