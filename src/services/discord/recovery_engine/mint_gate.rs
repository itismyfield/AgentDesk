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
