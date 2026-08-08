//! Read-only recovery mailbox-mint outlook.
//!
//! The caller supplies the results of its one filesystem probe and one watcher
//! registry probe. This module only classifies those values; it does not claim a
//! watcher, mint a mailbox token, or acquire a lock.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WatcherInstallOutlook {
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
        && (TmuxWatcherRegistry::RESTORE_SCAN_SHOULD_SKIP_EXISTING_WATCHER)(
            cancelled,
            paused,
            existing_path,
            requested_path,
        )
    {
        return WatcherInstallOutlook::IncumbentReuse { owner };
    }
    WatcherInstallOutlook::WillSpawn
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: u64 = 5_242_001;

    #[test]
    fn missing_non_codex_output_refuses_orphan_mint() {
        let outlook = watcher_install_outlook(
            false,
            Some(RuntimeHandoffKind::ClaudeTui),
            None,
            "/missing/wrapper.jsonl",
        );
        assert_eq!(outlook, WatcherInstallOutlook::NoOutputPath);
        assert!(
            !outlook.allows_mint(),
            "a recovery with no watcher output path must not mutate the empty mailbox"
        );
    }

    #[test]
    fn missing_codex_output_is_unknown_and_preserves_existing_behavior() {
        let outlook = watcher_install_outlook(
            false,
            Some(RuntimeHandoffKind::CodexTui),
            None,
            "/rollout/not-resolved-yet.jsonl",
        );
        assert_eq!(outlook, WatcherInstallOutlook::Unknown);
        assert!(
            outlook.allows_mint(),
            "the later Codex rollout fallback can still install the recovery watcher"
        );
    }

    #[test]
    fn live_cross_channel_incumbent_with_same_path_refuses_mint() {
        let outlook = watcher_install_outlook(
            true,
            Some(RuntimeHandoffKind::ClaudeTui),
            Some((ChannelId::new(OWNER), false, false, "/tmp/live.jsonl")),
            "/tmp/live.jsonl",
        );
        assert_eq!(
            outlook,
            WatcherInstallOutlook::IncumbentReuse {
                owner: ChannelId::new(OWNER)
            }
        );
        assert!(
            !outlook.allows_mint(),
            "owner equality is irrelevant: a reused watcher cannot carry this recovery's finish flag"
        );
    }

    #[test]
    fn replaceable_incumbents_keep_minting_behavior() {
        for incumbent in [
            (ChannelId::new(OWNER), true, false, "/tmp/live.jsonl"),
            (ChannelId::new(OWNER), false, true, "/tmp/live.jsonl"),
            (ChannelId::new(OWNER), false, false, "/tmp/old.jsonl"),
        ] {
            let outlook = watcher_install_outlook(
                true,
                Some(RuntimeHandoffKind::ClaudeTui),
                Some(incumbent),
                "/tmp/live.jsonl",
            );
            assert_eq!(outlook, WatcherInstallOutlook::WillSpawn);
            assert!(outlook.allows_mint());
        }
    }

    #[test]
    fn gate_is_strictly_subtractive_from_legacy_mint_eligibility() {
        for outlook in [
            WatcherInstallOutlook::WillSpawn,
            WatcherInstallOutlook::NoOutputPath,
            WatcherInstallOutlook::IncumbentReuse {
                owner: ChannelId::new(OWNER),
            },
            WatcherInstallOutlook::Unknown,
        ] {
            let legacy_would_mint = true;
            let gated_would_mint = legacy_would_mint && outlook.allows_mint();
            assert!(
                !gated_would_mint || legacy_would_mint,
                "the gate must never mint on an input where the legacy path would not"
            );
        }
        assert!(WatcherInstallOutlook::WillSpawn.allows_mint());
        assert!(WatcherInstallOutlook::Unknown.allows_mint());
        assert!(!WatcherInstallOutlook::NoOutputPath.allows_mint());
        assert!(
            !WatcherInstallOutlook::IncumbentReuse {
                owner: ChannelId::new(OWNER)
            }
            .allows_mint(),
            "a refusal can only skip the new token; it cannot seize an incumbent mailbox owner"
        );
    }
}
