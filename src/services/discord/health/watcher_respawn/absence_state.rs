use super::*;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct WatcherAbsenceKey {
    pub(super) provider: String,
    pub(super) channel_id: u64,
}

#[derive(Clone, Debug)]
pub(super) struct WatcherAbsenceState {
    pub(super) first_seen_unix_secs: i64,
    pub(super) last_seen_unix_secs: i64,
    pub(super) escalated: bool,
    /// Routing observations warn independently of the snapshot dead-man escalation.
    pub(super) idle_relay_announced: bool,
    pub(super) minimum_initial_offset: Option<u64>,
    pub(super) failed_attempts: u32,
    pub(super) next_attempt_unix_secs: i64,
    pub(super) retirement_logged: bool,
    #[cfg(unix)]
    pub(super) o_install_reason: Option<discord::tmux::OOnlyInstallReason>,
}

impl WatcherAbsenceState {
    pub(super) fn newly_absent(now_unix_secs: i64) -> Self {
        Self {
            first_seen_unix_secs: now_unix_secs,
            last_seen_unix_secs: now_unix_secs,
            escalated: false,
            idle_relay_announced: false,
            minimum_initial_offset: None,
            failed_attempts: 0,
            next_attempt_unix_secs: now_unix_secs,
            retirement_logged: false,
            #[cfg(unix)]
            o_install_reason: None,
        }
    }
}

/// Delay before the next respawn attempt after `failed_attempts` consecutive
/// failures.
pub(super) fn respawn_backoff_secs(failed_attempts: u32) -> u64 {
    let doublings = failed_attempts.saturating_sub(1).min(5);
    (WATCHER_RESPAWN_BACKOFF_BASE_SECS << doublings).min(WATCHER_RESPAWN_BACKOFF_CEILING_SECS)
}

impl WatcherAbsenceKey {
    pub(super) fn new(provider: &ProviderKind, channel_id: ChannelId) -> Self {
        Self {
            provider: provider.as_str().to_string(),
            channel_id: channel_id.get(),
        }
    }
}
