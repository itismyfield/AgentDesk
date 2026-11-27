//! The saved Codex inflight row fields absence checks read, through the inflight store's reader.

use std::sync::OnceLock;

/// The fields `InflightTurnIdentity` compares to name one turn.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnIdentity {
    pub user_msg_id: u64,
    pub started_at: String,
    pub tmux_session_name: Option<String>,
    pub turn_start_offset: Option<u64>,
}

pub struct CodexRow {
    pub provider: String,
    pub channel_id: u64,
    pub request_owner_user_id: u64,
    pub turn_nonce: Option<String>,
    pub identity: TurnIdentity,
}

type Reader = fn(u64) -> Result<Option<CodexRow>, String>;

static READER: OnceLock<Reader> = OnceLock::new();

/// Installs the inflight store's read-only load; the first install wins.
pub fn install(read: Reader) {
    let _ = READER.set(read);
}

/// The channel's saved Codex row, read without repair; an uninstalled reader reads as an error.
pub(super) fn load_read_only(channel: u64) -> Result<Option<CodexRow>, String> {
    let read = READER.get();
    // Test builds panic instead: a test reaching this port installs it at its own entry point.
    #[cfg(any(test, feature = "test-support"))]
    let read = Some(read.expect("codex inflight reader not installed"));
    let read = read.ok_or("codex inflight reader not installed")?;
    read(channel)
}
