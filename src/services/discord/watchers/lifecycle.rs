use super::*;

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
#[path = "dispatched_origin_ghost.rs"]
mod dispatched_origin_ghost;
use dispatched_origin_ghost::consume_dispatched_origin_ghost_if_current;

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

#[path = "lifecycle/restore_support.rs"]
mod restore_support;
pub(super) use self::restore_support::*;
#[path = "lifecycle/tests.rs"]
mod tests;
pub(super) use self::tests::*;

#[path = "lifecycle/ready_failure.rs"]
mod ready_failure;
pub(in crate::services::discord) use self::ready_failure::fail_dispatch_for_ready_for_input_stall;
pub(super) use self::ready_failure::*;

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
pub(super) use self::claims::*;
pub(in crate::services::discord) use self::claims::{
    claim_or_replace_watcher, claim_or_reuse_watcher,
};

#[path = "lifecycle/restore.rs"]
mod restore;
pub(super) use self::restore::*;
pub(in crate::services::discord) use self::restore::{
    restore_tmux_watchers, session_belongs_to_current_runtime,
};

#[path = "lifecycle/restore_tests.rs"]
mod restore_tests;
pub(super) use self::restore_tests::*;
