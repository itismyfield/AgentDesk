use super::footer_view_reconciler;
use super::gateway::DiscordGateway;
use super::inflight::optional_message_id;
use super::recovery_paths::restart::dispose_recovery_relay_outcome;
use super::recovery_paths::shared::RecoveryRelayOutcome;
use super::settings::{
    load_last_session_path, resolve_role_binding,
    validate_bot_channel_routing_with_provider_channel,
};
use super::turn_bridge::stale_inflight_message;
use super::turn_view_reconciler::note_intake_turn_completed as tv_done;
use super::*;
use crate::db::turns::TurnTokenUsage;
use crate::services::agent_protocol::{RuntimeHandoff, RuntimeHandoffKind, StreamMessage};
use crate::services::git::GitCommand;
#[cfg(unix)]
use crate::services::platform::binary_resolver;
#[cfg(unix)]
use crate::services::tmux_common::tmux_exact_target;
#[cfg(unix)]
use crate::services::tmux_diagnostics::{build_tmux_death_diagnostic, tmux_session_has_live_pane};
use crate::utils::format::tail_with_ellipsis;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Command;

#[path = "recovery_engine/status_panel.rs"]
mod recovery_status_panel;
#[path = "recovery_engine/status_panel_completion_producer.rs"]
mod status_panel_completion_producer;
use self::status_panel_completion_producer::*;

// #3479 r8: behavior-preserving extraction of pure clusters into leaf modules.
#[path = "recovery_engine/jsonl_extract.rs"]
mod jsonl_extract;
#[path = "recovery_engine/output_path_detect.rs"]
mod output_path_detect;
#[path = "recovery_engine/phase_policy.rs"]
mod phase_policy;
// #3479 item-2: behavior-preserving extraction of the terminal-success watcher /
// recovery start-offset helper cluster into a leaf module.
#[path = "recovery_engine/terminal_watcher.rs"]
mod terminal_watcher;
// #3479 item-2: behavior-preserving extraction of the inflight-state derivation
// helper cluster (handoff message, ready-for-input probes, worktree info /
// spawn-cwd derivation) into a leaf module.
#[path = "recovery_engine/analytics_transcript.rs"]
mod analytics_transcript;
#[path = "recovery_engine/rebind_runtime.rs"]
mod rebind_runtime;
#[path = "recovery_engine/state_extractors.rs"]
mod state_extractors;
// #3834: behavior-preserving extraction of the manual-rebind recovery path
// (`rebind_inflight_for_channel` + its private `codex_tui_*` / `Pending*` support
// cluster and unit tests) into a leaf module. `RebindOutcome` / `RebindError` stay
// in this root module (shared with `rebind_runtime` + external callers); the entry
// point is re-exported below so external call sites stay byte-identical.
#[path = "recovery_engine/manual_rebind/mod.rs"]
mod manual_rebind;
#[path = "recovery_engine/manual_rebind_output_path.rs"]
mod manual_rebind_output_path;
#[path = "recovery_engine/manual_rebind_override.rs"]
mod manual_rebind_override;
#[path = "recovery_engine/routing_orphan.rs"] // #3869 routing-orphan finalize
mod routing_orphan;
#[path = "recovery_engine/terminal_text_idempotency.rs"]
mod terminal_text_idempotency;
// #3834: behavior-preserving extraction of the runtime-rediscovery recovery path
// (`reregister_active_turn_from_inflight` + its private
// `reseed_watcher_owned_finalizer_ledger` helper and unit tests) into a leaf
// module. The entry point is re-exported below so external call sites
// (`watchers::lifecycle`, `manual_rebind`, the restart-path reattach calls) stay
// byte-identical.
#[path = "recovery_engine/runtime.rs"]
mod runtime;
// #3834 r2: behavior-preserving extraction of the terminal recovery delivery /
// visible completion helpers and their tests into a leaf module. Entry points
// are re-imported below so sibling modules and restart recovery call sites keep
// their existing names.
#[path = "recovery_engine/completion_delivery.rs"]
mod completion_delivery;
// #3834 r2: behavior-preserving extraction of the restart-path inflight recovery
// scan (`restore_inflight_turns`) plus its tmux retry/output-path helpers into a
// leaf module. Entry points are re-exported below so external paths stay stable.
#[path = "recovery_engine/restore_inflight.rs"]
mod restore_inflight;
// #4111: behavior-preserving extraction of guarded Codex rollout persist-outcome
// handling before restart-path watcher spawn into a leaf module.
#[path = "recovery_engine/restore_persist_outcome.rs"]
mod restore_persist_outcome;
// #4380: crash-restart relay-resume guard (yield-gate escape-hatch predicate +
// black-hole DLQ backstop). Lives here (a non-giant) so declaring it never
// re-inflates the discord/mod.rs giant; unix-gated to match its callers (the
// `tmux` watcher yield gate and the recovery watcher spawn path). The predicate +
// backstop are re-exported at `pub(in crate::services::discord)` so the yield gate
// in `tmux.rs` and the recovery re-adopt path can reach them.
#[cfg(unix)]
#[path = "recovery_engine/crash_resume_guard.rs"]
mod crash_resume_guard;
#[cfg(unix)]
pub(in crate::services::discord) use self::crash_resume_guard::{
    crash_readopt_live_relay_resume_required, guard_readopt_relay_resume_or_dead_letter,
};

// Re-import moved items so existing call sites stay byte-identical.
use self::jsonl_extract::extract_response_from_output;
#[cfg(unix)]
use self::output_path_detect::{
    DetectedRebindOutputPath, StaleOutputCandidate, detect_rebind_output_path_from_candidates,
    parse_lsof_output_candidates,
};
use self::phase_policy::{
    can_fast_path_captured_full_response, recovery_has_post_work_ready_evidence,
    recovery_phase_after_output_scan, recovery_phase_after_tmux_probe,
    recovery_phase_for_existing_inflight_rebind, recovery_ready_without_output_already_delivered,
    recovery_ready_without_output_has_captured_response,
    recovery_terminal_delivery_already_committed,
};
// `extract_response_from_output_pub` is re-exported (not just re-imported) so the
// `recovery_engine::extract_response_from_output_pub` path stays valid for the
// turn_bridge / tmux_restart_handoff external callers.
pub(super) use self::jsonl_extract::{
    extract_response_from_output_pub, success_result_end_offset_after_offset,
};
// #3479 item-2: re-import the externally-called terminal-watcher helpers so the
// existing call sites stay byte-identical. The remaining cluster members
// (`terminal_success_watcher_stop_allowed`, `jsonl_tail_contains_terminal_end_sentinel`,
// `TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD`) are only used inside the submodule and
// stay private to it.
use self::terminal_watcher::{
    output_has_bytes_after_offset, recovery_watcher_start_offset_for_state,
    terminal_success_output_drained_for_recovery,
};
// #3479 item-2: re-import the inflight-state derivation helpers used by the root
// module so the existing call sites stay byte-identical. The cluster-internal
// members (the worktree path/branch/info/git helpers, `recovery_dispatch_id`,
// `recovery_requires_worktree_context` and `inflight_ready_for_input_without_tui_pane`)
// stay private to the submodule.
use self::rebind_runtime::{
    claude_rebind_transcript_path, resolve_rebind_runtime_state,
    spawn_codex_tui_rebind_relay_output,
};
use self::state_extractors::{
    inflight_or_legacy_tmux_ready_for_input, interrupted_recovery_message, recovery_spawn_adk_cwd,
    recovery_tmux_session_name, restore_recovered_session_worktree,
};
// `save_missing_session_handoff` is re-exported (not just re-imported) so the
// `recovery_engine::save_missing_session_handoff` path stays valid for the
// `recovery_paths::restart` external caller.
pub(super) use self::state_extractors::save_missing_session_handoff;
// #3834: `rebind_inflight_for_channel` is re-exported (not just re-imported) so the
// `recovery_engine::rebind_inflight_for_channel` path stays valid for its `health`
// caller. Its private cluster (`codex_tui_*`, `Pending*`) is not re-exported.
#[cfg(test)]
pub(crate) use self::manual_rebind::{
    EpisodeAuthorityHeldBarrier, PostAdoptionClaimBarrier, install_episode_authority_held_barrier,
    install_post_adoption_claim_barrier,
};
pub(crate) use self::manual_rebind::{
    rebind_inflight_for_channel, rebind_inflight_for_channel_with_minimum_start_offset,
};
pub(crate) use self::manual_rebind_override::ManualRebindOverrides;
pub(in crate::services::discord) use self::runtime::reregister_active_turn_from_inflight_under_episode_guard;
// #3834: `reregister_active_turn_from_inflight` is re-exported (not just
// re-imported) so the `recovery_engine::reregister_active_turn_from_inflight`
// path stays valid for its `watchers::lifecycle` caller (via the `recovery`
// alias) and so the root's `restore_inflight_turns` reattach call sites stay
// byte-identical. Its private `reseed_watcher_owned_finalizer_ledger` helper is
// not re-exported.
pub(in crate::services::discord) use self::completion_delivery::relay_recovered_terminal_text_to_placeholder;
use self::completion_delivery::{
    RecoveryCompletionOutcome, complete_recovery_visible_turn, relay_recovery_terminal_notice,
    should_advance_recovery_dispatch_after_relay,
};
// `detect_live_tmux_output_path` exists only under `#[cfg(unix)]` in the child;
// a by-name import of a cfg'd-out item is a hard E0432 on non-unix targets.
#[cfg(unix)]
use self::restore_inflight::detect_live_tmux_output_path;
use self::restore_inflight::tmux_session_alive_with_retry;
pub(in crate::services::discord) use self::restore_inflight::{
    finish_recovered_turn_mailbox, restore_inflight_turns,
};
use self::restore_persist_outcome::{RestorePersistOutcome, restore_codex_rollout_output_path};
pub(super) use self::runtime::reregister_active_turn_from_inflight;
pub(in crate::services::discord) use self::terminal_text_idempotency::RecoveryDeliveryContext;
// #3479: re-import the analytics + transcript helpers so root call sites stay
// byte-identical. `recovered_transcript_turn_id` is gated on cfg(test) — the root
// reaches it only from its unit test (prod calls it inside analytics_transcript).
#[cfg(test)]
use self::analytics_transcript::recovered_transcript_turn_id;
use self::analytics_transcript::{
    extract_turn_analytics_from_output, lookup_turn_finished_dispatch_kind,
    persist_recovered_transcript, recovered_turn_duration_ms,
};

#[cfg(not(unix))]
fn tmux_session_has_live_pane(_name: &str) -> bool {
    false
}

/// #2428 H5: exponential backoff (+ jitter) for the 3-attempt recovery retry
/// loops in this module. Budget contract (Codex pass-1 review): the old fixed
/// schedule waited 1000+1000 = 2000ms total; the new gap schedule
/// `[700, 1300, 2000]`ms + 0..=100ms jitter preserves that wall-clock budget
/// (callers sleep on attempts 1 and 2: 700+1300 = 2000ms, jitter only adds)
/// while waking ~300ms earlier on average for sub-second transients. The
/// third slot is reachable only if a future change adds attempts; it caps the
/// per-gap wait. `attempt` is 1-indexed = the attempt that *just failed*;
/// call only when another attempt will actually run (`if attempt < 3`).
pub(super) fn recovery_retry_backoff(attempt: u32) -> std::time::Duration {
    // Gap schedule between attempts 1→2, 2→3, 3→4, …: 700ms, 1300ms, 2000ms.
    // The 700 + 1300 = 2000ms sum is what makes the 3-attempt total grace
    // window equal to the old fixed-1s × 3 budget. Do not adjust either of
    // the first two values without also reviewing every caller and updating
    // the budget contract above.
    const SCHEDULE_MS: [u64; 3] = [700, 1300, 2000];
    const MAX_BASE_MS: u64 = 2000;
    let idx = attempt.saturating_sub(1) as usize;
    let base_ms = SCHEDULE_MS
        .get(idx)
        .copied()
        .unwrap_or(MAX_BASE_MS)
        .min(MAX_BASE_MS);
    // Add 0..=100ms uniform jitter so simultaneous retries (e.g. two
    // channels recovering at once) do not lock-step into the same wakeup.
    use rand::Rng;
    let jitter_ms = rand::thread_rng().gen_range(0..=100);
    std::time::Duration::from_millis(base_ms + jitter_ms)
}

#[cfg(test)]
mod recovery_retry_backoff_tests {
    use super::recovery_retry_backoff;
    use std::time::Duration;

    #[test]
    fn backoff_attempt_1_is_in_700_to_800_ms() {
        let d = recovery_retry_backoff(1);
        assert!(d >= Duration::from_millis(700), "got {d:?}");
        assert!(d <= Duration::from_millis(800), "got {d:?}");
    }

    #[test]
    fn backoff_attempt_2_is_in_1300_to_1400_ms() {
        let d = recovery_retry_backoff(2);
        assert!(d >= Duration::from_millis(1300), "got {d:?}");
        assert!(d <= Duration::from_millis(1400), "got {d:?}");
    }

    #[test]
    fn backoff_attempt_3_is_in_2000_to_2100_ms() {
        let d = recovery_retry_backoff(3);
        assert!(d >= Duration::from_millis(2000), "got {d:?}");
        assert!(d <= Duration::from_millis(2100), "got {d:?}");
    }

    #[test]
    fn backoff_clamps_attempts_beyond_schedule() {
        // Even if we ever extend the loop past 3, the wait must not exceed
        // the documented cap.
        let d = recovery_retry_backoff(7);
        assert!(d <= Duration::from_millis(2100), "got {d:?}");
    }

    #[test]
    fn backoff_attempt_zero_is_treated_as_first() {
        // Defensive: a caller passing 0 should not get a divide-by-zero or
        // a tiny instant-retry; behave like attempt 1.
        let d = recovery_retry_backoff(0);
        assert!(d >= Duration::from_millis(700), "got {d:?}");
        assert!(d <= Duration::from_millis(800), "got {d:?}");
    }

    #[test]
    fn backoff_preserves_3_attempt_total_budget() {
        // Budget contract: 3-attempt loop with sleeps on attempts 1 and 2
        // must equal the old fixed-1s × 3 budget (= 2000ms wait time)
        // within the jitter envelope. This is the regression the Codex
        // pass-1 review flagged.
        let total = recovery_retry_backoff(1) + recovery_retry_backoff(2);
        // Lower bound: 700 + 1300 = 2000ms with zero jitter on both calls.
        assert!(total >= Duration::from_millis(2000), "got {total:?}");
        // Upper bound: 800 + 1400 = 2200ms with max jitter on both calls.
        assert!(total <= Duration::from_millis(2200), "got {total:?}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryPhase {
    Pending,
    WatcherReattach,
    InflightRestore,
    Done,
}

impl RecoveryPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::WatcherReattach => "watcher_reattach",
            Self::InflightRestore => "inflight_restore",
            Self::Done => "done",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "watcher_reattach" => Some(Self::WatcherReattach),
            "inflight_restore" => Some(Self::InflightRestore),
            "done" => Some(Self::Done),
            _ => None,
        }
    }

    pub fn from_optional_str(value: Option<&str>) -> Option<Self> {
        value.and_then(Self::from_str)
    }
}

fn recovery_input_fifo_for_runtime(
    runtime_kind: RuntimeHandoffKind,
    input_fifo_path: Option<String>,
) -> Result<Option<String>, &'static str> {
    if runtime_kind.requires_input_fifo() {
        input_fifo_path
            .filter(|path| !path.is_empty())
            .map(Some)
            .ok_or("input fifo path missing during recovery")
    } else {
        Ok(input_fifo_path.filter(|path| !path.is_empty()))
    }
}

fn runtime_handoff_for_recovery(
    runtime_kind: RuntimeHandoffKind,
    output_path: String,
    input_fifo_path: Option<String>,
    tmux_session_name: String,
    session_id: Option<String>,
    last_offset: u64,
) -> RuntimeHandoff {
    match runtime_kind {
        RuntimeHandoffKind::LegacyTmuxWrapper => RuntimeHandoff::LegacyTmuxWrapper {
            output_path,
            input_fifo_path: input_fifo_path.unwrap_or_default(),
            tmux_session_name,
            last_offset,
        },
        RuntimeHandoffKind::ClaudeTui => RuntimeHandoff::ClaudeTui {
            transcript_path: output_path,
            tmux_session_name,
            last_offset,
        },
        RuntimeHandoffKind::CodexTui => RuntimeHandoff::CodexTui {
            rollout_path: output_path,
            thread_id: session_id,
            tmux_session_name,
            last_offset,
        },
        RuntimeHandoffKind::ProcessBackend => RuntimeHandoff::ProcessBackend {
            output_path,
            session_name: tmux_session_name,
            last_offset,
        },
        RuntimeHandoffKind::ClaudeEAdapter => RuntimeHandoff::ClaudeEAdapter {
            output_path,
            session_name: tmux_session_name,
            last_offset,
        },
    }
}

fn emit_recovery_quality_event(
    provider: &ProviderKind,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    agent_id: Option<&str>,
    reason: &str,
) {
    crate::services::observability::emit_agent_quality_event(
        crate::services::observability::AgentQualityEvent {
            // #3562: prefer the turn_id as the source_event_id so this quality
            // row joins back to turn_started/turn_finished on the same standard
            // key (turn_analytics keys source_event_id = turn_id). Fall back to
            // the legacy session_key → dispatch_id chain when no turn_id exists.
            source_event_id: turn_id
                .map(str::to_string)
                .or_else(|| session_key.map(str::to_string))
                .or_else(|| dispatch_id.map(str::to_string)),
            correlation_id: dispatch_id
                .map(str::to_string)
                .or_else(|| session_key.map(str::to_string)),
            agent_id: agent_id.map(str::to_string),
            provider: Some(provider.as_str().to_string()),
            channel_id: Some(channel_id.to_string()),
            card_id: None,
            dispatch_id: dispatch_id.map(str::to_string),
            event_type: "recovery_fired".to_string(),
            payload: serde_json::json!({
                "reason": reason,
                "session_key": session_key,
                "turn_id": turn_id,
            }),
        },
    );
}

/// #896: Outcome of a successful [`rebind_inflight_for_channel`] call.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RebindOutcome {
    pub tmux_session: String,
    pub channel_id: u64,
    pub initial_offset: u64,
    /// `true` when a tmux watcher was spawned by this call. On unix this is
    /// always true on success. On non-unix builds watcher spawning is a
    /// no-op, so this reads `false` even though the inflight file was
    /// written.
    pub watcher_spawned: bool,
    /// #897 P2 #2 — `true` when a pre-existing watcher handle was present
    /// for this channel and has been cancelled + replaced by the freshly
    /// spawned one. Operators use this to distinguish a clean vacant claim
    /// from a zombie-slot recovery, which is the common case where an old
    /// watcher kept its DashMap entry after its tmux exited.
    pub watcher_replaced: bool,
}

/// #896: Errors from [`rebind_inflight_for_channel`]. Map 1:1 to HTTP status
/// codes in the `/api/inflight/rebind` handler.
#[derive(Debug)]
pub enum RebindError {
    /// Target tmux session is not alive — nothing to rebind to. 404.
    TmuxNotAlive { tmux_session: String },
    /// A persisted or supplied Discord channel id was zero. 400.
    ChannelIdZero,
    /// An inflight state already exists for this channel. Caller must clear
    /// it (force-kill or natural completion) before rebinding. 409.
    InflightAlreadyExists,
    /// The exact inflight episode reserved by automatic recovery was replaced
    /// before the lock-held rebind mutation. The replacement is untouched.
    InflightEpisodeChanged,
    /// The tmux pane is still writing to a deleted or replaced output fd, so
    /// rebinding the pathname would silently follow the wrong file. 409.
    StaleOutputPath {
        tmux_session: String,
        output_path: String,
        live_fd: String,
        live_inode: Option<u64>,
        live_path: String,
    },
    /// Channel is not bound to the requested provider in the role-map. 400.
    ChannelNotBound,
    /// A direct TUI tmux session was detected, but `/api/inflight/rebind` can
    /// only respawn wrapper-output watchers. Direct TUI recovery must go
    /// through the runtime-specific rehydrate/idle relay path instead.
    RuntimeBindingUnavailable {
        tmux_session: String,
        runtime_kind: RuntimeHandoffKind,
    },
    /// `tmux_session` not provided and no in-memory session supplies a
    /// channel_name — cannot derive the canonical tmux session name. 400.
    ChannelNameMissing,
    /// Unrecoverable internal error (inflight write, lock poisoning, etc.). 500.
    Internal(String),
}

impl std::fmt::Display for RebindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TmuxNotAlive { tmux_session } => {
                write!(f, "tmux session not alive: {tmux_session}")
            }
            Self::ChannelIdZero => write!(f, "channel id must be non-zero"),
            Self::InflightAlreadyExists => {
                write!(f, "inflight state already exists for this channel")
            }
            Self::InflightEpisodeChanged => {
                write!(f, "reserved inflight episode changed before rebind")
            }
            Self::StaleOutputPath {
                tmux_session,
                output_path,
                live_fd,
                live_inode,
                live_path,
            } => {
                write!(
                    f,
                    "StaleOutputPath: tmux session {tmux_session} still writes to fd {live_fd}"
                )?;
                if let Some(inode) = live_inode {
                    write!(f, " (inode {inode})")?;
                }
                write!(
                    f,
                    " via {live_path}; refusing to rebind pathname {output_path}"
                )
            }
            Self::ChannelNotBound => write!(f, "channel is not bound for this provider"),
            Self::RuntimeBindingUnavailable {
                tmux_session,
                runtime_kind,
            } => write!(
                f,
                "watcher rebind unavailable for {} tmux session {tmux_session}",
                runtime_kind.as_str()
            ),
            Self::ChannelNameMissing => write!(
                f,
                "channel name missing — pass tmux_session or pre-register the channel"
            ),
            Self::Internal(msg) => write!(f, "internal: {msg}"),
        }
    }
}
