use super::*;
use crate::services::discord::input_runtime::fence::{BootTarget, boot_skip};

#[path = "lifecycle/activity.rs"]
mod activity;
pub(super) use self::activity::maybe_refresh_watcher_activity_heartbeat;
#[allow(unused_imports)]
pub(in crate::services::discord) use self::activity::{
    HeartbeatRefreshMatch, HeartbeatRefreshOutcome, refresh_session_heartbeat_from_tmux_output,
    refresh_session_heartbeat_from_tmux_output_detailed, touch_session_activity,
};

#[path = "codex_tui_restore.rs"]
mod codex_restore;
#[cfg(all(test, unix))]
pub(crate) use codex_restore::commit_codex_watcher_restore_for_tests;
#[cfg(all(test, unix))]
pub(crate) use codex_restore::commit_codex_watcher_restore_to_empty_registry_for_tests;
#[path = "dispatched_origin_ghost.rs"]
mod dispatched_origin_ghost;
use dispatched_origin_ghost::consume_dispatched_origin_ghost_if_current;
#[cfg(test)]
#[path = "dispatched_origin_ghost_tests.rs"]
mod dispatched_origin_ghost_tests;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum LivenessProbeOutcome {
    /// No dead marker observed; the tmux pane liveness check answered.
    PaneCheckOnly { alive: bool },
    /// Both the dead marker and the live pane exist — the marker is stale
    /// (e.g. a prior wrapper recorded its own death but the session has
    /// been respawned, or `POST /api/inflight/rebind` adopted an
    /// externally-owned tmux session whose previous watcher marked the
    /// pane dead). Callers should remove the marker and treat the session
    /// as live; otherwise the watcher short-circuits to dead in its first
    /// poll and defeats the rebind forward-only relay contract.
    StaleMarkerClearAndAlive,
    /// Dead marker present and the pane really is gone — honour the marker.
    MarkerHonoredDead,
}

/// #2853 — for claude_tui sessions whose AgentDesk-side relay JSONL never lands
/// on disk (claude TUI writes its rollout to `~/.claude/projects/<cwd>/<uuid>.jsonl`),
/// fall back to the freshest Claude rollout transcript under the launched
/// session cwd; otherwise restart recovery hits the `no output file` branch and
/// never re-attaches a watcher to a live claude_tui pane. The claude_tui
/// inflight has `session_id = None` (#2843), so the rollout is resolved by
/// cwd + freshest-transcript, honoring #2843's anti-stealing constraints
/// (tmux launch-script mtime floor; exclude transcripts claimed by other live
/// Claude TUI sessions).
#[path = "lifecycle/claude_restore.rs"]
mod claude_restore;
pub(super) use self::claude_restore::*;

#[path = "lifecycle/liveness.rs"]
mod liveness;
pub(super) use self::liveness::*;

#[path = "lifecycle/watch_host.rs"]
mod watch_host;
pub(in crate::services::discord) use self::watch_host::{HostSnapshot, WatchHost, watch_host_of};

#[path = "lifecycle/restore_support.rs"]
mod restore_support;
pub(super) use self::restore_support::*;
#[path = "lifecycle/tests.rs"]
mod tests;

#[path = "lifecycle/ready_failure.rs"]
mod ready_failure;
pub(in crate::services::discord) use self::ready_failure::fail_dispatch_for_ready_for_input_stall;

#[path = "lifecycle/recovery_markers.rs"]
mod recovery_markers;
pub(super) use self::recovery_markers::*;
pub(in crate::services::discord) use self::recovery_markers::{
    clear_recovery_handled_channels, store_recovery_handled_channels,
};

#[path = "lifecycle/output_policy.rs"]
mod output_policy;
pub(super) use self::output_policy::*;

#[path = "lifecycle/claims.rs"]
mod claims;
#[rustfmt::skip]
#[cfg(test)]
pub(in crate::services::discord) use self::claims::{CLAIM_PAUSE, claim_cross_channel_tmux_watcher_for_test, claim_or_replace_watcher, claim_or_reuse_watcher, evict_claim_before_adoption_for_test};
pub(super) use self::claims::*;
pub(in crate::services::discord) use self::claims::{
    ThreadFollowUpParent, WatchWithheld, WatcherClaimIncarnation,
    claim_or_replace_watcher_for_host, claim_or_reuse_watcher_for_host,
    thread_follow_up_parent_channel_id, thread_follow_up_parent_from_live,
};

#[cfg(test)]
pub(in crate::services::discord) use n4d2_entry_support::InstallFixture;

#[cfg(all(test, unix))]
#[path = "lifecycle/restore_legacy_tests.rs"]
mod restore_legacy_tests;

#[cfg(test)]
#[path = "lifecycle/n4d2_boundary_tests.rs"]
mod n4d2_boundary_tests;
#[cfg(test)]
#[path = "lifecycle/n4d2_entry_test.rs"]
pub(in crate::services::discord) mod n4d2_entry_support;

#[path = "lifecycle/restore.rs"]
mod restore;
pub(in crate::services::discord) use self::restore::restore_tmux_watchers;
#[path = "lifecycle/restore_mapping.rs"]
mod restore_mapping;
use self::restore_mapping::add_configured_channel_bindings;
pub(in crate::services::discord) use self::restore_mapping::session_belongs_to_current_runtime;

#[path = "lifecycle/restore_tests.rs"]
mod restore_tests;
