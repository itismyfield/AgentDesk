//! Read-only recovery watcher-install outlook.
//!
//! Callers supply one filesystem observation and one watcher-registry observation.
//! This module only classifies those values; it neither claims a watcher nor mints
//! a mailbox token. Registry references are reduced to owned scalar values before
//! any later `.await`, so no DashMap shard guard crosses an await point.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum WatcherInstallOutlook {
    WillSpawn,
    NoOutputPath,
    IncumbentReuse { owner: ChannelId },
    Unknown,
}

impl WatcherInstallOutlook {
    pub(super) fn allows_mint(self) -> bool {
        matches!(self, Self::WillSpawn | Self::Unknown)
    }

    pub(super) fn refusal(self) -> Option<(&'static str, Option<ChannelId>)> {
        match self {
            Self::NoOutputPath => Some(("no_output_path", None)),
            Self::IncumbentReuse { owner } => Some(("incumbent_reuse", Some(owner))),
            Self::WillSpawn | Self::Unknown => None,
        }
    }
}

pub(super) fn watcher_install_outlook(
    path_exists: bool,
    runtime_kind: Option<RuntimeHandoffKind>,
    incumbent: Option<(ChannelId, bool, bool, &str)>,
    requested_path: &str,
) -> WatcherInstallOutlook {
    if !path_exists {
        return if runtime_kind == Some(RuntimeHandoffKind::CodexTui) {
            WatcherInstallOutlook::Unknown
        } else {
            WatcherInstallOutlook::NoOutputPath
        };
    }
    if let Some((owner, cancelled, paused, existing_path)) = incumbent
        && !cancelled
        && !paused
        && existing_path == requested_path
    {
        return WatcherInstallOutlook::IncumbentReuse { owner };
    }
    WatcherInstallOutlook::WillSpawn
}

pub(super) async fn reregister_active_turn_for_output_path(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    state: &inflight::InflightTurnState,
    tmux_session_name: &str,
    output_path: &str,
) -> (bool, WatcherInstallOutlook) {
    let path_exists = std::fs::metadata(output_path).is_ok();
    let outlook = super::recovery_mint_outlook(
        shared,
        state,
        Some(tmux_session_name),
        output_path,
        path_exists,
    );
    // Refusal suppresses only token mint. Runtime recovery still reseeds the
    // finalizer and attempts the readoption marker write.
    let started = if !outlook.allows_mint() {
        super::reregister_active_turn_from_inflight_with_outlook(shared, state, outlook).await
    } else {
        super::reregister_active_turn_from_inflight(shared, state).await
    };
    #[cfg(unix)]
    super::guard_readopt_relay_resume_or_dead_letter(shared, provider, channel_id);
    (started, outlook)
}

pub(super) async fn reregister_active_turn_for_restart_report(
    shared: &Arc<SharedData>,
    state: &inflight::InflightTurnState,
    tmux_session_name: Option<&str>,
    watcher_start: Option<&(String, u64, u64, bool)>,
) -> (bool, WatcherInstallOutlook) {
    let outlook = watcher_start.map_or(WatcherInstallOutlook::NoOutputPath, |(output_path, ..)| {
        super::recovery_mint_outlook(shared, state, tmux_session_name, output_path, true)
    });
    // Site-specific outer guard; `&& false` is a required mutation target.
    let started = if !outlook.allows_mint() {
        super::reregister_active_turn_from_inflight_with_outlook(shared, state, outlook).await
    } else {
        super::reregister_active_turn_from_inflight(shared, state).await
    };
    (started, outlook)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_value_watcher_install_policy_is_exact() {
        let owner = ChannelId::new(5_242_701);
        let cases = [
            (
                "missing Codex path remains unresolved",
                false,
                Some(RuntimeHandoffKind::CodexTui),
                None,
                WatcherInstallOutlook::Unknown,
            ),
            (
                "missing non-Codex path refuses mint",
                false,
                Some(RuntimeHandoffKind::ClaudeTui),
                None,
                WatcherInstallOutlook::NoOutputPath,
            ),
            (
                "healthy same-path incumbent is reused",
                true,
                Some(RuntimeHandoffKind::ClaudeTui),
                Some((owner, false, false, "/tmp/requested.jsonl")),
                WatcherInstallOutlook::IncumbentReuse { owner },
            ),
            (
                "different-path incumbent cannot be reused",
                true,
                Some(RuntimeHandoffKind::ClaudeTui),
                Some((owner, false, false, "/tmp/other.jsonl")),
                WatcherInstallOutlook::WillSpawn,
            ),
            (
                "cancelled same-path incumbent is replaced",
                true,
                Some(RuntimeHandoffKind::ClaudeTui),
                Some((owner, true, false, "/tmp/requested.jsonl")),
                WatcherInstallOutlook::WillSpawn,
            ),
            (
                "present path without incumbent spawns",
                true,
                Some(RuntimeHandoffKind::CodexTui),
                None,
                WatcherInstallOutlook::WillSpawn,
            ),
        ];

        for (name, path_exists, runtime_kind, incumbent, expected) in cases {
            assert_eq!(
                watcher_install_outlook(
                    path_exists,
                    runtime_kind,
                    incumbent,
                    "/tmp/requested.jsonl",
                ),
                expected,
                "{name}"
            );
        }
    }
}
