mod abandon_request_store;
mod adk_session;
pub(crate) mod agent_handoff;
pub(crate) mod agentdesk_config;
mod answer_flush_barrier;
pub(crate) mod bot_role;
// #3479 item-2: restart-gap message recovery extracted to its catch-up sibling.
mod catch_up;
mod commands;
mod compact_turn_authority;
mod delivery_lease_key;
mod destructive_cancel_gate;
mod discord_io;
mod dispatch_policy;
mod footer_view_reconciler;
pub(crate) mod formatting;
mod gateway;
mod gateway_voice_queue;
pub(crate) mod health;
pub(crate) mod http;
mod idle_detector;
pub(crate) mod idle_recap;
mod idle_recap_interaction;
mod idle_relay_drift;
mod inflight;
mod inflight_heartbeat_sweeper;
pub(crate) mod internal_api;
mod jsonl_watcher;
mod mailbox_finish;
mod mcp_credential_watcher;
pub(crate) mod meeting_artifact_store;
pub(crate) mod meeting_orchestrator;
pub(crate) mod meeting_state_machine;
mod metrics;
mod model_catalog;
mod model_picker_interaction;
pub(crate) mod monitoring_status;
mod org_schema;
pub(crate) mod org_writer;
pub(crate) mod outbound;
mod placeholder_cleanup;
mod placeholder_controller;
mod placeholder_live_events;
mod placeholder_sweeper;
mod prompt_builder;
mod queue_dispatch;
mod queue_io;
mod queue_marker;
mod queue_overflow_dlq;
mod queue_reactions;
mod queued_placeholders_store;
mod reaction_cleanup;
mod reaction_lifecycle;
mod readopted_mailbox_ledger;
mod relay_health;
pub(crate) mod relay_recovery;
mod replace_outcome_policy;
pub(crate) mod response_sanitizer;
// #3983 item4: one-shot top session banner emit + dual-path (sink/watcher) de-dup.
mod session_banner;
#[cfg(unix)]
mod session_relay_sink;
mod sidecar_interaction;
// #2011 Phase 5.3: standalone JSONL → Discord relay loop on cluster-standby nodes (leader uses tmux_watcher's relay path).
#[cfg(unix)]
mod standby_relay;
// #1074: landing zone for the future recovery-engine module split (restart / runtime / manual_rebind; see `docs/recovery-paths.md`). Named `recovery_paths` to avoid shadowing the `recovery_engine as recovery` alias until the split lands.
mod recovery_engine;
mod recovery_paths;
mod restart_mode;
// #1074: session identity parsing SSoT (legacy + namespaced session_key forms).
pub(crate) mod restart_report;
mod role_map;
mod router;
mod runtime_bootstrap;
pub(in crate::services::discord) mod semantic_boundaries;
// #1446 stall-deadlock recovery: shared post-clear bookkeeping for the THREAD-GUARD
// + stall-watchdog cleanup paths so neither leaks `global_active` / cancel tokens.
pub mod runtime_store;
// #3646 OBSERVATION-ONLY: pure payload builders + owner-split derivation for the
// relay flight recorder's two-signal owner separation and the three terminal
// lifecycle events. No relay/cleanup behaviour lives here.
mod relay_owner_observability;
pub(crate) mod session_identity;
mod session_runtime;
pub(crate) mod settings;
pub(crate) mod shared_memory;
// #3038 S1/S2: extracted SharedData field clusters (named sub-structs + their
// dedicated inherent impls). See `shared_state::QueuedPlaceholderState` and
// `shared_state::SessionOverrideState`.
mod shared_state;
mod single_message_panel;
mod stall_recovery;
mod startup_reclaim;
mod status_panel_orphan_store;
mod steering;
pub(in crate::services::discord) mod streaming_finalizer;
mod task_notification_delivery;
pub(in crate::services::discord) mod task_supervisor;
mod terminal_ui_obligation;
#[cfg(unix)]
mod tmux;
mod turn_completion_events;
pub(in crate::services::discord) mod turn_end_wip_warning;
#[cfg(unix)]
pub(crate) use tmux::write_spawn_nonce;
#[cfg(unix)]
mod tmux_error_detect;
#[cfg(unix)]
mod tmux_lifecycle;
#[cfg(unix)]
mod tmux_overload_retry;
#[cfg(unix)]
mod tmux_reaper;
#[cfg(unix)]
mod tmux_restart_handoff;
mod tui_direct_abort_marker;
mod tui_direct_pending_start;
mod tui_prompt_relay;
mod tui_task_card;
mod turn_bridge;
mod turn_finalizer;
mod turn_view_reconciler;
mod voice_acknowledgement;
mod voice_background_driver;
mod voice_barge_in;
mod voice_config_cache;
mod voice_id_sequences;
mod voice_lifecycle;
mod voice_routing;
mod voice_sensitivity;
#[path = "watchers/lifecycle_decision.rs"]
mod watcher_lifecycle_decision;

pub(crate) use meeting_orchestrator as meeting;
pub(in crate::services::discord) use {
    delivery_lease_key::DeliveryLeaseKey, relay_health::RelayFrontierToken,
};
// #3479 item-2: re-export the catch-up subsystem entry points referenced
// outside the extracted cluster (`maybe_schedule_catch_up_retry_after_queue_drain`
// here in mod.rs and `catch_up_missed_messages` in runtime_bootstrap recovery).
pub(in crate::services::discord) use catch_up::{
    CatchUpRetryState, catch_up_missed_messages, catch_up_missed_messages_for_retry,
    should_trigger_catch_up_retry, take_catch_up_retry_checkpoint_after_queue_drain,
};
pub(in crate::services::discord) use mailbox_finish::{
    mailbox_finish_cancelled_turn, mailbox_finish_owned_turn, mailbox_finish_turn,
    mailbox_finish_turn_if_matches, mailbox_finish_turn_if_matches_started_before,
};
pub(in crate::services::discord) use recovery_engine as recovery;
// #3038 S1: re-export the extracted cluster type so the `SharedData` field
// declaration and constructor literals reference it without a module-qualified
// path (surface freeze, #3294/#3295 pattern).
pub(crate) use restart_mode::InflightRestartMode;
pub(crate) use router::{
    HeadlessTurnStartError, IntakeRequest, TurnKind, execute_intake_turn_core,
};
#[cfg(unix)]
pub(crate) use session_relay_sink::run_session_bound_discord_relay_supervisor;
// #3038 S4: re-export the live-placeholder cluster type so `SharedData`
// declarations/constructors keep the S1/S2/S3 unqualified surface.
pub(in crate::services::discord) use shared_state::{PlaceholderState, PolicyRuntime};
pub(in crate::services::discord) use shared_state::{QueuedPlaceholderState, RuntimeHttpCache};
// #3038 S2: the cluster-D members were `pub(super)` on `SharedData` (visible up
// to `crate::services`), so the group type is re-exported with that same scope.
pub(in crate::services) use shared_state::SessionOverrideState;
// #3479 Item 3: the cluster members were `pub(super)` on `SharedData` (visible
// up to `crate::services`), so the group type is re-exported with that scope.
pub(in crate::services) use shared_state::DispatchRoutingState;
// #3038 S3: same scope rationale as S2 — the cluster-E members were
// `pub(super)` on `SharedData` (visible up to `crate::services`).
pub(in crate::services) use shared_state::RestartLifecycle;
pub(crate) use turn_bridge::TmuxCleanupPolicy;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use crate::services::agent_protocol::{DEFAULT_ALLOWED_TOOLS, StreamMessage};
use crate::services::claude;
use crate::services::codex;
use crate::services::gemini;
use crate::services::opencode;
use crate::services::provider::{CancelToken, ProviderKind, ReadOutputResult};
use crate::services::qwen;
use crate::ui::ai_screen::{self, HistoryItem, HistoryType};

use crate::services::turn_orchestrator::ChannelMailboxHandle;
use crate::services::turn_orchestrator::HasPendingSoftQueueResult;
use adk_session::{
    build_adk_session_key, build_session_key_candidates, derive_adk_session_info,
    lookup_pending_dispatch_for_thread, parse_dispatch_id, post_adk_session_status,
};
pub(in crate::services) use compact_turn_authority::{
    ManagedCompactTurnIdentity, compact_eligible_turn_source, live_managed_turn_matches,
};
use formatting::{
    BUILTIN_SKILLS, extract_skill_description, format_for_discord, format_tool_input,
    send_long_message_raw, truncate_str,
};
use inflight::{InflightTurnState, load_inflight_states, save_inflight_state};
pub(crate) use inflight::{clear_inflight_state, lock_inflight_state_path};
pub(in crate::services::discord) use prompt_builder::load_channel_recent_context;
use prompt_builder::{RecoveryContextManifestInput, build_system_prompt_with_manifest};
pub(in crate::services::discord) use queue_dispatch::MailboxEnqueueOutcome;
use queue_dispatch::{
    MailboxTakeNextSoftOutcome, mailbox_abandon_unclaimed_dispatch_after_success,
};
use recovery_engine::restore_inflight_turns;
use restart_report::flush_restart_reports;
use router::handle_event;
use settings::{
    RoleBinding, channel_upload_dir, cleanup_old_uploads, load_bot_settings,
    load_last_session_path, resolve_role_binding, save_bot_settings,
    validate_bot_channel_routing_with_provider_channel,
};
#[cfg(unix)]
use tmux::restore_tmux_watchers;
#[cfg(unix)]
use tmux_reaper::{cleanup_orphan_tmux_sessions, reap_dead_tmux_sessions};
use turn_bridge::{TurnBridgeContext, spawn_turn_bridge, tmux_runtime_paths};

pub(crate) use crate::services::turn_orchestrator::has_soft_intervention_at;
pub(crate) use prompt_builder::{DispatchProfile, PromptProfiles};
pub(crate) use runtime_bootstrap::RunBotContext;
pub(crate) use runtime_bootstrap::run_bot;

use crate::services::turn_orchestrator::{
    ActiveTurnKind, CancelActiveTurnResult, CancelQueuedMessageResult, ChannelMailboxSnapshot,
    ClearChannelResult, FinishTurnResult, HydratePendingQueueResult,
    PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER, QueueExitEvent, QueueExitKind,
    QueuePersistenceContext, RecoveryKickoffResult, RequeueInterventionResult, TakeNextSoftResult,
    VALVE_CLEARED_DISPATCH_MARKER_GRACE, load_channel_pending_dispatch_marker,
    load_pending_dispatch_markers, load_pending_queues, warn_legacy_pending_queue_files,
};
pub(super) use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, Intervention, InterventionMode, MAX_INTERVENTIONS_PER_CHANNEL,
    PendingQueueItem,
};
pub use discord_io::{
    retry_failed_dm_notifications, send_file_to_channel, send_message_to_channel,
    send_message_to_user,
};
pub(in crate::services::discord) use dispatch_policy::{
    is_allowed_turn_sender, prepend_monitor_auto_turn_origin, resolve_announce_bot_user_id,
    resolve_notify_bot_user_id, should_phase2_recover_message,
    stale_dispatch_turn_for_queued_intervention, stale_dispatch_turn_for_text,
    strip_monitor_auto_turn_origin,
};
pub(crate) use inflight::latest_request_owner_user_id_for_channel;
pub use settings::{
    load_discord_bot_launch_configs, resolve_discord_bot_provider, resolve_discord_token_by_hash,
};
// #2047 Finding 5 — expose the role-map resolver so HTTP channel lookups can deny channels that are
// not registered with this AgentDesk instance.
pub(crate) use settings::resolve_role_binding as resolve_channel_role_binding;

/// Discord message length limit
pub(super) const DISCORD_MSG_LIMIT: usize = 2000;

/// Lower bound of the synthetic-headless message-id range. Real Discord
/// snowflake ids never reach this value, so any id at or above it is a
/// synthetic placeholder (headless recovery / creation-failed fallback).
/// Centralized here so both `turn_bridge::is_synthetic_headless_message_id`
/// and the typed `inflight` status-panel ownership ops (#3077) agree on the
/// boundary without coupling `inflight` to the serenity `MessageId` newtype.
pub(in crate::services::discord) const SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR: u64 =
    8_000_000_000_000_000_000;

/// Raw `u64` form of `turn_bridge::is_synthetic_headless_message_id`.
pub(in crate::services::discord) fn is_synthetic_headless_message_id_raw(value: u64) -> bool {
    value >= SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR
}
const UPLOAD_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const UPLOAD_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const SESSION_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60); // 1 hour
// #1085 (908-3): extended from 1h → 4h. Working agents idle between dispatch
// turns and the prior 60-min cap forced the next user/dispatch turn to start a
// fresh provider session, defeating cache reuse. 4h covers typical "go for
// lunch / sync meeting" gaps while still bounding zombie growth via the
// cleanup interval reaper at `mod.rs:2093`.
const SESSION_MAX_IDLE: Duration = Duration::from_secs(4 * 60 * 60); // 4 hours
const DEAD_SESSION_REAP_INTERVAL: Duration = Duration::from_secs(60); // 1 minute
const RESTART_REPORT_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const DEFERRED_RESTART_POLL_INTERVAL: Duration = Duration::from_secs(10);

pub(in crate::services::discord) fn queued_message_ids(
    snapshot: &ChannelMailboxSnapshot,
) -> std::collections::HashSet<u64> {
    let mut ids = std::collections::HashSet::new();
    for item in &snapshot.intervention_queue {
        ids.insert(item.message_id.get());
        ids.extend(
            item.source_message_ids
                .iter()
                .map(|message_id| message_id.get()),
        );
    }
    ids
}

pub(in crate::services::discord) fn recovery_known_message_ids(
    snapshot: &ChannelMailboxSnapshot,
) -> std::collections::HashSet<u64> {
    let mut ids = queued_message_ids(snapshot);
    if let Some(active_id) = snapshot.active_user_message_id {
        ids.insert(active_id.get());
    }
    ids
}

pub(in crate::services::discord) fn advance_last_message_checkpoint(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
) -> u64 {
    let message_id = message_id.get();
    let checkpoint = *shared
        .last_message_ids
        .entry(channel_id)
        .and_modify(|current| *current = (*current).max(message_id))
        .or_insert(message_id);
    runtime_store::save_last_message_id(provider.as_str(), channel_id.get(), checkpoint);
    checkpoint
}

#[cfg(test)]
mod last_message_checkpoint_tests {
    use super::*;

    struct ScopedRuntimeRoot {
        _lock: std::sync::MutexGuard<'static, ()>,
        temp: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl ScopedRuntimeRoot {
        fn path(&self) -> &std::path::Path {
            self.temp.path()
        }
    }

    impl Drop for ScopedRuntimeRoot {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                    None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
                }
            }
        }
    }

    fn scoped_runtime_root() -> ScopedRuntimeRoot {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        let temp = tempfile::tempdir().expect("last-message runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        ScopedRuntimeRoot {
            _lock: lock,
            temp,
            previous,
        }
    }

    fn last_message_path(
        root: &std::path::Path,
        provider: &ProviderKind,
        channel_id: ChannelId,
    ) -> std::path::PathBuf {
        root.join("runtime")
            .join("last_message")
            .join(provider.as_str())
            .join(format!("{}.txt", channel_id.get()))
    }

    #[test]
    fn advance_last_message_checkpoint_interleaved_advances_keep_max() {
        let root = scoped_runtime_root();
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_162_000);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

        let low_shared = std::sync::Arc::clone(&shared);
        let low_barrier = std::sync::Arc::clone(&barrier);
        let low = std::thread::spawn(move || {
            low_barrier.wait();
            advance_last_message_checkpoint(
                &low_shared,
                &ProviderKind::Claude,
                channel_id,
                MessageId::new(90_001),
            )
        });

        let high_shared = std::sync::Arc::clone(&shared);
        let high_barrier = std::sync::Arc::clone(&barrier);
        let high = std::thread::spawn(move || {
            high_barrier.wait();
            advance_last_message_checkpoint(
                &high_shared,
                &ProviderKind::Claude,
                channel_id,
                MessageId::new(90_002),
            )
        });

        barrier.wait();
        let _ = low.join().expect("low checkpoint thread");
        let _ = high.join().expect("high checkpoint thread");

        assert_eq!(
            shared.last_message_ids.get(&channel_id).map(|entry| *entry),
            Some(90_002)
        );
        let path = last_message_path(root.path(), &provider, channel_id);
        assert_eq!(
            std::fs::read_to_string(&path)
                .expect("checkpoint file")
                .trim(),
            "90002"
        );

        let mut stale_snapshot = std::collections::HashMap::new();
        stale_snapshot.insert(channel_id.get(), 90_001);
        runtime_store::save_all_last_message_ids(provider.as_str(), &stale_snapshot);
        assert_eq!(
            std::fs::read_to_string(path)
                .expect("checkpoint file after stale full-map save")
                .trim(),
            "90002"
        );
    }
}

pub(in crate::services::discord) use queue_io::{
    arm_slow_idle_queue_backstop_if_queue_nonempty, schedule_deferred_idle_queue_kickoff,
    schedule_deferred_idle_queue_kickoff_immediate, spawn_turn_completion_idle_queue_listener,
};
pub(super) fn single_message_panel_enabled() -> bool {
    single_message_panel::enabled()
}
/// Parse `var` as a `u64` seconds `Duration`, falling back to `default_secs`.
fn env_duration_secs(var: &str, default_secs: u64) -> Duration {
    let secs = (std::env::var(var).ok()).and_then(|s| s.parse::<u64>().ok());
    Duration::from_secs(secs.unwrap_or(default_secs))
}

/// Minimum interval between Discord placeholder progress edits (AGENTDESK_STATUS_INTERVAL_SECS, default 5s).
pub(super) fn status_update_interval() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| env_duration_secs("AGENTDESK_STATUS_INTERVAL_SECS", 5))
}

/// #3419 B: turn watchdog ABSOLUTE cap, a generous supplementary upper bound —
/// the primary firing measure is IDLE (`turn_idle_timeout`), so a turn emitting
/// output stays alive until it idles. Default 6h only guards an output that
/// never stops yet never finishes. AGENTDESK_TURN_TIMEOUT_SECS.
pub(super) fn turn_watchdog_timeout() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| env_duration_secs("AGENTDESK_TURN_TIMEOUT_SECS", 6 * 3600))
}

/// #3419 B: watcher turn IDLE window — fire only after this much silence since
/// the last real byte (`last_output_at`, NOT empty polls). Default 3600s == the
/// old absolute cap, so a turn must be FULLY idle for an hour (codex
/// interactive/subagent turns emit far sooner). AGENTDESK_TURN_IDLE_TIMEOUT_SECS.
pub(super) fn turn_idle_timeout() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| env_duration_secs("AGENTDESK_TURN_IDLE_TIMEOUT_SECS", 3600))
}

/// #3557 (A): per-turn HARD ceiling measured from turn start. Unlike
/// [`turn_watchdog_timeout`] (which the auto-extend loop pushes forward
/// indefinitely while inflight stays warm — the root of the unbounded turn
/// length), this is an absolute wall-clock cap on a single turn that the
/// auto-extend loop clamps to. Default 6h matches the current effective cap so
/// this is non-destructive by default; lower it via
/// `AGENTDESK_TURN_HARD_CEILING_SECS` to enforce a real backstop. When the
/// ceiling is hit, no further extension is granted and the next watchdog tick
/// drives the turn through the existing reconcile/cancel path.
pub(super) fn turn_hard_ceiling_timeout() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| env_duration_secs("AGENTDESK_TURN_HARD_CEILING_SECS", 6 * 3600))
}

/// #3557 (A): Codex-specific per-turn HARD ceiling. Codex `exec` turns are the
/// source of the worst outliers (a 13125s≈3.6h turn from a hung Codex process
/// that emitted no terminal event), so they get a tighter default ceiling (4h)
/// than the generic [`turn_hard_ceiling_timeout`]. Override via
/// `AGENTDESK_CODEX_TURN_HARD_CEILING_SECS`.
pub(super) fn codex_turn_hard_ceiling_timeout() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| env_duration_secs("AGENTDESK_CODEX_TURN_HARD_CEILING_SECS", 4 * 3600))
}

/// #3557 (A): the absolute hard-ceiling deadline (ms) for a turn given when it
/// started and which provider runs it. Codex uses the tighter
/// [`codex_turn_hard_ceiling_timeout`]; every other provider uses the generic
/// [`turn_hard_ceiling_timeout`]. The auto-extend loop never pushes the
/// watchdog deadline past this value.
pub(super) fn turn_hard_ceiling_deadline_ms(turn_started_ms: i64, provider: &ProviderKind) -> i64 {
    let ceiling = if matches!(provider, ProviderKind::Codex) {
        codex_turn_hard_ceiling_timeout()
    } else {
        turn_hard_ceiling_timeout()
    };
    turn_started_ms.saturating_add(ceiling.as_millis() as i64)
}

/// #3557 (A): clamp a proposed auto-extend deadline so it never exceeds the
/// per-turn hard ceiling. Returns the clamped deadline and whether clamping
/// actually capped the proposal (so the caller can warn exactly once). The
/// proposal is only ever lowered, never raised — a ceiling already in the past
/// hard-stops further extension.
pub(super) fn clamp_auto_extend_deadline_ms(
    proposed_deadline_ms: i64,
    ceiling_deadline_ms: i64,
) -> (i64, bool) {
    if proposed_deadline_ms > ceiling_deadline_ms {
        (ceiling_deadline_ms, true)
    } else {
        (proposed_deadline_ms, false)
    }
}

/// Extend the watchdog deadline for a channel and move the per-turn max cap
/// with it. Also refreshes the in-memory voice-background handoff marker TTL so
/// extended turns keep their routing metadata (#2352). When `pg_pool` is `Some`
/// the durable PG `expires_at` is refreshed too (`refresh_handoff_ttl_durable`);
/// durable errors are logged and ignored so a PG hiccup cannot break extension.
pub async fn extend_watchdog_deadline(
    channel_id: u64,
    extend_by_secs: u64,
    pg_pool: Option<&sqlx::PgPool>,
) -> Result<
    crate::services::turn_orchestrator::WatchdogDeadlineExtension,
    crate::services::turn_orchestrator::WatchdogDeadlineExtensionError,
> {
    let Some(handle) = ChannelMailboxRegistry::global_handle(ChannelId::new(channel_id)) else {
        return Err(
            crate::services::turn_orchestrator::WatchdogDeadlineExtensionError::MailboxUnavailable,
        );
    };
    let extension = handle.extend_timeout(extend_by_secs).await?;

    // Refresh the handoff marker TTL so a long-running turn does not lose
    // its voice routing metadata (#2352).
    let snapshot = handle.snapshot().await;
    if let Some(message_id) = snapshot.active_user_message_id {
        crate::voice::announce_meta::global_store().refresh_handoff_deadline(message_id);

        if let Some(pool) = pg_pool {
            if let Err(error) =
                crate::voice::announce_meta::refresh_handoff_ttl_durable(pool, message_id).await
            {
                tracing::warn!(
                    channel_id,
                    message_id = message_id.get(),
                    %error,
                    "failed to refresh durable handoff TTL after watchdog extension"
                );
            }
        }
    }

    Ok(extension)
}

/// Read and consume the deadline override for a channel (if any).
pub(super) async fn take_watchdog_deadline_override(
    channel_id: u64,
) -> Option<crate::services::turn_orchestrator::WatchdogDeadlineExtension> {
    ChannelMailboxRegistry::global_handle(ChannelId::new(channel_id))?
        .take_timeout_override()
        .await
}

/// Remove the deadline override for a channel (on turn completion).
pub(super) async fn clear_watchdog_deadline_override(channel_id: u64) {
    if let Some(handle) = ChannelMailboxRegistry::global_handle(ChannelId::new(channel_id)) {
        handle.clear_timeout_override().await;
    }
}

pub(crate) fn clear_inflight_by_tmux_name(provider: &ProviderKind, tmux_name: &str) -> bool {
    inflight::clear_inflight_by_tmux_name(provider, tmux_name)
}

pub(crate) fn clear_inflight_state_for_channel(provider: &ProviderKind, channel_id: u64) {
    inflight::clear_inflight_state(provider, channel_id);
}

pub(crate) fn inflight_state_allows_idle_tmux_repair_for_channel(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<bool> {
    inflight::inflight_state_allows_idle_tmux_repair(provider, channel_id)
}

pub(crate) fn has_fresh_inflight_for_channel(channel_id: u64) -> bool {
    let now_unix_secs = chrono::Local::now().timestamp();
    [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::Gemini,
        ProviderKind::OpenCode,
        ProviderKind::Qwen,
    ]
    .iter()
    .flat_map(load_inflight_states)
    .any(|state| {
        !state.rebind_origin
            && state.channel_id == channel_id
            && !inflight::inflight_state_is_stale(
                &state,
                now_unix_secs,
                inflight::INFLIGHT_STALENESS_THRESHOLD_SECS,
            )
    })
}

async fn has_active_session_for_thread_pg(
    pg_pool: Option<&sqlx::PgPool>,
    thread_id: &str,
) -> Result<bool, String> {
    let Some(pool) = pg_pool else {
        return Ok(false);
    };

    let row = sqlx::query(
        "SELECT 1
         FROM sessions
         WHERE thread_channel_id = $1
           AND LOWER(COALESCE(status, '')) IN ('turn_active', 'working')
           AND COALESCE(last_heartbeat, created_at) > NOW() - INTERVAL '10 minutes'
         LIMIT 1",
    )
    .bind(thread_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load active session for thread {thread_id}: {error}"))?;

    Ok(row.is_some())
}

pub(crate) async fn should_defer_thread_archive_pg(
    pg_pool: Option<&sqlx::PgPool>,
    thread_id: &str,
) -> Result<bool, String> {
    if let Ok(channel_id) = thread_id.parse::<u64>()
        && has_fresh_inflight_for_channel(channel_id)
    {
        return Ok(true);
    }

    has_active_session_for_thread_pg(pg_pool, thread_id).await
}

/// Consume a legacy deferred-restart signal.
///
/// #2713 changed restart semantics to quick-exit + rehydrate: provider TUI/tmux
/// sessions survive process restart, so this helper no longer waits for
/// `global_active` / `global_finalizing` to drain. Callers must persist cheap
/// queue/checkpoint state before invoking it.
pub(super) fn check_deferred_restart(shared: &SharedData) {
    if !shared
        .restart
        .restart_pending
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    // CAS: ensure this provider only decrements once
    if shared
        .restart
        .shutdown_counted
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Relaxed,
        )
        .is_err()
    {
        return;
    }
    if shared
        .restart
        .shutdown_remaining
        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
        != 1
    {
        return;
    }
    let version = crate::agentdesk_runtime_root()
        .map(|root| root.join("restart_pending"))
        .and_then(|marker| {
            let version = fs::read_to_string(&marker).unwrap_or_default();
            let _ = fs::remove_file(&marker);
            Some(version)
        })
        .unwrap_or_default();
    let version = version.trim();
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] 🔄 Deferred restart quick-exit requested for v{version}");
    std::process::exit(0);
}

pub(in crate::services::discord) fn saturating_decrement_counter(counter: &AtomicUsize) -> bool {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_sub(1)
        })
        .is_ok()
}

/// Decrement `global_active` without allowing a stale/restored cleanup path
/// to wrap the counter from 0 to `usize::MAX`.
pub(in crate::services::discord) fn saturating_decrement_global_active(
    shared: &SharedData,
) -> bool {
    saturating_decrement_counter(shared.restart.global_active.as_ref())
}

/// Single authoritative writer for the INCREMENT side of `global_active`,
/// mirroring [`saturating_decrement_global_active`] on the decrement side
/// (#3019, sub-issue of #3016).
///
/// INVARIANT: `global_active` == number of mailbox slots currently in the
/// started-not-yet-finished state. This helper MUST be called +1 IFF a mailbox
/// `try_start_turn` / `recovery_kickoff` actually activated a slot
/// (`started` / `activated_turn == true`); the matching -1 happens IFF a
/// mailbox finish/clear actually removed it (`removed_token.is_some()`). Keeping
/// increment/decrement 1:1 with the real mailbox state transition — NEVER caller
/// intent — is what prevents the drift/underflow seen in #2934.
///
/// Callers are responsible for the mailbox-activation gate; this helper does NOT
/// change WHEN the counter moves, only funnels HOW it moves so increment is
/// single-authority/single-helper exactly like decrement. `reason` is recorded
/// for observability so every increment is attributable to its activation site.
pub(in crate::services::discord) fn increment_global_active(
    shared: &SharedData,
    reason: &str,
) -> usize {
    increment_counter(shared.restart.global_active.as_ref(), reason)
}

fn increment_counter(counter: &AtomicUsize, reason: &str) -> usize {
    let next = counter.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::debug!(
        target: "agentdesk::global_active",
        reason,
        global_active = next,
        "global_active increment"
    );
    next
}

#[cfg(test)]
mod global_active_counter_tests {
    use super::{increment_counter, saturating_decrement_counter};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn saturating_decrement_counter_does_not_underflow_zero() {
        let counter = AtomicUsize::new(0);

        assert!(!saturating_decrement_counter(&counter));
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn saturating_decrement_counter_decrements_positive_value() {
        let counter = AtomicUsize::new(2);

        assert!(saturating_decrement_counter(&counter));
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn increment_counter_increments_once_and_returns_new_value() {
        let counter = AtomicUsize::new(0);

        assert_eq!(increment_counter(&counter, "unit_test"), 1);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn paired_increment_then_decrement_keeps_counter_balanced() {
        let counter = AtomicUsize::new(0);

        // A single activated mailbox transition: +1 on start, -1 on finish.
        increment_counter(&counter, "mailbox_started");
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert!(saturating_decrement_counter(&counter));
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn increment_counter_is_strictly_additive_across_repeated_calls() {
        let counter = AtomicUsize::new(0);

        for expected in 1..=6 {
            assert_eq!(increment_counter(&counter, "repeated_site"), expected);
        }
        assert_eq!(counter.load(Ordering::Relaxed), 6);
    }
}

use session_runtime::{
    DiscordSession, RuntimeChannelBindingStatus, WorktreeInfo, auto_restore_session,
    auto_restore_session_force, auto_restore_session_with_dm_hint, bootstrap_thread_session,
    cleanup_git_worktree, create_git_worktree, detect_worktree_conflict, provider_handles_channel,
    resolve_channel_category, resolve_is_dm_channel, resolve_reusable_worktree,
    resolve_runtime_channel_binding_status, resolve_thread_parent, select_restored_session_path,
    synthetic_thread_channel_name, validate_live_channel_routing,
    validate_live_channel_routing_with_dm_hint,
};

/// Bot-level settings persisted to disk
#[derive(Clone)]
pub(super) struct DiscordBotSettings {
    /// Optional agent identity (e.g. "codex", "spark") for same-provider isolation.
    pub(super) agent: Option<String>,
    pub(super) provider: ProviderKind,
    pub(super) allowed_tools: Vec<String>,
    /// Explicit Discord channel allowlist for this bot token.
    /// Empty means "no channel restriction".
    pub(super) allowed_channel_ids: Vec<u64>,
    /// Channels that require an explicit bot mention before intake proceeds.
    pub(super) require_mention_channel_ids: Vec<u64>,
    /// channel_id (string) → persisted model override
    pub(super) channel_model_overrides: std::collections::HashMap<String, String>,
    /// channel_id (string) → native fast mode enabled
    pub(super) channel_fast_modes: std::collections::HashMap<String, bool>,
    /// channel_id (string) → pending native fast mode reset on the next turn
    pub(super) channel_fast_mode_reset_pending: std::collections::HashSet<String>,
    /// channel_id (string) → Codex goals feature enabled
    pub(super) channel_codex_goals: std::collections::HashMap<String, bool>,
    /// channel_id (string) → pending Codex goals session reset on the next turn
    pub(super) channel_codex_goals_reset_pending: std::collections::HashSet<String>,
    /// channel_id (string) → selected cluster node instance for intake routing
    pub(super) channel_node_overrides: std::collections::HashMap<String, String>,
    /// Discord user ID of the registered owner (must be configured explicitly)
    pub(super) owner_user_id: Option<u64>,
    /// Additional authorized user IDs (added by owner via /adduser)
    pub(super) allowed_user_ids: Vec<u64>,
    /// When true, any Discord user may talk to this bot in allowed channels.
    pub(super) allow_all_users: bool,
    /// Bot IDs whose messages are NOT ignored (e.g. announce bot for CEO directives)
    pub(super) allowed_bot_ids: Vec<u64>,
}

impl Default for DiscordBotSettings {
    fn default() -> Self {
        Self {
            agent: None,
            provider: ProviderKind::Claude,
            allowed_tools: DEFAULT_ALLOWED_TOOLS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            allowed_channel_ids: Vec::new(),
            require_mention_channel_ids: Vec::new(),
            channel_model_overrides: std::collections::HashMap::new(),
            channel_fast_modes: std::collections::HashMap::new(),
            channel_fast_mode_reset_pending: std::collections::HashSet::new(),
            channel_codex_goals: std::collections::HashMap::new(),
            channel_codex_goals_reset_pending: std::collections::HashSet::new(),
            channel_node_overrides: std::collections::HashMap::new(),
            owner_user_id: None,
            allowed_user_ids: Vec::new(),
            allow_all_users: false,
            allowed_bot_ids: Vec::new(),
        }
    }
}

/// Shared state for the Discord bot (multi-channel: each channel has its own session)
/// Handle for a background tmux output watcher
pub(super) struct TmuxWatcherHandle {
    /// Tmux session this watcher owns. Used to enforce the single-watcher
    /// policy when the same session is reattached through another path.
    pub(super) tmux_session_name: String,
    /// JSONL/transcript path this watcher tails for the session. A single tmux
    /// session can change relay files when it graduates from the prelaunch
    /// wrapper to a provider-native TUI handoff.
    pub(super) output_path: String,
    /// Signal to pause monitoring (while Discord handler reads its own turn)
    pub(super) paused: Arc<std::sync::atomic::AtomicBool>,
    /// After Discord handler finishes its turn, set this offset so watcher resumes from here
    pub(super) resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    /// Signal to cancel the watcher (quiet exit, no "session ended" message)
    pub(super) cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Epoch counter: incremented each time paused is set to true.
    /// Watcher snapshots this before reading; if it changed, the read is stale.
    pub(super) pause_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Set by turn_bridge when it delivers the response directly (non-handoff path).
    /// Watcher checks this before relay to avoid duplicate messages.
    pub(super) turn_delivered: Arc<std::sync::atomic::AtomicBool>,
    /// Updated by the watcher task loop. If this stops moving while the registry
    /// still has a slot, the slot is stale and must not suppress a new watcher.
    pub(super) last_heartbeat_ts_ms: Arc<std::sync::atomic::AtomicI64>,
}

// #3016 phase-5b2: the per-handle `mailbox_finalize_owed: Arc<AtomicBool>` field
// (#1452 turn-scoped bridge→watcher finalization debt) has been removed. Phase-5b1
// replaced every finalize-decision consumer of the flag — the watcher's
// normal-completion finalize now fires on the confirmed-completion / structural
// signal (`normal_completion = true`), and the bridge-handoff invariant uses the
// ledger's `register_start(RelayOwnerKind::Watcher)` authority — so the flag was
// write-only and is now deleted entirely with identical behaviour.

pub(super) const TMUX_WATCHER_STALE_HEARTBEAT_MS: i64 = 60_000;

pub(super) fn tmux_watcher_now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl TmuxWatcherHandle {
    pub(super) fn heartbeat_stale(&self) -> bool {
        let last = self
            .last_heartbeat_ts_ms
            .load(std::sync::atomic::Ordering::Acquire);
        last <= 0 || tmux_watcher_now_ms().saturating_sub(last) > TMUX_WATCHER_STALE_HEARTBEAT_MS
    }
}

pub(super) type TmuxWatcherRegistryGuard = std::sync::MutexGuard<'static, ()>;

static TMUX_WATCHER_REGISTRY_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

pub(super) fn lock_tmux_watcher_registry() -> TmuxWatcherRegistryGuard {
    TMUX_WATCHER_REGISTRY_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Registry for active tmux output watchers.
///
/// Ownership is keyed by tmux session name so duplicate attaches for the same
/// live session converge before a second relay can spawn. A channel index is
/// retained for existing routing and diagnostics callers that ask "does this
/// Discord channel currently have watcher coverage?".
pub(super) struct TmuxWatcherRegistry {
    by_tmux_session: dashmap::DashMap<String, TmuxWatcherHandle>,
    tmux_session_by_channel: dashmap::DashMap<ChannelId, String>,
    owner_channel_by_tmux_session: dashmap::DashMap<String, ChannelId>,
    /// #3105: authoritative owner-channel bindings re-registered for LIVE tmux
    /// sessions that currently have no live watcher handle — e.g. a Claude TUI
    /// session the user is typing into directly whose watcher slot was evicted
    /// by a compact/restart/rebind and never re-claimed (no foreground turn).
    ///
    /// This is part of the authoritative registry, NOT the `tui_prompt_dedupe`
    /// mirror: it is sourced only from the configured channel→provider bindings
    /// (`settings::list_registered_channel_bindings`), which deterministically
    /// resolve a session's owner channel from its (base or thread-suffixed)
    /// tmux name. Kept in a separate map so the strict 1:1 watcher-handle
    /// invariant across the three maps above is untouched; the live watcher map
    /// always wins on lookup, and a real watcher claim for the session clears
    /// the restored entry so it can never shadow live truth.
    restored_owner_by_tmux_session: dashmap::DashMap<String, ChannelId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TmuxWatcherBinding {
    pub(super) owner_channel_id: ChannelId,
    pub(super) tmux_session_name: String,
}

impl TmuxWatcherRegistry {
    pub(super) fn new() -> Self {
        Self {
            by_tmux_session: dashmap::DashMap::new(),
            tmux_session_by_channel: dashmap::DashMap::new(),
            owner_channel_by_tmux_session: dashmap::DashMap::new(),
            restored_owner_by_tmux_session: dashmap::DashMap::new(),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.by_tmux_session.len()
    }

    pub(super) fn contains_key(&self, channel_id: &ChannelId) -> bool {
        self.channel_binding(channel_id)
            .and_then(|binding| self.by_tmux_session.get(&binding.tmux_session_name))
            .is_some()
    }

    pub(super) fn get(
        &self,
        channel_id: &ChannelId,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, TmuxWatcherHandle>> {
        let tmux_session_name = self.tmux_session_by_channel.get(channel_id)?.clone();
        self.by_tmux_session.get(&tmux_session_name)
    }

    // #3034: test-only convenience wrapper (prod code calls `insert_locked`
    // with an explicit registry guard). Used only by `#[cfg(test)]` setup.
    #[allow(dead_code)]
    pub(super) fn insert(
        &self,
        channel_id: ChannelId,
        handle: TmuxWatcherHandle,
    ) -> Option<TmuxWatcherHandle> {
        let guard = lock_tmux_watcher_registry();
        self.insert_locked(&guard, channel_id, handle)
    }

    pub(super) fn insert_locked(
        &self,
        _guard: &TmuxWatcherRegistryGuard,
        channel_id: ChannelId,
        handle: TmuxWatcherHandle,
    ) -> Option<TmuxWatcherHandle> {
        if let Some((_, old_tmux_session_name)) = self.tmux_session_by_channel.remove(&channel_id) {
            self.owner_channel_by_tmux_session
                .remove(&old_tmux_session_name);
            self.by_tmux_session.remove(&old_tmux_session_name);
        }

        let tmux_session_name = handle.tmux_session_name.clone();
        if let Some((_, old_owner_channel_id)) = self
            .owner_channel_by_tmux_session
            .remove(&tmux_session_name)
        {
            self.tmux_session_by_channel.remove(&old_owner_channel_id);
        }

        // #3105: a live watcher handle is now the authoritative owner for this
        // session — drop any restored owner-only binding so it can never shadow
        // or contradict live truth.
        self.restored_owner_by_tmux_session
            .remove(&tmux_session_name);

        self.tmux_session_by_channel
            .insert(channel_id, tmux_session_name.clone());
        self.owner_channel_by_tmux_session
            .insert(tmux_session_name.clone(), channel_id);
        self.by_tmux_session.insert(tmux_session_name, handle)
    }

    pub(super) fn remove(&self, channel_id: &ChannelId) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let guard = lock_tmux_watcher_registry();
        self.remove_locked(&guard, channel_id)
    }

    pub(super) fn remove_locked(
        &self,
        _guard: &TmuxWatcherRegistryGuard,
        channel_id: &ChannelId,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let (_, tmux_session_name) = self.tmux_session_by_channel.remove(channel_id)?;
        self.owner_channel_by_tmux_session
            .remove(&tmux_session_name);
        self.by_tmux_session
            .remove(&tmux_session_name)
            .map(|(_, handle)| (*channel_id, handle))
    }

    pub(super) fn remove_tmux_session_locked(
        &self,
        _guard: &TmuxWatcherRegistryGuard,
        tmux_session_name: &str,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let (_, owner_channel_id) = self
            .owner_channel_by_tmux_session
            .remove(tmux_session_name)?;
        self.tmux_session_by_channel.remove(&owner_channel_id);
        self.by_tmux_session
            .remove(tmux_session_name)
            .map(|(_, handle)| (owner_channel_id, handle))
    }

    pub(super) fn remove_tmux_session_if_current(
        &self,
        tmux_session_name: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let guard = lock_tmux_watcher_registry();
        let is_current = self
            .by_tmux_session
            .get(tmux_session_name)
            .is_some_and(|entry| Arc::ptr_eq(&entry.cancel, expected_cancel));
        if !is_current {
            return None;
        }
        self.remove_tmux_session_locked(&guard, tmux_session_name)
    }

    pub(super) fn cancel_and_remove_channel_if_current(
        &self,
        channel_id: &ChannelId,
        expected_tmux_session_name: &str,
        expected_output_path: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> bool {
        let guard = lock_tmux_watcher_registry();
        let Some(tmux_session_name) = self
            .tmux_session_by_channel
            .get(channel_id)
            .map(|entry| entry.clone())
        else {
            return false;
        };
        if tmux_session_name != expected_tmux_session_name {
            return false;
        }
        let matches_current = self
            .by_tmux_session
            .get(&tmux_session_name)
            .is_some_and(|entry| {
                entry.output_path == expected_output_path
                    && Arc::ptr_eq(&entry.cancel, expected_cancel)
            });
        if !matches_current {
            return false;
        }
        let Some((_, handle)) = self.remove_locked(&guard, channel_id) else {
            return false;
        };
        handle
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        true
    }

    pub(super) fn iter(&self) -> dashmap::iter::Iter<'_, String, TmuxWatcherHandle> {
        self.by_tmux_session.iter()
    }

    pub(super) fn channel_binding(&self, channel_id: &ChannelId) -> Option<TmuxWatcherBinding> {
        let tmux_session_name = self.tmux_session_by_channel.get(channel_id)?.clone();
        let owner_channel_id = self
            .owner_channel_by_tmux_session
            .get(&tmux_session_name)
            .map(|entry| *entry.value())
            .unwrap_or(*channel_id);
        Some(TmuxWatcherBinding {
            owner_channel_id,
            tmux_session_name,
        })
    }

    pub(super) fn owner_channel_for_tmux_session(
        &self,
        tmux_session_name: &str,
    ) -> Option<ChannelId> {
        // The live watcher-handle binding is the primary authority. When no live
        // watcher owns the session (e.g. a TUI-direct session whose slot was
        // evicted by compact/restart/rebind), fall back to the #3105 restored
        // owner map — still authoritative (settings-derived), unlike the dedupe
        // mirror, which is never consulted here.
        self.owner_channel_by_tmux_session
            .get(tmux_session_name)
            .map(|entry| *entry.value())
            .or_else(|| {
                self.restored_owner_by_tmux_session
                    .get(tmux_session_name)
                    .map(|entry| *entry.value())
            })
    }

    /// #3105: re-register the authoritative owner channel for a LIVE tmux
    /// session that currently has no live watcher handle.
    ///
    /// This is the self-heal path for the permanent relay drop described in
    /// #3105: the idle transcript relay resolves a session's owner channel
    /// deterministically from the configured channel→provider bindings (which
    /// handle both base and thread-suffixed tmux names) and promotes that
    /// evidence into the authoritative registry here, instead of routing from
    /// the `tui_prompt_dedupe` mirror (which #3018 forbids as a reverse
    /// authority).
    ///
    /// No-ops when a live watcher already owns the session (live truth wins) or
    /// when the binding is unchanged. Returns `true` only on the first/changed
    /// registration so callers can emit a single bounded incident instead of a
    /// per-poll log.
    pub(super) fn restore_owner_channel_for_tmux_session(
        &self,
        tmux_session_name: &str,
        channel_id: ChannelId,
    ) -> bool {
        let _guard = lock_tmux_watcher_registry();
        if self
            .owner_channel_by_tmux_session
            .contains_key(tmux_session_name)
        {
            // A live watcher handle already owns this session authoritatively.
            self.restored_owner_by_tmux_session
                .remove(tmux_session_name);
            return false;
        }
        let changed = self
            .restored_owner_by_tmux_session
            .get(tmux_session_name)
            .map(|entry| *entry.value())
            != Some(channel_id);
        self.restored_owner_by_tmux_session
            .insert(tmux_session_name.to_string(), channel_id);
        changed
    }

    /// #3105: drop a restored owner-only binding (e.g. once the session is no
    /// longer live). Idempotent.
    pub(super) fn clear_restored_owner_for_tmux_session(&self, tmux_session_name: &str) {
        self.restored_owner_by_tmux_session
            .remove(tmux_session_name);
    }

    /// #3105 (codex P1 sub-case B): true when a LIVE watcher handle currently
    /// owns this tmux session. Used to distinguish a genuinely dead/orphaned
    /// session (no live watcher) from a live session whose authoritative owner
    /// map entry was transiently evicted (which must self-heal, not be tombstoned).
    pub(super) fn has_live_watcher_handle(&self, tmux_session_name: &str) -> bool {
        self.by_tmux_session.contains_key(tmux_session_name)
    }

    pub(super) fn tmux_session_is_stale(&self, tmux_session_name: &str) -> Option<bool> {
        self.by_tmux_session
            .get(tmux_session_name)
            .map(|entry| entry.heartbeat_stale())
    }

    pub(super) fn tmux_session_live_for_relay(&self, tmux_session_name: &str) -> Option<bool> {
        self.by_tmux_session.get(tmux_session_name).map(|entry| {
            !entry.cancel.load(std::sync::atomic::Ordering::Relaxed) && !entry.heartbeat_stale()
        })
    }

    /// #2843: the output path the live watcher (if any) is tailing for this tmux
    /// session. The Claude idle relay uses this to decide whether a non-stale
    /// watcher genuinely covers the freshest transcript (and thus already relays
    /// it). Comparing against the runtime *binding* is wrong: re-registering the
    /// binding does not retarget the already-running watcher, so the binding and
    /// the watcher can point at different files.
    pub(super) fn watcher_output_path(&self, tmux_session_name: &str) -> Option<String> {
        self.by_tmux_session
            .get(tmux_session_name)
            .map(|entry| entry.output_path.clone())
    }
}

#[cfg(test)]
mod tmux_watcher_registry_restore_tests {
    use super::*;

    fn live_watcher_handle(tmux_session_name: &str) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: format!("/tmp/{tmux_session_name}.jsonl"),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(
                std::sync::atomic::AtomicI64::new(tmux_watcher_now_ms()),
            ),
        }
    }

    // #3105: a LIVE TUI session whose authoritative watcher-handle binding is
    // missing (slot evicted by compact/restart/rebind, never re-claimed) must be
    // self-healable via an authoritative re-registration so the idle relay can
    // route again — instead of dropping every poll forever. This is the
    // registry-side half of the fix; it asserts the restore is treated as
    // authoritative on lookup and does NOT depend on the dedupe mirror.
    #[test]
    fn restored_owner_makes_missing_registry_resolve_for_live_session() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-claude-adk-cc-t1504468805772902471";
        let channel = ChannelId::new(1_504_468_805_772_902_471);

        // No live watcher handle yet → registry misses (the #3018 drop trigger).
        assert_eq!(registry.owner_channel_for_tmux_session(tmux), None);

        // Authoritative (settings-derived) re-registration repairs the miss.
        assert!(
            registry.restore_owner_channel_for_tmux_session(tmux, channel),
            "first restore must report a change so a single bounded incident is emitted"
        );
        assert_eq!(
            registry.owner_channel_for_tmux_session(tmux),
            Some(channel),
            "restored owner must resolve authoritatively from the registry"
        );

        // Re-applying the same binding is a no-op (no repeated per-poll incident).
        assert!(
            !registry.restore_owner_channel_for_tmux_session(tmux, channel),
            "unchanged restore must not re-report a change"
        );
    }

    // A real live watcher handle is the primary authority and must win over (and
    // evict) any restored owner-only binding — restored entries can never shadow
    // or contradict live truth.
    #[test]
    fn live_watcher_handle_overrides_and_evicts_restored_owner() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-claude-adk-cc-t1504468805772902471";
        let restored_channel = ChannelId::new(111_000_000_000_000);
        let live_channel = ChannelId::new(222_000_000_000_000);

        registry.restore_owner_channel_for_tmux_session(tmux, restored_channel);
        assert_eq!(
            registry.owner_channel_for_tmux_session(tmux),
            Some(restored_channel)
        );

        // Claiming a live watcher handle takes over and drops the restored entry.
        registry.insert(live_channel, live_watcher_handle(tmux));
        assert_eq!(
            registry.owner_channel_for_tmux_session(tmux),
            Some(live_channel),
            "live watcher handle must win over a restored owner-only binding"
        );

        // Removing the live watcher must NOT resurrect the evicted restored entry.
        registry.remove(&live_channel);
        assert_eq!(
            registry.owner_channel_for_tmux_session(tmux),
            None,
            "evicted restored entry must not resurrect after the live watcher is removed"
        );
    }

    // Restoring an owner while a live watcher already owns the session must be a
    // no-op (and clear any leftover restored entry) — never override live truth.
    #[test]
    fn restore_is_noop_when_live_watcher_owns_session() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-claude-adk-cc";
        let live_channel = ChannelId::new(333_000_000_000_000);

        registry.insert(live_channel, live_watcher_handle(tmux));
        assert!(
            !registry
                .restore_owner_channel_for_tmux_session(tmux, ChannelId::new(444_000_000_000_000)),
            "restore must not report a change when a live watcher owns the session"
        );
        assert_eq!(
            registry.owner_channel_for_tmux_session(tmux),
            Some(live_channel),
            "live watcher owner must be unchanged by a restore attempt"
        );
    }

    // The base and thread-suffixed tmux names are distinct registry keys; a
    // restored owner for the thread-suffixed live session resolves on its own
    // exact key (the relay resolves the channel from the suffixed name).
    #[test]
    fn base_and_thread_suffixed_names_resolve_independently() {
        let registry = TmuxWatcherRegistry::new();
        let base = "AgentDesk-claude-adk-cc";
        let suffixed = "AgentDesk-claude-adk-cc-t1504468805772902471";
        let channel = ChannelId::new(1_504_468_805_772_902_471);

        registry.restore_owner_channel_for_tmux_session(suffixed, channel);
        assert_eq!(
            registry.owner_channel_for_tmux_session(suffixed),
            Some(channel)
        );
        assert_eq!(
            registry.owner_channel_for_tmux_session(base),
            None,
            "the base name must not borrow the thread-suffixed session's owner"
        );
    }

    // Clearing a restored owner (e.g. when the pane is no longer live) must drop
    // the binding so a dead session can never resolve.
    #[test]
    fn clear_restored_owner_drops_binding() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-claude-adk-cc-t1504468805772902471";
        let channel = ChannelId::new(1_504_468_805_772_902_471);

        registry.restore_owner_channel_for_tmux_session(tmux, channel);
        registry.clear_restored_owner_for_tmux_session(tmux);
        assert_eq!(registry.owner_channel_for_tmux_session(tmux), None);
    }

    #[test]
    fn cancel_and_remove_channel_if_current_only_rolls_back_matching_claim() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-codex-adk-cdx";
        let channel = ChannelId::new(1_504_468_805_772_902_471);
        let handle = live_watcher_handle(tmux);
        let expected_output_path = handle.output_path.clone();
        let expected_cancel = handle.cancel.clone();
        registry.insert(channel, handle);

        assert!(
            !registry.cancel_and_remove_channel_if_current(
                &channel,
                tmux,
                "/tmp/different.jsonl",
                &expected_cancel
            ),
            "output-path mismatch must not remove a possibly newer watcher"
        );
        assert_eq!(registry.owner_channel_for_tmux_session(tmux), Some(channel));
        assert!(!expected_cancel.load(std::sync::atomic::Ordering::Relaxed));

        assert!(registry.cancel_and_remove_channel_if_current(
            &channel,
            tmux,
            &expected_output_path,
            &expected_cancel
        ));
        assert_eq!(registry.owner_channel_for_tmux_session(tmux), None);
        assert!(expected_cancel.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn tmux_session_is_stale_does_not_fold_cancel_flag_into_heartbeat() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-codex-adk-cdx-fresh-cancel";
        let channel = ChannelId::new(1_504_468_805_772_902_472);
        let handle = live_watcher_handle(tmux);
        let cancel = handle.cancel.clone();
        registry.insert(channel, handle);

        cancel.store(true, std::sync::atomic::Ordering::Relaxed);

        assert_eq!(
            registry.tmux_session_is_stale(tmux),
            Some(false),
            "a fresh heartbeat watcher with an early cancel flag is cancelled, not heartbeat-stale"
        );
        assert_eq!(
            registry.tmux_session_live_for_relay(tmux),
            Some(false),
            "the same cancelled handle is still not relay-live; cancel is evaluated separately"
        );
    }
}

/// Per-channel coordination for watcher-to-Discord relay emission.
///
/// Shared across watcher-handle replacements, this serializes overlapping
/// outgoing/successor relay emission and exposes the confirmed-output watermark.
/// Scope: intra-process only; restart-persistent dedupe remains in
/// `InflightTurnState::last_watcher_relayed_offset`.
pub(super) struct TmuxRelayCoord {
    /// Non-zero while some watcher instance is actively emitting a relay for
    /// this channel. Holds the `data_start_offset` of the in-progress emission.
    /// Acquired via `compare_exchange(0, offset)` — only one watcher can
    /// hold the slot, so concurrent attempts from outgoing+incoming watchers
    /// serialize rather than double-fire.
    pub(super) relay_slot: Arc<std::sync::atomic::AtomicU64>,
    /// End offset (exclusive) of the last relay this process has confirmed
    /// delivery for. 0 = no confirmed delivery yet this process lifetime.
    ///
    /// #3017: this is the single output-offset authority for the relay-dedup
    /// paths (read via `SharedData::committed_relay_offset`, advanced by the
    /// watcher's `advance_watcher_confirmed_end`). For an inflight-less wake /
    /// idle-background / monitor-auto-turn turn, the secondary relay actors
    /// (idle-JSONL relay, session-bound sink) CONSULT this watermark so a
    /// byte-range the watcher already committed is relayed exactly once
    /// regardless of which actor observes it first (the E-13 dedup invariant).
    /// For a normal Discord-origin turn (inflight present) the watcher remains
    /// sole relay owner; only no-inflight wake/idle paths gate on this watermark.
    pub(super) confirmed_end_offset: Arc<std::sync::atomic::AtomicU64>,
    pub(in crate::services::discord) reset_state:
        std::sync::Mutex<relay_health::FrontierResetState>,
    /// Wall-clock timestamp (ms since epoch) of the most recent confirmed
    /// relay. 0 = no confirmed relay observed yet. Read by the
    /// `watcher-state` observability endpoint (#964). Monotonic is NOT
    /// required — this is a telemetry field only.
    pub(super) last_relay_ts_ms: Arc<std::sync::atomic::AtomicI64>,
    /// Number of watcher reattach/reconnect spawns observed for this channel
    /// in the current dcserver process. Exposed through watcher-state (#964).
    pub(super) reconnect_count: Arc<std::sync::atomic::AtomicU64>,
    /// `.generation` marker file mtime (nanos since epoch) snapshotted the
    /// last time `confirmed_end_offset` was advanced. 0 = never observed.
    ///
    /// `reset_stale_relay_watermark_if_output_regressed` (#1270) uses this
    /// to distinguish two output-regression scenarios that look identical
    /// at the byte level:
    ///   - Mid-flight rotation (`truncate_jsonl_head_safe` rename — same
    ///     wrapper, same `.generation` mtime): pin watermark to current
    ///     EOF so we don't re-relay surviving content (PR #1256 intent).
    ///   - Cancel→respawn (`cleanup_session_temp_files` deletes
    ///     `.generation`, claude.rs writes a fresh one — new wrapper, new
    ///     mtime): reset watermark to 0 so the genuinely-new response is
    ///     relayed.
    ///
    /// `.generation` is the stable wrapper-identity signal because it's
    /// written once per spawn and never touched by the live wrapper, so its
    /// mtime survives jsonl rotation but flips on a fresh spawn.
    pub(super) confirmed_end_generation_mtime_ns: Arc<std::sync::atomic::AtomicI64>,
    /// #3041 P1-1: the LIVE per-channel delivery lease. Added ALONGSIDE
    /// `relay_slot` (which is NOT removed yet — its guard migration is a later
    /// step). The watcher acquires this before delivering the terminal response
    /// and commits it after; the commit is what advances `confirmed_end_offset`
    /// (replacing the watcher's inline advance). Shared via `Arc` across all
    /// watcher instances for the channel so a replacement watcher observes a
    /// live holder's lease and skips the duplicate send (the §5.2 B2 invariant).
    pub(in crate::services::discord) delivery_lease: Arc<DeliveryLeaseCell>,
}

impl TmuxRelayCoord {
    pub(super) fn new(channel_id: ChannelId) -> Self {
        Self {
            relay_slot: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            confirmed_end_offset: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            reset_state: std::sync::Mutex::new(relay_health::FrontierResetState::default()),
            last_relay_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            reconnect_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            confirmed_end_generation_mtime_ns: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            delivery_lease: Arc::new(DeliveryLeaseCell::new(channel_id)),
        }
    }

    pub(super) fn note_relay_progress_heartbeat(&self, now_ms: i64) {
        self.last_relay_ts_ms.store(now_ms, Ordering::Release);
    }
}

#[cfg(test)]
mod relay_coord_tests {
    use super::*;

    #[test]
    fn relay_progress_heartbeat_stamps_each_confirmed_chunk() {
        let coord = TmuxRelayCoord::new(ChannelId::new(4_178));

        coord.note_relay_progress_heartbeat(1_000);
        assert_eq!(coord.last_relay_ts_ms.load(Ordering::Acquire), 1_000);

        coord.note_relay_progress_heartbeat(1_500);
        assert_eq!(coord.last_relay_ts_ms.load(Ordering::Acquire), 1_500);
        assert_eq!(coord.confirmed_end_offset.load(Ordering::Acquire), 0);
    }
}

// ===========================================================================
// #3041 §2-§3 — Delivery-lease `DeliveryLeaseCell` state machine.
//
// As of P1-1 the WATCHER terminal-delivery path wires this LIVE: the watcher
// acquires the cell before sending, heartbeat-renews it during the send, and
// commits+advances+releases INLINE. The `relay_slot` field above is LEFT
// UNTOUCHED for now (its guard migration is a later step). The SINK/BRIDGE
// committers (P1-2) and the 3-way ACK reconciliation (P1-3) are not wired yet,
// so the actor `CommitDelivery`/`ReleaseDelivery` messages and some helpers
// remain dormant — those still carry targeted `#[allow(dead_code)]` attributes
// tagged with this issue/phase, to be wired/removed by the follow-up phases.
//
// Design (faithful to #3041 §2-§3):
//   lease = (delivery_lease_key, byte_range [start,end))
//           → a "one-time terminal-delivery right".
//   The lease key is deliberately separate from the finalizer's `TurnKey`: the
//   finalizer keeps its id-0 channel-collapse semantics, while delivery leasing
//   needs id-0 turns disambiguated by their inflight start identity.
//   State machine:
//     Unleased --(CAS acquire)--> Leased{holder, deadline, range}
//               --(commit)-------> Committed{Delivered|NotDelivered|Unknown}
//               --(release)------> Unleased
//     deadline reclaim: Leased --(deadline elapsed)--> Unleased
// ===========================================================================

/// Who currently holds (or is attempting to hold) the delivery lease.
///
/// #3041 P1-0: dormant, wired in P1-1.. — the holder is matched on
/// compare-and-release so an actor can only release a lease it actually owns.
#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) enum LeaseHolder {
    /// A tmux watcher instance. `instance_id` distinguishes an outgoing
    /// watcher from its successor across a reattach so a stale watcher cannot
    /// release the live watcher's lease.
    Watcher { instance_id: u64 },
    /// The standby / output sink relay.
    Sink,
    /// The bridge (turn-bridge handoff path).
    Bridge,
}

/// The three-way commit outcome (#3041 §3). `Unknown` is the safety value for
/// any ambiguous terminal (drop / panic / partial write) and MUST NOT advance
/// the confirmed-delivery offset — only `Delivered` does.
///
/// #3041 P1-0: dormant, wired in P1-1...
#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) enum LeaseOutcome {
    /// Terminal output was confirmed delivered to Discord; the offset may
    /// advance to `end`.
    Delivered,
    /// Delivery was intentionally suppressed / not performed; offset unchanged.
    NotDelivered,
    /// Ambiguous (drop / panic / partial). Offset MUST NOT advance.
    Unknown,
}

/// The lease state machine value, owned behind the cell's mutex. The `AtomicU8`
/// tag below is the single-winner CAS gate for acquire; this payload is only
/// ever mutated by that winner (or by a deadline reclaim), and every mutation
/// flips the tag AND writes the payload under the SAME mutex, so the tag and
/// payload are always observed coherently (#3041 codex). `read()` also takes
/// the mutex — there is no lock-free read fast path.
///
/// #3041 P1-0: dormant, wired in P1-1...
#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
#[derive(Clone, Debug)]
enum LeaseState {
    /// No holder; the lease is available to acquire.
    Unleased,
    /// Held by `holder` for delivery identity `key` until `deadline` (monotonic ms
    /// since process start); covers the half-open byte range `[start, end)`.
    /// The lease key is the FULL `(DeliveryLeaseKey, [start,end))` identity
    /// (#3041 §2): `commit`/`release` verify it so a stale commit or release
    /// from an OLDER turn (or the same turn with a different range) cannot act
    /// on a reacquired NEWER lease. `reclaim_if_expired` is intentionally
    /// deadline-only (identity-agnostic) — it force-returns an expired lease
    /// regardless of holder/key so a dead holder cannot strand the cell.
    Leased {
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        deadline_ms: u64,
        start: u64,
        end: u64,
    },
    /// Committed with a three-way outcome; carries the same `(holder, key,
    /// range)` identity forward so a stale release is rejected. Awaits a
    /// `release` to return to `Unleased`.
    Committed {
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        start: u64,
        end: u64,
        outcome: LeaseOutcome,
    },
}

/// #3041 P1-1: process-monotonic millisecond clock for delivery-lease
/// deadlines. The acquire deadline and the reconciler's `reclaim_if_expired`
/// MUST read the SAME clock; a wall clock would jump on NTP steps and could
/// reclaim a live holder or strand a dead one. Anchored to a process-start
/// `Instant` so it is purely monotonic (never goes backwards). NOTE: this is a
/// real wall-monotonic clock, not the Tokio test clock; gated-clock tests drive
/// `reclaim_if_expired` with explicit `now_ms` arguments rather than this fn.
pub(in crate::services::discord) fn lease_now_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// Internal CAS gate tag for the [`DeliveryLeaseCell`]. The CAS that flips
/// `UNLEASED → LEASED` is the single-winner acquire primitive — exactly one
/// acquirer wins; concurrent losers serialize on the payload mutex and observe
/// a non-`UNLEASED` tag under the lock. The tag is taken/flipped under the
/// payload mutex (never on its own); it is NOT a lock-free read fast path —
/// `read()` always takes the mutex (#3041 R1 coherence fix).
const TAG_UNLEASED: u8 = 0;
const TAG_LEASED: u8 = 1;
const TAG_COMMITTED: u8 = 2;

/// One-time terminal-delivery right for a single `(channel, turn, byte_range)`
/// (#3041 §2-§3). DORMANT in P1-0 — added alongside, NOT replacing,
/// `TmuxRelayCoord::relay_slot`. The `state_tag` is the single-winner CAS
/// acquire primitive; the `payload` mutex carries the rich lease state (holder
/// / deadline / range / outcome). The tag flip, payload write, and `read()` all
/// happen under the one mutex, so they are always mutually coherent.
///
/// #3041 P1-0: dormant, wired in P1-1...
#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
pub(in crate::services::discord) struct DeliveryLeaseCell {
    /// The channel this lease coordinates. Part of the lease identity.
    channel_id: ChannelId,
    /// Internal CAS gate tag (`TAG_*`). The acquire CAS on this word is the
    /// single-winner gate; it is flipped under the payload mutex, NOT lock-free
    /// for readers — `read()` takes the mutex.
    state_tag: std::sync::atomic::AtomicU8,
    /// Rich lease payload. Mutated by the CAS winner or a deadline reclaim, and
    /// read by `read()` — all under this one mutex (the coherence invariant).
    payload: std::sync::Mutex<LeaseState>,
}

/// A point-in-time snapshot of a [`DeliveryLeaseCell`], returned by `read()`
/// (which materializes it under the payload mutex).
///
/// #3041 P1-0: dormant, wired in P1-1...
#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
#[derive(Clone, Debug)]
pub(in crate::services::discord) enum LeaseSnapshot {
    Unleased,
    Leased {
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        deadline_ms: u64,
        start: u64,
        end: u64,
    },
    Committed {
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        start: u64,
        end: u64,
        outcome: LeaseOutcome,
    },
}

#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
impl DeliveryLeaseCell {
    /// Construct a fresh `Unleased` cell for `channel_id`. The lease key and
    /// byte range are supplied per-acquire, not at
    /// construction, so one cell serves the channel across sequential turns.
    pub(in crate::services::discord) fn new(channel_id: ChannelId) -> Self {
        Self {
            channel_id,
            state_tag: std::sync::atomic::AtomicU8::new(TAG_UNLEASED),
            payload: std::sync::Mutex::new(LeaseState::Unleased),
        }
    }

    /// The channel this lease coordinates.
    pub(in crate::services::discord) fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Read the current lease state. Always materialized UNDER the payload
    /// mutex so the snapshot can never disagree with a concurrently-acquiring
    /// writer (#3041 codex): because `try_acquire`/`commit`/`release`/`reclaim`
    /// flip `state_tag` AND write `payload` while holding the SAME mutex, any
    /// observer that takes the lock sees a tag/payload pair that are mutually
    /// coherent. `state_tag` remains the single-winner CAS gate for acquire; it
    /// is NOT used as a lock-free read fast-path here because that reintroduced
    /// the publish/observe window the codex review flagged.
    pub(in crate::services::discord) fn read(&self) -> LeaseSnapshot {
        let guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*guard {
            LeaseState::Unleased => LeaseSnapshot::Unleased,
            LeaseState::Leased {
                holder,
                key,
                deadline_ms,
                start,
                end,
            } => LeaseSnapshot::Leased {
                holder: *holder,
                key: key.clone(),
                deadline_ms: *deadline_ms,
                start: *start,
                end: *end,
            },
            LeaseState::Committed {
                holder,
                key,
                start,
                end,
                outcome,
            } => LeaseSnapshot::Committed {
                holder: *holder,
                key: key.clone(),
                start: *start,
                end: *end,
                outcome: *outcome,
            },
        }
    }

    /// CAS-acquire the lease for the full `(delivery_lease_key, [start,end))`
    /// identity (#3041 §2) on behalf of `holder` until `deadline_ms`. Records
    /// `key` so a later `commit`/`release` carrying a STALE older lease key is
    /// rejected (the §2 hazard: a reclaim+reacquire reuses the same holder kind,
    /// so holder alone is insufficient).
    ///
    /// Ordering invariant (codex coherence fix): the tag CAS and the payload
    /// write happen UNDER the SAME mutex, and `read()` also locks, so a tag and
    /// its payload are never observed out of step. The CAS keeps single-winner
    /// semantics — exactly one acquirer flips `UNLEASED → LEASED`; every
    /// concurrent loser (already holding the lock by then) sees a non-`UNLEASED`
    /// tag under the lock and returns `false` without mutating the payload.
    pub(in crate::services::discord) fn try_acquire(
        &self,
        key: DeliveryLeaseKey,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        deadline_ms: u64,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Single-winner gate, taken while holding the payload lock so the tag
        // flip and the payload write publish together. Concurrent acquirers
        // serialize on the mutex; whoever runs second sees a non-`UNLEASED` tag.
        if self
            .state_tag
            .compare_exchange(
                TAG_UNLEASED,
                TAG_LEASED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        *guard = LeaseState::Leased {
            holder,
            key,
            deadline_ms,
            start,
            end,
        };
        true
    }

    /// Commit the lease three-way (#3041 §3). Verifies the FULL `(holder, key,
    /// [start,end))` identity against the currently-`Leased` lease (#3041 §2):
    /// any mismatch — wrong holder, a STALE older lease key, or a different range
    /// — or a non-`Leased` state is a no-op that returns `false`. This closes
    /// the §2 hazard where a stale commit from an older turn could act on a
    /// reacquired same-channel/same-holder-kind lease. On success the tag
    /// advances `LEASED → COMMITTED` (under the lock) and the outcome is
    /// recorded. `Unknown` records but the caller MUST NOT advance the offset.
    pub(in crate::services::discord) fn commit(
        &self,
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        start: u64,
        end: u64,
        outcome: LeaseOutcome,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*guard {
            LeaseState::Leased {
                holder: cur_holder,
                key: cur_key,
                start: cur_start,
                end: cur_end,
                ..
            } if *cur_holder == holder
                && cur_key == &key
                && *cur_start == start
                && *cur_end == end =>
            {
                *guard = LeaseState::Committed {
                    holder,
                    key,
                    start,
                    end,
                    outcome,
                };
                self.state_tag.store(TAG_COMMITTED, Ordering::Release);
                true
            }
            // Identity mismatch (holder / stale turn / range) or not Leased.
            _ => false,
        }
    }

    /// Compare-and-release: return the cell to `Unleased` ONLY if the FULL
    /// `(holder, key, [start,end))` identity matches the recorded lease (#3041
    /// §2-§3) — symmetric with `commit`. Verifying the key AND the byte range
    /// (not just the holder) is what closes the §2 hazard: a stale release from
    /// an OLDER turn — or from the SAME turn but an OLDER byte range after a
    /// reclaim+reacquire re-leased a different range (e.g. a continuation chunk)
    /// — is a no-op returning `false`, so it can never release the live newer
    /// lease. A release is valid from either `Leased` (abandoned without commit)
    /// or `Committed` (the normal post-commit release).
    pub(in crate::services::discord) fn release(
        &self,
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        start: u64,
        end: u64,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let matches = match &*guard {
            LeaseState::Leased {
                holder: cur,
                key: cur_key,
                start: cur_start,
                end: cur_end,
                ..
            }
            | LeaseState::Committed {
                holder: cur,
                key: cur_key,
                start: cur_start,
                end: cur_end,
                ..
            } => *cur == holder && cur_key == &key && *cur_start == start && *cur_end == end,
            LeaseState::Unleased => false,
        };
        if !matches {
            return false;
        }
        *guard = LeaseState::Unleased;
        self.state_tag.store(TAG_UNLEASED, Ordering::Release);
        true
    }

    /// #3041 P1-1 (§3, codex R2 Issue-1): HEARTBEAT renew. While the holder's
    /// terminal send future is in flight, the holder periodically calls this to
    /// extend the lease deadline so the (deliberately SHORT) deadline is a
    /// HOLDER-LIVENESS signal, not a hard cap on delivery duration. If the cell
    /// is `Leased` by EXACTLY `(holder, key)` (matched on holder + delivery lease
    /// key), its `deadline_ms` is overwritten with `new_deadline_ms`
    /// and `true` is returned. ANY other state — a different holder, a stale
    /// older key, a `Committed`/`Unleased` cell, or a cell already reclaimed and
    /// reacquired by someone else — is a no-op returning `false`. The range is
    /// intentionally NOT matched: a renew only ever needs to prove "this exact
    /// holder for this exact lease key is still alive", and the live holder's range is
    /// fixed for the lifetime of the lease anyway.
    ///
    /// Race-safety (why renew can never extend SOMEONE ELSE's lease): the match
    /// requires the recorded `holder` AND `key` to equal the caller's, both
    /// taken UNDER the same payload mutex as every other mutation. If the cell
    /// was reclaimed (→ `Unleased`) and reacquired by a replacement, the holder
    /// or key will differ and the renew no-ops. A late heartbeat tick that
    /// fires after the holder already committed sees `Committed` (not `Leased`)
    /// and no-ops. The ONLY successful renew extends the caller's OWN live lease.
    pub(in crate::services::discord) fn renew(
        &self,
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        new_deadline_ms: u64,
    ) -> bool {
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let LeaseState::Leased {
            holder: cur_holder,
            key: cur_key,
            deadline_ms,
            ..
        } = &mut *guard
        {
            if *cur_holder == holder && cur_key == &key {
                *deadline_ms = new_deadline_ms;
                return true;
            }
        }
        false
    }

    /// Deadline reclaim: if the lease is `Leased` and `now_ms >= deadline_ms`,
    /// force it back to `Unleased` regardless of holder (the holder is presumed
    /// dead/stuck). Returns `true` if a reclaim occurred. A `Committed` lease is
    /// never reclaimed by deadline — it awaits an explicit holder `release`.
    pub(in crate::services::discord) fn reclaim_if_expired(&self, now_ms: u64) -> bool {
        use std::sync::atomic::Ordering;
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let LeaseState::Leased { deadline_ms, .. } = &*guard {
            if now_ms >= *deadline_ms {
                *guard = LeaseState::Unleased;
                self.state_tag.store(TAG_UNLEASED, Ordering::Release);
                return true;
            }
        }
        false
    }
}

/// #3041 P1-1/P1-2: delivery-lease acquire deadline shared by BOTH the watcher
/// and the bridge terminal-delivery paths. The deadline is a HOLDER-LIVENESS
/// signal, NOT a hard cap on delivery duration — while a send future is in
/// flight the holder keeps the lease alive with a background HEARTBEAT that
/// `renew()`s the deadline every [`DELIVERY_LEASE_HEARTBEAT_MS`]. Because a LIVE
/// holder always re-extends within one interval, a long multi-chunk send (which
/// can exceed any FIXED deadline) is NEVER reclaimed mid-flight; a genuinely
/// DEAD holder stops renewing, so the lease expires and a replacement reclaims
/// it within ~one deadline. Picked as 3× the heartbeat (15s = 3 × 5s): one tick
/// can be skipped entirely and the lease still survives to the next, while
/// dead-holder recovery is ~15s. P1-2 reuses this so the WATCHER and the BRIDGE
/// share one deadline against the one per-channel cell — whoever holds it blocks
/// the other's acquire (cross-actor duplicate prevention).
pub(in crate::services::discord) const DELIVERY_LEASE_DEADLINE_MS: u64 = 15_000;

/// #3041 P1-1/P1-2: how often an in-flight holder renews its delivery lease.
/// Must be strictly less than (and a small fraction of)
/// [`DELIVERY_LEASE_DEADLINE_MS`] so a live holder always re-extends before
/// expiry even if one tick is delayed (the deadline is 3× this).
pub(in crate::services::discord) const DELIVERY_LEASE_HEARTBEAT_MS: u64 = 5_000;

/// #3041 P1-1 (§3, codex R2 Issue-1) / P1-2: RAII handle for the in-flight
/// delivery-lease heartbeat task, shared by the watcher and the bridge. The
/// holder spawns the heartbeat right after a successful `try_acquire` and
/// `stop()`s it BEFORE the inline commit (and the `Drop` impl aborts it on any
/// early return / panic), so the renew loop can NEVER outlive the send and race
/// the commit. While the holder task lives the heartbeat keeps the lease alive
/// (`renew`); if the holder TASK dies the spawned heartbeat is dropped/aborted
/// with it → the lease stops being renewed → it expires → a replacement reclaims
/// it. A heartbeat tick can only ever `renew` THIS holder's OWN still-`Leased`
/// lease (matched on holder+key), so a last tick that races `stop()`+commit
/// merely extends our own deadline, which the immediately-following commit then
/// flips to `Committed` — harmless.
pub(in crate::services::discord) struct DeliveryLeaseHeartbeat {
    handle: tokio::task::JoinHandle<()>,
}

impl DeliveryLeaseHeartbeat {
    /// Spawn a background task that renews `(holder, key)`'s lease on `cell`
    /// every [`DELIVERY_LEASE_HEARTBEAT_MS`], each time pushing the deadline to
    /// `lease_now_ms() + DELIVERY_LEASE_DEADLINE_MS`. The first tick fires AFTER
    /// one interval (the acquire already set a fresh deadline). The loop exits on
    /// its own as soon as a `renew` returns false (the lease is no longer ours —
    /// committed, released, or reclaimed), so it self-terminates even before an
    /// explicit `stop()`.
    pub(in crate::services::discord) fn spawn(
        cell: std::sync::Arc<DeliveryLeaseCell>,
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
    ) -> Self {
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(
                DELIVERY_LEASE_HEARTBEAT_MS,
            ));
            // Skip the immediate tick `interval` emits at t=0; the acquire just
            // set a fresh deadline, so the first renew is one interval later.
            interval.tick().await;
            loop {
                interval.tick().await;
                let renewed = cell.renew(
                    holder,
                    key.clone(),
                    lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS),
                );
                if !renewed {
                    // Lease is no longer ours (committed/released/reclaimed):
                    // nothing left to keep alive.
                    break;
                }
            }
        });
        Self { handle }
    }

    /// Stop the heartbeat. Idempotent. Called BEFORE the inline commit so the
    /// renew loop is guaranteed not to race the commit.
    pub(in crate::services::discord) fn stop(self) {
        self.handle.abort();
    }
}

impl Drop for DeliveryLeaseHeartbeat {
    fn drop(&mut self) {
        // Safety net: if the send path returns early / panics before an explicit
        // `stop()`, aborting on drop guarantees the heartbeat cannot outlive the
        // owning holder frame.
        self.handle.abort();
    }
}

#[derive(Clone)]
pub(super) struct ModelPickerPendingState {
    pub(super) owner_user_id: UserId,
    pub(super) target_channel_id: ChannelId,
    pub(super) pending_model: Option<String>,
    pub(super) updated_at: Instant,
}

/// Core state that requires atomic multi-field access (always locked together)
pub(super) struct CoreState {
    /// Per-channel sessions (each Discord channel can have its own Claude Code session)
    pub(in crate::services::discord) sessions: HashMap<ChannelId, DiscordSession>,
    /// Per-channel active meeting (one meeting per channel)
    active_meetings: HashMap<ChannelId, meeting::Meeting>,
}

const CHANNEL_ROSTER_MAX_USERS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UserRecord {
    pub(super) id: UserId,
    pub(super) name: String,
}

impl UserRecord {
    pub(super) fn new(id: UserId, name: &str) -> Self {
        let collapsed = name.split_whitespace().collect::<Vec<_>>().join(" ");
        let base = if collapsed.is_empty() {
            format!("user {}", id.get())
        } else {
            collapsed
        };
        let sanitized = base
            .chars()
            .map(|ch| match ch {
                '\r' | '\n' => ' ',
                _ => ch,
            })
            .collect::<String>();
        Self {
            id,
            name: sanitized.split_whitespace().collect::<Vec<_>>().join(" "),
        }
    }

    pub(super) fn label(&self) -> String {
        format!("{} (ID: {})", self.name, self.id.get())
    }
}

/// Shared state for the Discord bot — split into independently-lockable groups.
///
/// Phase 2-pre.3 of intake-node-routing: widened from `pub(super)` to
/// `pub(crate)` so the public worker entry point `execute_intake_turn_core`
/// can accept `&Arc<SharedData>` from a non-`services::discord` caller
/// (Phase 3 worker polling loop).
pub(crate) struct SharedData {
    /// Core state (sessions + request lifecycle) — requires atomic access
    pub(super) core: Mutex<CoreState>,
    /// Per-channel request lifecycle actor registry.
    mailboxes: ChannelMailboxRegistry,
    /// Bot settings — mostly reads, rare writes
    pub(super) settings: tokio::sync::RwLock<DiscordBotSettings>,
    /// Per-channel timestamps of the last Discord API call (for rate limiting)
    pub(super) api_timestamps: dashmap::DashMap<ChannelId, tokio::time::Instant>,
    /// Cached skill list: (name, description)
    pub(super) skills_cache: tokio::sync::RwLock<Vec<(String, String)>>,
    /// Active tmux output watchers for terminal→Discord relay.
    pub(super) tmux_watchers: TmuxWatcherRegistry,
    /// Per-channel relay coordination state. Unlike `tmux_watchers`, this
    /// entry is preserved across watcher-handle replacements so an outgoing
    /// watcher and an incoming watcher share the same emission-slot atomic
    /// and confirmed-offset watermark. See `TmuxRelayCoord`.
    pub(super) tmux_relay_coords: dashmap::DashMap<ChannelId, Arc<TmuxRelayCoord>>,
    /// #3038 cluster F — live-placeholder/status-panel state: cleanup tombstones,
    /// edit controller, live-event feed, and live-event/status-panel gates.
    /// Field docs live on `shared_state::PlaceholderState`; call sites use `shared.ui.*`.
    pub(in crate::services::discord) ui: PlaceholderState,
    /// #3038 cluster C — queued-placeholder handoff state (the
    /// `queued_placeholders` mapping, the `queue_exit_placeholder_clears`
    /// sidecar mirror, and the per-channel `queued_placeholders_persist_locks`).
    /// Field declarations, docs, and the round-5 P2 lock-span invariant live on
    /// `shared_state::QueuedPlaceholderState`; access stays via the inherent
    /// `SharedData` methods that own this cluster (no field-path changes at call
    /// sites).
    pub(in crate::services::discord) queued: QueuedPlaceholderState,
    /// #3082 part B — per-channel answer-flush barrier. Set while a multi-chunk
    /// final answer is being delivered (>1 Discord chunk) by
    /// `send_long_message_raw*`, so a queued-turn notice POST
    /// (`send_intake_placeholder`) does NOT interleave between the answer's
    /// chunks. The queued-card POST path waits on this gate with a BOUNDED
    /// timeout and proceeds regardless once it elapses, so a stuck/errored
    /// flush can never permanently suppress the queued card. The gate is
    /// cleared by an RAII guard on every exit path (success, error, panic) so
    /// it never strands set.
    pub(in crate::services::discord) answer_flush_barrier:
        Arc<answer_flush_barrier::AnswerFlushBarrier>,
    /// #3038 cluster E — restart-lifecycle state (the per-channel recovery
    /// markers and reconcile bookkeeping for the current boot, the
    /// restart/shutdown drain flags and restart generation, and the
    /// process-global active/finalizing/shutdown counters shared across all
    /// providers as injected `Arc` handles). Field declarations and docs live
    /// on `shared_state::RestartLifecycle`; call sites access the members via
    /// `shared.restart.<original field name>`.
    pub(super) restart: RestartLifecycle,
    /// EPIC #3016 — single-authority turn finalizer. The only code path that
    /// owns the four finalize side-effects (inflight clear, mailbox
    /// cancel_token release, `global_active` decrement, trailing terminal
    /// side-effects) as an atomic, exactly-once unit. Bridge/watcher terminals
    /// submit terminal events here instead of finalizing inline.
    pub(in crate::services::discord) turn_finalizer: Arc<turn_finalizer::TurnFinalizer>,
    /// #3479 Item 3 — dispatch intake/routing state: the intake dedup cache, the
    /// parent→dispatch-thread map, and the per-thread role/model override map.
    /// Field declarations and docs live on `shared_state::DispatchRoutingState`;
    /// call sites access the members via `shared.dispatch.<original field name>`.
    pub(super) dispatch: DispatchRoutingState,
    /// Runtime bridge from songbird receive events and STT transcript sidecars
    /// into live playback cuts, explicit-stop cancellation, and deferred prompts.
    pub(in crate::services::discord) voice_barge_in: Arc<voice_barge_in::VoiceBargeInRuntime>,
    /// Persistent mapping from Discord voice channel IDs to their text control
    /// channels so voice turns can enter the same Kanban/session routing path
    /// as typed Discord turns.
    pub(in crate::services::discord) voice_pairings: Arc<voice_routing::VoiceChannelPairingStore>,
    /// Set to true after Discord gateway ready event fires.
    pub(super) bot_connected: std::sync::atomic::AtomicBool,
    /// ISO 8601 timestamp of the last completed turn (for health reporting).
    pub(super) last_turn_at: std::sync::Mutex<Option<String>>,
    /// #3038 cluster D — session-scoped override / reset-pending state (the
    /// `model_overrides` map, the fast-mode / Codex-goals enablement sets, the
    /// per-cause `*_session_reset_pending` sets plus the aggregated
    /// `session_reset_pending` set, and the staged `model_picker_pending`
    /// selections). Field declarations and docs live on
    /// `shared_state::SessionOverrideState`; call sites access the members via
    /// `shared.overrides.<original field name>`.
    pub(super) overrides: SessionOverrideState,
    /// Per-channel last processed message ID — used for startup catch-up polling.
    pub(super) last_message_ids: dashmap::DashMap<ChannelId, u64>,
    /// Channels where catch-up stopped because the intervention queue was at
    /// capacity. Carries the pinned `after` checkpoint + bounded fetch-failure
    /// count for the next in-process pass, independent of live message
    /// checkpoints that may advance while the queued backlog drains.
    pub(super) catch_up_retry_pending: dashmap::DashMap<ChannelId, CatchUpRetryState>,
    /// Per-channel turn start time — used for metrics duration calculation.
    pub(super) turn_start_times: dashmap::DashMap<ChannelId, std::time::Instant>,
    /// Per-channel known speakers collected lazily from incoming messages.
    pub(super) channel_rosters: dashmap::DashMap<ChannelId, Vec<UserRecord>>,
    /// #3038 cluster G — cached Discord HTTP runtime state: the gateway
    /// serenity context plus the bot-token fallback for standby REST sends.
    /// Field docs live on `shared_state::RuntimeHttpCache`; call sites use
    /// `shared.http.*` for direct cache reads and keep
    /// `shared.serenity_http_or_token_fallback()` for the accessor.
    pub(in crate::services::discord) http: RuntimeHttpCache,
    /// SHA-256 hash of the bot token — used to namespace the pending-queue directory
    /// so that multiple bots sharing the same runtime root cannot steal each other's queues.
    pub(super) token_hash: String,
    /// #1332 round-3: the provider this `SharedData` was bootstrapped for.
    /// Persisted alongside `token_hash` so the `queued_placeholders` write-through
    /// helper can resolve `discord_queued_placeholders/<provider>/<token_hash>/`
    /// without a hot-path lock acquisition on `settings`.
    pub(super) provider: ProviderKind,
    /// HTTP API port for self-referencing requests (from config server.port).
    pub(super) api_port: u16,
    /// Shared PostgreSQL pool for PG-backed route and runtime helpers.
    pub(super) pg_pool: Option<sqlx::PgPool>,
    pub(in crate::services::discord) policy: PolicyRuntime,
    /// Weak reference to the process-wide health registry so turn handlers can
    /// reach dedicated Discord bot HTTP clients without creating an Arc cycle.
    pub(super) health_registry: std::sync::Weak<health::HealthRegistry>,
    /// Set of registered slash command names (populated at framework setup).
    /// Used by the router to distinguish known slash commands from arbitrary
    /// `/`-prefixed user text that should fall through to the AI provider.
    pub(super) known_slash_commands: tokio::sync::OnceCell<std::collections::HashSet<String>>,
    /// #2448: process-wide broadcast of explicit inflight-lifecycle signals.
    /// turn_bridge's `CompletionGuard` publishes `InflightSignal::Completed`
    /// on terminal drop so subscribers (currently `run_standby_relay`) can
    /// exit immediately instead of polling against a 15min wall-clock
    /// timeout. Capacity is intentionally generous so a brief listener
    /// hiccup yields `RecvError::Lagged` rather than dropped channels.
    pub(in crate::services::discord) inflight_signals:
        tokio::sync::broadcast::Sender<inflight::InflightSignal>,
    /// #4048 S3: canonical finalize-completion edge bus for idle-queue drain.
    /// The TurnFinalizer publishes after the mailbox token release point, so this
    /// is not coupled to visible status-panel/footer rendering.
    pub(in crate::services::discord) turn_completion_events:
        tokio::sync::broadcast::Sender<turn_completion_events::TurnCompletionEvent>,
    pub(in crate::services::discord) turn_view_reconciler: turn_view_reconciler::TurnViewReconciler,
    readopted_mailbox_ledger: readopted_mailbox_ledger::ReadoptedMailboxLedger, // #4370
}

impl SharedData {
    pub(super) fn has_runtime_storage(&self) -> bool {
        self.pg_pool.is_some()
    }

    fn mailbox(&self, channel_id: ChannelId) -> ChannelMailboxHandle {
        self.mailboxes.handle(channel_id)
    }

    /// #3293: non-creating mailbox lookup for probes — `mailbox()` mints a
    /// permanent registry entry for any channel id it is asked about.
    fn mailbox_peek(&self, channel_id: ChannelId) -> Option<ChannelMailboxHandle> {
        self.mailboxes.peek(channel_id)
    }

    fn health_registry(&self) -> Option<Arc<health::HealthRegistry>> {
        self.health_registry.upgrade()
    }

    /// #1031: snapshot every active mailbox for the idle-detector pass.
    /// Reduces the per-channel snapshot to the minimal fields the detector
    /// actually consumes — `cancel_token` / `recovery_started_at` /
    /// `turn_started_at` — so the detector module never imports the private
    /// mailbox types.
    pub(super) async fn mailbox_snapshots_for_idle_detector(
        &self,
    ) -> Vec<(ChannelId, bool, bool, Option<chrono::DateTime<chrono::Utc>>)> {
        self.mailboxes
            .snapshot_all()
            .await
            .into_iter()
            .map(|(channel_id, snapshot)| {
                (
                    channel_id,
                    snapshot.cancel_token.is_some(),
                    snapshot.recovery_started_at.is_some(),
                    snapshot.turn_started_at,
                )
            })
            .collect()
    }

    /// #1031: borrow the same `health_registry()` Arc the rest of the discord
    /// runtime uses. Exposed under a distinct name so the idle detector does
    /// not depend on the un-public method.
    pub(super) fn health_registry_for_idle_detector(&self) -> Option<Arc<health::HealthRegistry>> {
        self.health_registry()
    }

    /// Fetch the per-channel relay coordination state, creating a fresh one
    /// on first access. Returned Arc is shared across all watcher instances
    /// (outgoing and incoming) for the channel, so they coordinate relay
    /// emission without duplicate-sending the same tmux range.
    pub(super) fn tmux_relay_coord(&self, channel_id: ChannelId) -> Arc<TmuxRelayCoord> {
        self.tmux_relay_coords
            .entry(channel_id)
            .or_insert_with(|| Arc::new(TmuxRelayCoord::new(channel_id)))
            .clone()
    }

    /// #3041 P1-1: the LIVE per-channel delivery-lease cell, created on first
    /// access alongside the relay coord. The watcher acquires/commits through
    /// this to make terminal delivery + offset advance a single-holder unit
    /// (§5.2). The returned `Arc` is shared across all watcher instances for the
    /// channel so a replacement watcher sees the live holder and skips the
    /// duplicate send (B2).
    pub(in crate::services::discord) fn delivery_lease(
        &self,
        channel_id: ChannelId,
    ) -> Arc<DeliveryLeaseCell> {
        self.tmux_relay_coord(channel_id).delivery_lease.clone()
    }

    /// #3041 P1-1 (B3): reclaim any delivery lease whose acquire deadline has
    /// elapsed, force-returning a dead holder's cell to `Unleased` so a later
    /// legitimate acquire can win. Identity-agnostic (deadline-only) by design —
    /// a `Committed` lease is never reclaimed here (it awaits an explicit holder
    /// release). Driven from the finalizer's reconcile tick. Returns the number
    /// of cells reclaimed (for observability/tests).
    pub(in crate::services::discord) fn reclaim_expired_delivery_leases(
        &self,
        now_ms: u64,
    ) -> usize {
        let mut reclaimed = 0usize;
        for coord in self.tmux_relay_coords.iter() {
            if coord.value().delivery_lease.reclaim_if_expired(now_ms) {
                reclaimed += 1;
            }
        }
        reclaimed
    }

    /// #3017 single output-offset authority for the relay-dedup paths — a
    /// read-only snapshot of the authoritative committed relayed offset.
    ///
    /// The per-channel `confirmed_end_offset` is the ONE authoritative "JSONL
    /// byte offset (exclusive) past which output has already been relayed to
    /// Discord". The single committer is the tmux watcher (the primary relay)
    /// via `advance_watcher_confirmed_end`. The inflight-less wake /
    /// idle-background / monitor relay paths (idle-JSONL relay, session-bound
    /// sink) CONSULT this BEFORE relaying so a byte-range the watcher already
    /// delivered is relayed EXACTLY ONCE regardless of which actor observes it
    /// first (the E-13 dedup invariant). They do NOT claim it themselves —
    /// claiming on a secondary path could suppress the primary watcher's own
    /// delivery on a failed forward and drop the response. It is the
    /// cross-actor generalization of the watcher's process-local
    /// `last_relayed_offset`.
    pub(super) fn committed_relay_offset(&self, channel_id: ChannelId) -> u64 {
        self.tmux_relay_coord(channel_id)
            .confirmed_end_offset
            .load(Ordering::Acquire)
    }

    /// #4181: `true` while some watcher is actively emitting a relay for this
    /// channel — the `relay_slot` holds the in-progress emission's
    /// `data_start_offset` (non-zero). A single relay POST can be in-flight for
    /// longer than the stall grace under extreme rate-limiting, freezing the
    /// committed offset without the turn being stalled. Redrive consults this so
    /// it does not re-drive an already-in-flight emission (a duplicate, not a
    /// loss).
    pub(super) fn relay_emission_in_flight(&self, channel_id: ChannelId) -> bool {
        self.tmux_relay_coord(channel_id)
            .relay_slot
            .load(Ordering::Acquire)
            != 0
    }

    /// Record a recovery/reattach watcher spawn and purge the channel footer so the
    /// dead prior generation's task/subagent slots don't linger as zombies (#3436, #964).
    pub(super) fn record_tmux_watcher_reconnect(&self, channel_id: ChannelId) {
        self.tmux_relay_coord(channel_id)
            .reconnect_count
            .fetch_add(1, Ordering::AcqRel);
        self.ui.placeholder_live_events.clear_channel(channel_id);
    }

    pub(super) fn record_channel_speaker(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        user_name: &str,
        is_dm: bool,
    ) {
        let record = UserRecord::new(user_id, user_name);
        if is_dm {
            self.channel_rosters.insert(channel_id, vec![record]);
            return;
        }

        match self.channel_rosters.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                let roster = entry.get_mut();
                if let Some(existing) = roster.iter_mut().find(|user| user.id == user_id) {
                    existing.name = record.name;
                } else if roster.len() < CHANNEL_ROSTER_MAX_USERS {
                    roster.push(record);
                }
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(vec![record]);
            }
        }
    }

    pub(super) fn channel_roster(
        &self,
        channel_id: ChannelId,
        fallback_user_id: UserId,
        fallback_user_name: &str,
    ) -> Vec<UserRecord> {
        self.channel_rosters
            .get(&channel_id)
            .map(|entry| entry.clone())
            .filter(|users| !users.is_empty())
            .unwrap_or_else(|| vec![UserRecord::new(fallback_user_id, fallback_user_name)])
    }

    // #3038 S1: the queued-placeholder cluster methods
    // (queued_placeholders_persist_lock, insert/remove_queued_placeholder*,
    // queued_placeholder_still_owned, add/remove_pending_queue_exit_placeholder_clear*,
    // pending_queue_exit_placeholder_clears) moved verbatim to the
    // `shared_state` sibling module alongside `QueuedPlaceholderState`.
}

#[cfg(test)]
pub(super) fn make_shared_data_for_tests() -> Arc<SharedData> {
    make_shared_data_for_tests_with_storage(None)
}

#[cfg(test)]
pub(super) fn make_shared_data_for_tests_with_storage(
    pg_pool: Option<sqlx::PgPool>,
) -> Arc<SharedData> {
    Arc::new(SharedData {
        core: tokio::sync::Mutex::new(CoreState {
            sessions: std::collections::HashMap::new(),
            active_meetings: std::collections::HashMap::new(),
        }),
        mailboxes: ChannelMailboxRegistry::default(),
        settings: tokio::sync::RwLock::new(DiscordBotSettings::default()),
        api_timestamps: dashmap::DashMap::new(),
        skills_cache: tokio::sync::RwLock::new(Vec::new()),
        tmux_watchers: TmuxWatcherRegistry::new(),
        tmux_relay_coords: dashmap::DashMap::new(),
        ui: PlaceholderState {
            placeholder_cleanup: Arc::new(
                placeholder_cleanup::PlaceholderCleanupRegistry::default(),
            ),
            placeholder_controller: Arc::new(
                placeholder_controller::PlaceholderController::default(),
            ),
            placeholder_live_events: Arc::new(
                placeholder_live_events::PlaceholderLiveEvents::default(),
            ),
            placeholder_live_events_enabled: false,
            status_panel_v2_enabled: false,
            two_message_panel_enabled: false,
        },
        queued: QueuedPlaceholderState {
            queued_placeholders: dashmap::DashMap::new(),
            queue_exit_placeholder_clears: dashmap::DashMap::new(),
            queued_placeholders_persist_locks: dashmap::DashMap::new(),
        },
        answer_flush_barrier: Arc::new(answer_flush_barrier::AnswerFlushBarrier::default()),
        // #3038 S3: wrapped at the first-member position (evaluation-order
        // preserved — the three members hoisted above the spawn calls are
        // side-effect-free constructors; see run_bot_build_shared_data).
        restart: RestartLifecycle {
            recovering_channels: dashmap::DashMap::new(),
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            intake_worker_lifecycle: Default::default(),
            finalizing_turns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            current_generation: 0,
            restart_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reconcile_done: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            deferred_hook_backlog: std::sync::atomic::AtomicUsize::new(0),
            deferred_hook_channels: dashmap::DashMap::new(),
            recovery_started_at: std::time::Instant::now(),
            recovery_duration_ms: std::sync::atomic::AtomicU64::new(0),
            global_active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            global_finalizing: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutdown_remaining: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutdown_counted: std::sync::atomic::AtomicBool::new(false),
        },
        turn_finalizer: turn_finalizer::TurnFinalizer::spawn(),
        dispatch: DispatchRoutingState {
            intake_dedup: dashmap::DashMap::new(),
            thread_parents: dashmap::DashMap::new(),
            role_overrides: dashmap::DashMap::new(),
        },
        voice_barge_in: Arc::new(voice_barge_in::VoiceBargeInRuntime::disabled()),
        voice_pairings: Arc::new(voice_routing::VoiceChannelPairingStore::load_default()),
        bot_connected: std::sync::atomic::AtomicBool::new(false),
        last_turn_at: std::sync::Mutex::new(None),
        overrides: SessionOverrideState {
            model_overrides: dashmap::DashMap::new(),
            fast_mode_channels: dashmap::DashSet::new(),
            fast_mode_session_reset_pending: dashmap::DashSet::new(),
            codex_goals_channels: dashmap::DashSet::new(),
            codex_goals_session_reset_pending: dashmap::DashSet::new(),
            node_overrides: dashmap::DashMap::new(),
            model_session_reset_pending: dashmap::DashSet::new(),
            session_reset_pending: dashmap::DashSet::new(),
            model_picker_pending: dashmap::DashMap::new(),
        },
        last_message_ids: dashmap::DashMap::new(),
        catch_up_retry_pending: dashmap::DashMap::new(),
        turn_start_times: dashmap::DashMap::new(),
        channel_rosters: dashmap::DashMap::new(),
        http: RuntimeHttpCache {
            cached_serenity_ctx: tokio::sync::OnceCell::new(),
            cached_bot_token: tokio::sync::OnceCell::new(),
        },
        token_hash: "test-token-hash".to_string(),
        provider: ProviderKind::Claude,
        api_port: 9,
        pg_pool,
        policy: PolicyRuntime { engine: None },
        health_registry: std::sync::Weak::new(),
        known_slash_commands: tokio::sync::OnceCell::new(),
        inflight_signals: tokio::sync::broadcast::channel(256).0,
        turn_completion_events: turn_completion_events::turn_completion_event_bus(),
        turn_view_reconciler: turn_view_reconciler::TurnViewReconciler::default(),
        readopted_mailbox_ledger: readopted_mailbox_ledger::ReadoptedMailboxLedger::default(),
    })
}

fn queue_persistence_context(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> QueuePersistenceContext {
    QueuePersistenceContext::new(
        provider,
        &shared.token_hash,
        shared
            .dispatch
            .role_overrides
            .get(&channel_id)
            .map(|override_id| override_id.value().get()),
    )
}

async fn mailbox_snapshot(shared: &SharedData, channel_id: ChannelId) -> ChannelMailboxSnapshot {
    match shared.mailbox_peek(channel_id) {
        Some(handle) => handle.snapshot().await,
        None => ChannelMailboxSnapshot::default(),
    }
}

async fn mailbox_cancel_token(
    shared: &SharedData,
    channel_id: ChannelId,
) -> Option<Arc<CancelToken>> {
    shared.mailbox(channel_id).cancel_token().await
}

async fn mailbox_cancel_active_turn(
    shared: &SharedData,
    channel_id: ChannelId,
) -> CancelActiveTurnResult {
    mailbox_cancel_active_turn_with_reason(shared, channel_id, "mailbox_cancel_active_turn").await
}

async fn mailbox_cancel_active_turn_with_reason(
    shared: &SharedData,
    channel_id: ChannelId,
    reason: &str,
) -> CancelActiveTurnResult {
    let tmux_session_name = shared
        .tmux_watchers
        .channel_binding(&channel_id)
        .map(|binding| binding.tmux_session_name)
        .or_else(|| infer_inflight_tmux_session_for_channel(channel_id));
    // Issue #2374 — the reason-write and the `cancelled` flip are now
    // performed atomically by the mailbox actor (see
    // `ChannelMailboxMsg::CancelActiveTurnWithReason`). The previous
    // design (introduced by PR #2373 for issue #2335) read the active
    // token from the actor, wrote `cancel_source` on it from the caller
    // task, then sent the actor a `CancelActiveTurn`. That kept the
    // ordering correct for a single canceller but allowed two concurrent
    // cancellers to both fetch the same token, race on
    // `set_cancel_source`, and lose one of the reasons. Owning the write
    // inside the actor serializes both transitions per channel and
    // removes the small ordering window.
    let result = shared
        .mailbox(channel_id)
        .cancel_active_turn_with_reason(reason.to_string())
        .await;
    #[cfg(unix)]
    if result.token.is_some() {
        // #2549: in-memory publish remains immediate, while the matching PG
        // mirror is awaited before the cancel path returns so a quick
        // dcserver restart cannot drop the durable tombstone.
        tmux::record_recent_turn_stop(channel_id, tmux_session_name.as_deref(), reason).await;
    }
    result
}

/// #2374 Codex round-1 fix (HIGH-1) — identity-guarded variant for the
/// voice handoff cancel path. Cancels the active turn on `channel_id`
/// ONLY when its `user_message_id` matches `handoff_message_id`. An
/// unguarded cancel from the tombstone retry path could otherwise kill
/// an unrelated turn that happened to start on the same target channel
/// after the original handoff turn finalized.
///
/// Recording the tombstone is the caller's responsibility (see
/// [`record_voice_handoff_cancel_tombstone`]) so a tombstone can be
/// written even when no active turn is present (HIGH-2 fix).
pub(crate) async fn mailbox_cancel_active_turn_if_handoff_user_message_with_reason(
    shared: &SharedData,
    channel_id: ChannelId,
    handoff_message_id: MessageId,
    reason: &str,
) -> CancelActiveTurnResult {
    let tmux_session_name = shared
        .tmux_watchers
        .channel_binding(&channel_id)
        .map(|binding| binding.tmux_session_name)
        .or_else(|| infer_inflight_tmux_session_for_channel(channel_id));
    let result = shared
        .mailbox(channel_id)
        .cancel_active_turn_if_user_message_with_reason(handoff_message_id, reason.to_string())
        .await;
    #[cfg(unix)]
    if result.token.is_some() {
        tmux::record_recent_turn_stop(channel_id, tmux_session_name.as_deref(), reason).await;
    }
    result
}

/// #2374 Codex round-1 fix (HIGH-2) — record the voice handoff
/// cancel-tombstone unconditionally when a cancel is observed for a
/// known `handoff_message_id`. The original PR only recorded a
/// tombstone when the target mailbox cancel returned a live token,
/// missing the cases where the target turn had not yet started (intake
/// race) or had already finalized. In both cases a later retry for the
/// same handoff must still observe the tombstone and discard itself.
pub(crate) fn record_voice_handoff_cancel_tombstone(
    handoff_message_id: MessageId,
    reason: impl Into<String>,
) {
    crate::voice::cancel_tombstone::global_store().record(handoff_message_id, reason);
}

async fn mailbox_cancel_active_turn_if_current_with_reason(
    shared: &SharedData,
    channel_id: ChannelId,
    expected_token: Arc<CancelToken>,
    reason: &str,
) -> CancelActiveTurnResult {
    // Issue #2374 — actor-owned reason write. The `if_current` guard is
    // preserved so a stale caller cannot cancel a freshly-restarted turn
    // that happens to live on the same channel. The same
    // already-cancelled protection PR #2373 added to the caller-side
    // write is now enforced inside the actor handler itself.
    let tmux_session_name = shared
        .tmux_watchers
        .channel_binding(&channel_id)
        .map(|binding| binding.tmux_session_name)
        .or_else(|| infer_inflight_tmux_session_for_channel(channel_id));
    let result = shared
        .mailbox(channel_id)
        .cancel_active_turn_if_current_with_reason(expected_token, reason.to_string())
        .await;
    #[cfg(unix)]
    if result.token.is_some() {
        tmux::record_recent_turn_stop(channel_id, tmux_session_name.as_deref(), reason).await;
    }
    result
}

fn infer_inflight_tmux_session_for_channel(channel_id: ChannelId) -> Option<String> {
    [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::Gemini,
        ProviderKind::Qwen,
    ]
    .into_iter()
    .find_map(|provider| {
        inflight::load_inflight_state(&provider, channel_id.get())
            .and_then(|state| state.tmux_session_name)
    })
}

#[cfg(unix)]
pub(crate) async fn record_turn_stop_tombstone(
    channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    reason: &str,
) {
    tmux::record_recent_turn_stop(channel_id, tmux_session_name, reason).await;
}

#[cfg(not(unix))]
pub(crate) async fn record_turn_stop_tombstone(
    _channel_id: ChannelId,
    _tmux_session_name: Option<&str>,
    _reason: &str,
) {
}

async fn mailbox_has_active_turn(shared: &SharedData, channel_id: ChannelId) -> bool {
    shared.mailbox(channel_id).has_active_turn().await
}

/// #3167 — true only when a *real* (non-background) active turn holds the
/// slot. The external-input dequeue uses this instead of
/// `mailbox_has_active_turn` so a continuously-cycling background turn
/// (monitor relay / self-paced TUI loop) does not starve a queued user
/// intervention.
async fn mailbox_has_blocking_active_turn(shared: &SharedData, channel_id: ChannelId) -> bool {
    shared.mailbox(channel_id).has_blocking_active_turn().await
}

fn cleanup_retry_inflight_blocks_idle_kickoff(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> bool {
    let Some(state) = inflight::load_inflight_state(provider, channel_id.get()) else {
        return false;
    };
    let Some(current_msg_id) = inflight::opt_message_id(state.current_msg_id) else {
        return false;
    };

    shared
        .ui
        .placeholder_cleanup
        .terminal_cleanup_retry_pending(provider, channel_id, current_msg_id)
}

fn idle_queue_snapshot_has_pending_or_marker_backlog(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> bool {
    if !snapshot.intervention_queue.is_empty() {
        return true;
    }
    let Some((marker, _)) =
        load_channel_pending_dispatch_marker(provider, &shared.token_hash, channel_id)
    else {
        return false;
    };
    if snapshot
        .recently_valve_cleared_dispatch
        .is_some_and(|(cleared_id, cleared_at)| {
            cleared_id == marker.message_id
                && cleared_at.elapsed() < VALVE_CLEARED_DISPATCH_MARKER_GRACE
        })
    {
        return false;
    }
    match (
        snapshot.pending_user_dispatch,
        snapshot.pending_user_dispatch_since,
    ) {
        (Some(reserved_id), Some(reserved_at)) => {
            reserved_id == marker.message_id
                && !snapshot.pending_user_dispatch_lease_held_by_caller
                && reserved_at.elapsed() >= PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER
        }
        (Some(_), None) => false,
        (None, _) => true,
    }
}

fn idle_queue_snapshot_has_kickable_backlog(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> bool {
    // #3167 — a background turn (monitor relay / self-paced TUI loop) holds the
    // slot but must NOT block a queued user intervention. Only a real
    // (non-background) active turn blocks the kickoff. The previous
    // `cancel_token.is_none()` / `active_request_owner.is_none()` /
    // `active_user_message_id.is_none()` checks all proxied "no active turn at
    // all" and so starved a queued user message behind a continuously-cycling
    // background turn.
    let blocked_by_real_turn =
        snapshot.cancel_token.is_some() && !snapshot.active_turn_kind.is_background();
    !blocked_by_real_turn
        && snapshot.recovery_started_at.is_none()
        && idle_queue_snapshot_has_pending_or_marker_backlog(shared, provider, channel_id, snapshot)
        && !cleanup_retry_inflight_blocks_idle_kickoff(shared, provider, channel_id)
        // #3154: while a deferred synthetic turn-start is pending for this
        // channel, the per-channel worker is waiting for the prior turn to
        // finalize before claiming. Do NOT kick normal queued work in the
        // meantime — that would re-introduce the very turn-interleave this fix
        // serializes away.
        && !tui_direct_pending_start::pending_synthetic_start_blocks_idle_kickoff(
            provider.as_str(),
            channel_id.get(),
        )
}

async fn idle_queue_channel_has_kickable_backlog(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> bool {
    idle_queue_snapshot_has_kickable_backlog(shared, provider, channel_id, snapshot)
}

async fn mailbox_try_start_turn(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_message_id: MessageId,
) -> bool {
    mailbox_try_start_turn_kinded(
        shared,
        channel_id,
        cancel_token,
        request_owner,
        user_message_id,
        ActiveTurnKind::UserOrAgent,
    )
    .await
}

/// #3167 — kinded variant of [`mailbox_try_start_turn`]. The monitor auto-turn
/// and the self-paced TUI loop claim the slot as `ActiveTurnKind::Background`
/// so a queued external USER intervention is not perpetually deferred behind
/// the continuously-cycling background turn.
async fn mailbox_try_start_turn_kinded(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_message_id: MessageId,
    turn_kind: ActiveTurnKind,
) -> bool {
    queue_io::mailbox_try_start_turn_kinded_with_feedback(
        shared,
        channel_id,
        cancel_token,
        request_owner,
        user_message_id,
        turn_kind,
    )
    .await
}

// #3034: dormant production restore path (wraps `mailbox.restore_active_turn`,
// itself `#[allow(dead_code)]` in turn_orchestrator). Kept as the wired-but-not-
// yet-dispatched rehydrate seam; do not delete without removing the method too.
#[allow(dead_code)]
async fn mailbox_restore_active_turn(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_message_id: MessageId,
) {
    shared
        .mailbox(channel_id)
        .restore_active_turn(cancel_token, request_owner, user_message_id)
        .await;
}

async fn mailbox_recovery_kickoff(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    // `None` when the recovery turn has no anchored user message
    // (user_msg_id == 0, e.g. a TUI-direct turn).
    user_message_id: Option<MessageId>,
) -> RecoveryKickoffResult {
    // #2443 — reset the per-channel `recovery_done` latch BEFORE recovery
    // starts; a stale "done" flag would let `watchers/lifecycle.rs` graduate
    // its skip early and race the ongoing recovery. Idempotent and cheap.
    shared.mailboxes.recovery_done(channel_id).reset();
    // #3297 r3 — tombstone refusal ⇒ retry on a fresh registered actor.
    let result = shared
        .mailboxes
        .recovery_kickoff_with_closed_retry(
            channel_id,
            cancel_token,
            request_owner,
            user_message_id,
        )
        .await;
    if result.activated_turn {
        increment_global_active(shared, "recovery_kickoff");
    }
    result
}

fn ensure_cancel_token_bound_from_inflight_state(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    cancel_token: &Arc<CancelToken>,
    reason: &str,
) -> bool {
    let Some(tmux_session_name) = state.tmux_session_name.as_deref() else {
        tracing::error!(
            "cancel token rebind failed: provider={} channel_id={} reason={} error=inflight_missing_tmux_session",
            provider.as_str(),
            state.channel_id,
            reason
        );
        return false;
    };

    turn_bridge::bind_cancel_token_tmux_runtime(provider, cancel_token, tmux_session_name, reason);
    true
}

fn ensure_cancel_token_bound_from_inflight(
    provider: &ProviderKind,
    channel_id: ChannelId,
    cancel_token: &Arc<CancelToken>,
    reason: &str,
) -> bool {
    if turn_bridge::cancel_token_has_tmux_session(cancel_token) {
        return true;
    }

    let Some(state) = inflight::load_inflight_state(provider, channel_id.get()) else {
        tracing::error!(
            "cancel token rebind failed: provider={} channel_id={} reason={} error=inflight_not_found",
            provider.as_str(),
            channel_id.get(),
            reason
        );
        return false;
    };

    ensure_cancel_token_bound_from_inflight_state(provider, &state, cancel_token, reason)
}

async fn mailbox_clear_recovery_marker(shared: &SharedData, channel_id: ChannelId) {
    shared.mailbox(channel_id).clear_recovery_marker().await;
    // #2443 — graduate the 60s `recovery_started_at < 60s` skip via a
    // deterministic wake-up. Every exit path of the recovery engine
    // (success / failure / cancel / stale-cleanup) funnels through this
    // helper, so a single `mark_done()` here covers all of them. Watchers
    // selecting on `recovery_done.wait()` proceed immediately; the 60s
    // timeout remains as a hook-miss safety net.
    shared.mailboxes.recovery_done(channel_id).mark_done();
}

async fn mailbox_enqueue_intervention(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    intervention: Intervention,
) -> MailboxEnqueueOutcome {
    // #3297 r3 — tombstone refusal ⇒ retry on a fresh registered actor
    // instead of orphaning the queue on a purged one.
    let result = shared
        .mailboxes
        .enqueue_with_closed_retry(
            channel_id,
            intervention,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    if let Some(error) = result.persistence_error.as_ref() {
        tracing::error!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            error = %error,
            "mailbox enqueue failed durable pending-queue persistence"
        );
    }
    if result.enqueued && result.persistence_error.is_none() {
        queue_io::schedule_post_enqueue_idle_queue_kick(
            shared.clone(),
            provider.clone(),
            channel_id,
        );
    }
    MailboxEnqueueOutcome {
        enqueued: result.enqueued,
        merged: result.merged,
        refusal_reason: result.refusal_reason,
        persistence_error: result.persistence_error,
    }
}

pub(in crate::services::discord) fn queue_exit_feedback_emoji(kind: QueueExitKind) -> char {
    match kind {
        QueueExitKind::Cancelled => '🚫',
        QueueExitKind::Expired => '⌛',
        // #4260 dual r1: `Overflow` inherits the pre-split ⏏ feedback.
        QueueExitKind::Superseded | QueueExitKind::Overflow => '⏏',
    }
}

/// codex review P2 (#1332 follow-up): replacement card body for a queued
/// placeholder when its intervention exits the queue without ever being
/// dispatched. Replaces the `📬 메시지 대기 중` promise with a concise
/// terminal notice, so the user is not left wondering when the turn will
/// run.
fn queue_exit_card_body(kind: QueueExitKind) -> &'static str {
    match kind {
        QueueExitKind::Cancelled => "🚫 **큐에서 제거됨** — 사용자 취소로 처리되지 않습니다.",
        QueueExitKind::Expired => "⌛ **큐에서 제거됨** — 대기 시간 초과로 처리되지 않습니다.",
        // #4260 dual r1: the pre-split ⏏ text carries over to `Overflow`.
        QueueExitKind::Superseded | QueueExitKind::Overflow => {
            "⏏ **큐에서 제거됨** — 후속 메시지로 대체되어 처리되지 않습니다."
        }
    }
}

#[cfg(test)]
mod queue_exit_feedback_reconciler_tests {
    use super::*;

    struct ScopedRuntimeRoot {
        _lock: std::sync::MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    impl Drop for ScopedRuntimeRoot {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                    None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
                }
            }
        }
    }

    #[must_use]
    fn scoped_runtime_root() -> ScopedRuntimeRoot {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        let temp = tempfile::tempdir().expect("create temp runtime dir for feedback test");
        unsafe {
            std::env::set_var(
                "AGENTDESK_ROOT_DIR",
                temp.path().to_str().expect("temp path must be valid utf-8"),
            );
        }
        ScopedRuntimeRoot {
            _lock: lock,
            _temp: temp,
            prev,
        }
    }

    fn queue_exit_intervention(message_id: MessageId) -> Intervention {
        Intervention {
            author_id: UserId::new(7),
            author_is_bot: false,
            message_id,
            queued_generation: 91,
            source_message_ids: vec![message_id],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "queued text".to_string(),
            mode: InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    #[tokio::test]
    async fn apply_queue_exit_feedback_adds_feedback_reaction_through_reconciler() {
        let _root = scoped_runtime_root();
        let shared = make_shared_data_for_tests();
        let _ = shared
            .http
            .cached_bot_token
            .set("Bot test-token".to_string());
        let channel_id = ChannelId::new(100_000_000_000_231);
        let cases = [
            (
                MessageId::new(100_000_000_000_232),
                QueueExitKind::Cancelled,
            ),
            (MessageId::new(100_000_000_000_233), QueueExitKind::Expired),
            (
                MessageId::new(100_000_000_000_234),
                QueueExitKind::Superseded,
            ),
            (MessageId::new(100_000_000_000_235), QueueExitKind::Overflow),
        ];

        for (message_id, kind) in cases {
            let event = QueueExitEvent {
                intervention: queue_exit_intervention(message_id),
                kind,
            };
            apply_queue_exit_feedback(&shared, channel_id, &[event]).await;
            let emoji = queue_exit_feedback_emoji(kind);

            assert!(
                shared.turn_view_reconciler.ops().iter().any(|op| {
                    op.target.channel_id == channel_id
                        && op.target.message_id == message_id
                        && op.add
                        && op.emoji == emoji
                }),
                "{emoji} queue-exit feedback must route through the reconciler"
            );
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QueueExitVisibleCard {
    user_msg_id: MessageId,
    placeholder_msg_id: MessageId,
    kind: QueueExitKind,
}

/// codex review P2 (#1332 follow-up): drain the in-memory `queued_placeholders`
/// + `placeholder_controller` rows for every queue-exit event and return the
/// visible Discord card ids the caller should edit/delete. Split out from
/// `apply_queue_exit_feedback` so the bookkeeping is testable without a
/// serenity HTTP client.
async fn queue_exit_drain_queued_placeholders(
    shared: &SharedData,
    channel_id: ChannelId,
    queue_exit_events: &[&QueueExitEvent],
) -> Vec<QueueExitVisibleCard> {
    // codex review round-4 P2 + round-5 P2: hold the channel's persistence
    // mutex (async since round-5 so `.await`-spanning callers serialize too)
    // across the whole batch drain + snapshot write, or a concurrent
    // `insert_queued_placeholder` could win the disk write with a pre-drain
    // snapshot that resurrects already-exited entries on restart.
    let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
    let _persist_guard = persist_lock.lock().await;
    let mut visible_cards_to_clear: Vec<QueueExitVisibleCard> = Vec::new();
    let mut mutated = false;
    for event in queue_exit_events {
        for message_id in &event.intervention.source_message_ids {
            if let Some((_, placeholder_msg_id)) = shared
                .queued
                .queued_placeholders
                .remove(&(channel_id, *message_id))
            {
                shared
                    .ui
                    .placeholder_controller
                    .detach_by_message(channel_id, placeholder_msg_id);
                visible_cards_to_clear.push(QueueExitVisibleCard {
                    user_msg_id: *message_id,
                    placeholder_msg_id,
                    kind: event.kind,
                });
                mutated = true;
            }
        }
    }
    // codex review round-3 P2: persist the write-through after the batch
    // drain so a restart sees the same state as memory (queue-exit cleanup
    // must clear the on-disk snapshot, otherwise restart would resurrect
    // mappings for cancelled/expired/superseded interventions).
    if mutated {
        queued_placeholders_store::persist_channel_from_map(
            &shared.queued.queued_placeholders,
            &shared.provider,
            &shared.token_hash,
            channel_id,
        );
    }
    visible_cards_to_clear
}

async fn apply_queue_exit_feedback(
    shared: &SharedData,
    channel_id: ChannelId,
    queue_exit_events: &[QueueExitEvent],
) {
    let queue_exit_events: Vec<&QueueExitEvent> = queue_exit_events
        .iter()
        .filter(|event| event.intervention.author_id.get() > 1)
        .collect();
    if queue_exit_events.is_empty() {
        return;
    }

    // #1332: drop stale `📬 메시지 대기 중` placeholder mappings + controller
    // entries up front (Queued rows are exempt from the standard eviction
    // sweep) so a later dispatch never wires a new turn to a cancelled/expired
    // intervention's placeholder; the bookkeeping runs even without a cached
    // serenity ctx so a missing ctx never misroutes the next turn. codex
    // review P2 (#1332 follow-up): also collect the visible card ids to
    // rewrite/delete once a ctx exists (best-effort; drain rationale on the
    // `queue_exit_drain_queued_placeholders` doc).
    let visible_cards_to_clear =
        queue_exit_drain_queued_placeholders(shared, channel_id, &queue_exit_events).await;

    // #4260 dual r1: dead-letter + notice for capacity-`Overflow` evicts only
    // (benign Superseded producers pass through untouched). Fire-and-forget —
    // detached spawns inside — and sited before the Http guard below because
    // neither call needs an Http source nor may stall feedback on a pool
    // acquire.
    queue_overflow_dlq::record_queue_overflow_dead_letters(shared, channel_id, &queue_exit_events);
    queue_overflow_dlq::maybe_notify_orphan_queue_overflow(
        shared,
        channel_id,
        &queue_exit_events,
        &visible_cards_to_clear,
    );

    // Phase 5.2 of intake-node-routing (issue #2009): use gateway-or-token
    // fallback so cluster-standby workers can still rewrite queue-exit
    // placeholder cards via REST. Falling back to the deferred-cleanup
    // path is still correct for genuinely-no-token startup races.
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        shared
            .add_pending_queue_exit_placeholder_clears(channel_id, &visible_cards_to_clear)
            .await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⚠ QUEUE-FEEDBACK: skipped {} queue exit reaction(s) in channel {} (no Http source); queued {} visible card(s) for ready-time cleanup",
            queue_exit_events.len(),
            channel_id,
            visible_cards_to_clear.len(),
        );
        return;
    };

    // codex review P2: rewrite each leftover queued card to a brief
    // exit-state notice so the user is not left looking at a `📬` promise
    // for a turn that will never run. Edit-on-failure falls back to delete
    // — either way the stale `📬 메시지 대기 중` text is removed. We use
    // the shared Discord HTTP boundary instead of the placeholder controller
    // because the controller entry was just detached (and the public
    // `transition` API only renders terminal monitor-handoff cards).
    for card in &visible_cards_to_clear {
        let body = queue_exit_card_body(card.kind);
        let edit_result =
            http::edit_channel_message(&http, channel_id, card.placeholder_msg_id, &body).await;
        if edit_result.is_err() {
            let _ = channel_id
                .delete_message(&http, card.placeholder_msg_id)
                .await;
        }
    }

    queue_marker::drain_queue_exit_markers(shared, &http, channel_id, &queue_exit_events).await;
    for event in queue_exit_events {
        let message_id = event.intervention.message_id;
        let emoji = queue_exit_feedback_emoji(event.kind);
        queue_marker::note_exit_feedback_added(shared, &http, channel_id, message_id, emoji).await;
    }
}

struct QueueExitPendingPlaceholderDeleter {
    http: Arc<serenity::Http>,
}

impl runtime_bootstrap::StalePlaceholderDeleter for QueueExitPendingPlaceholderDeleter {
    fn delete<'a>(
        &'a self,
        channel_id: ChannelId,
        placeholder_msg_id: MessageId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            channel_id
                .delete_message(&self.http, placeholder_msg_id)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }
}

pub(in crate::services::discord) async fn drain_pending_queue_exit_placeholder_clears(
    shared: &SharedData,
) {
    // Phase 5.2 of intake-node-routing (issue #2009): use gateway-or-token
    // fallback so the deferred drain that fires on `bot_connected` /
    // `runtime_bootstrap` can still run on standby workers.
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return;
    };
    let deleter = QueueExitPendingPlaceholderDeleter { http };
    drain_pending_queue_exit_placeholder_clears_with(shared, &deleter).await;
}

pub(in crate::services::discord) async fn drain_pending_queue_exit_placeholder_clears_with(
    shared: &SharedData,
    deleter: &dyn runtime_bootstrap::StalePlaceholderDeleter,
) -> (usize, usize) {
    let pending = shared.pending_queue_exit_placeholder_clears();
    if pending.is_empty() {
        return (0, 0);
    }

    let mut deleted_by_channel: HashMap<ChannelId, Vec<(MessageId, MessageId)>> = HashMap::new();
    let mut deleted = 0usize;
    let mut failed = 0usize;
    for (channel_id, user_msg_id, placeholder_msg_id) in pending {
        match deleter.delete(channel_id, placeholder_msg_id).await {
            Ok(_) => {
                deleted += 1;
                deleted_by_channel
                    .entry(channel_id)
                    .or_default()
                    .push((user_msg_id, placeholder_msg_id));
                tracing::debug!(
                    channel_id = channel_id.get(),
                    user_msg_id = user_msg_id.get(),
                    placeholder_msg_id = placeholder_msg_id.get(),
                    "queue_exit_pending_clear: deleted queued placeholder card",
                );
            }
            Err(error) => {
                failed += 1;
                tracing::warn!(
                    channel_id = channel_id.get(),
                    user_msg_id = user_msg_id.get(),
                    placeholder_msg_id = placeholder_msg_id.get(),
                    "queue_exit_pending_clear: failed to delete queued placeholder card ({error}); keeping pending",
                );
            }
        }
    }

    for (channel_id, cards) in deleted_by_channel {
        shared
            .remove_pending_queue_exit_placeholder_clears(channel_id, &cards)
            .await;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🧹 QUEUE-EXIT: deleted {deleted} pending queued placeholder card(s) after ctx ready (failed {failed})",
    );
    (deleted, failed)
}

pub(in crate::services::discord) async fn enqueue_internal_followup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reply_message_id: MessageId,
    text: impl Into<String>,
    reason: &'static str,
) -> bool {
    let outcome = mailbox_enqueue_intervention(
        shared,
        provider,
        channel_id,
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: reply_message_id,
            queued_generation: shared.restart.current_generation,
            source_message_ids: vec![reply_message_id],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.into(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        },
    )
    .await;

    if let Some(error) = outcome.persistence_error.as_ref() {
        tracing::error!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            reason,
            error = %error,
            "internal followup enqueue failed durable pending-queue persistence"
        );
        return false;
    }

    if outcome.enqueued {
        schedule_deferred_idle_queue_kickoff(shared.clone(), provider.clone(), channel_id, reason);
    }

    outcome.enqueued
}

async fn mailbox_has_pending_soft_queue(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> HasPendingSoftQueueResult {
    let result = shared
        .mailbox(channel_id)
        .has_pending_soft_queue(queue_persistence_context(shared, provider, channel_id))
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    result
}

fn maybe_schedule_catch_up_retry_after_queue_drain(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    queue_len_after: usize,
) -> bool {
    if !should_trigger_catch_up_retry(queue_len_after) {
        return false;
    }

    // Phase 5.2 of intake-node-routing (issue #2009): catch-up retry runs
    // on whatever node hosts the channel; on standby workers it falls back
    // to a token-built REST `Arc<Http>` so retries still fire even
    // without a gateway runtime.
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return false;
    };

    let Some(retry_state) =
        take_catch_up_retry_checkpoint_after_queue_drain(shared, channel_id, queue_len_after)
    else {
        return false;
    };

    let shared = Arc::clone(shared);
    let provider = provider.clone();
    task_supervisor::spawn_observed("catch_up_retry_after_queue_drain", async move {
        let retry_checkpoints = HashMap::from([(channel_id, retry_state)]);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔁 catch-up: retrying channel {} after queue drained to {} item(s)",
            channel_id,
            queue_len_after
        );
        catch_up_missed_messages_for_retry(&http, &shared, &provider, &retry_checkpoints).await;
        schedule_deferred_idle_queue_kickoff(
            shared,
            provider,
            channel_id,
            "catch-up retry after queue drain",
        );
    });
    true
}

async fn mailbox_take_next_soft_intervention(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> MailboxTakeNextSoftOutcome {
    loop {
        let result: TakeNextSoftResult = shared
            .mailbox(channel_id)
            .take_next_soft(queue_persistence_context(shared, provider, channel_id))
            .await;
        let queue_len_after = result.queue_len_after;
        apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
        if let Some(error) = result.persistence_error {
            tracing::error!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                error = %error,
                "mailbox dequeue failed durable pending-queue persistence"
            );
            return MailboxTakeNextSoftOutcome {
                intervention: None,
                dispatch_lease: None,
                has_more: result.has_more,
                persistence_error: Some(error),
            };
        }
        maybe_schedule_catch_up_retry_after_queue_drain(
            shared,
            provider,
            channel_id,
            queue_len_after,
        );
        let Some(intervention) = result.intervention else {
            return MailboxTakeNextSoftOutcome {
                intervention: None,
                dispatch_lease: None,
                has_more: result.has_more,
                persistence_error: None,
            };
        };

        if let Some(stale) =
            stale_dispatch_turn_for_queued_intervention(shared.pg_pool.as_ref(), &intervention)
                .await
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⏭ DISPATCH-GUARD: dropped queued terminal dispatch {} in channel {} (status={})",
                stale.dispatch_id,
                channel_id,
                stale.status
            );
            let queue_exit_events = [QueueExitEvent {
                intervention: intervention.clone(),
                kind: stale.queue_exit_kind,
            }];
            apply_queue_exit_feedback(shared, channel_id, &queue_exit_events).await;
            mailbox_abandon_pending_dispatch(shared, provider, channel_id, intervention.message_id)
                .await;
            drop(result.dispatch_lease);
            continue;
        }

        return MailboxTakeNextSoftOutcome {
            intervention: Some(intervention),
            dispatch_lease: result.dispatch_lease,
            has_more: result.has_more,
            persistence_error: None,
        };
    }
}

#[cfg(test)]
mod queued_dequeue_dispatch_guard_wiring_tests {
    #[test]
    fn dequeue_uses_preservation_aware_stale_dispatch_guard() {
        let source = include_str!("mod.rs");
        let function_start = source
            .find("async fn mailbox_take_next_soft_intervention(")
            .expect("mailbox dequeue helper exists");
        let function_end = source[function_start..]
            .find("\nasync fn idle_queue_take_next_soft_if_ready(")
            .map(|offset| function_start + offset)
            .expect("mailbox dequeue helper has a stable following function");
        let function_body = &source[function_start..function_end];
        let queued_guard = format!(
            "{}{}",
            "stale_dispatch_turn_for_queued_",
            "intervention(shared.pg_pool.as_ref(), &intervention)"
        );
        let text_guard = format!(
            "{}{}",
            "stale_dispatch_turn_for_", "text(shared.pg_pool.as_ref(), &intervention.text)"
        );

        assert!(
            function_body.contains(&queued_guard),
            "dequeue must retain the preservation-aware queued dispatch guard"
        );
        assert!(
            !function_body.contains(&text_guard),
            "dequeue must not bypass queued preservation with the raw text guard"
        );
    }
}

async fn idle_queue_take_next_soft_if_ready(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> MailboxTakeNextSoftOutcome {
    // #3167 — only a real (non-background) active turn blocks the dequeue. The
    // cleanup-retry guard remains a correctness guard; the hosted-TUI busy-pane
    // re-scrape gate was removed in #4048 S3 because finalize completion is now
    // the drain authority.
    if mailbox_has_blocking_active_turn(shared, channel_id).await
        || cleanup_retry_inflight_blocks_idle_kickoff(shared, provider, channel_id)
    {
        return MailboxTakeNextSoftOutcome::default();
    }

    // #3167 — the blocking gate above passed, but a *background* turn (monitor
    // relay / self-paced TUI loop) may still hold the slot. Dequeuing now would
    // race the background turn for the single active-turn slot. Instead, cancel
    // the background turn's token and re-kick: the background turn's own
    // identity-guarded finalizer releases the slot, and the immediate re-kick
    // retries against a now-idle mailbox and dequeues the user intervention.
    //
    // RACE-SAFETY: the user turn only ever claims the slot through the normal
    // actor-serialized `mailbox_try_start_turn` AFTER the background turn fully
    // releases — never two concurrent real turns. Cancelling the monitor/loop
    // turn loses no terminal output: the watcher relays output independently of
    // the mailbox slot.
    //
    // #3167 BLOCKER-1 — the kind check and the cancel are performed as a SINGLE
    // atomic, kind-guarded actor step. The previous code read
    // `active_turn_kind()` and THEN sent a separate unguarded
    // `cancel_active_turn_with_reason()`; between the two the background turn
    // could finalize and a real user turn start, and the unguarded cancel would
    // abort the freshly-started real turn. `cancel_active_background_turn_if_current`
    // returns `true` ONLY when it performs a NEW cancel. We re-kick exactly once
    // on that NEW cancel to drain the superseded slot once the background
    // finalizer releases it.
    //
    // CRITICAL (no hot-loop): when the background token is ALREADY cancelling,
    // the call returns `false` (no-op) — NOT `true`. If it returned `true` here,
    // every re-kick would re-observe the same already-cancelled slot (finalizer
    // not done yet), reply `true`, and spawn yet another immediate re-kick: a
    // livelock. On `false` we spawn NO new re-kick and fall through to the
    // normal dequeue/await path below; the deferred-retry cadence (queue_io.rs,
    // ~2s) waits for the finalizer to release the slot. `false` also covers an
    // idle slot (fall through to dequeue) or a real turn holding it (the
    // blocking gate above would already have returned).
    if shared
        .mailbox(channel_id)
        .cancel_active_background_turn_if_current()
        .await
    {
        schedule_deferred_idle_queue_kickoff_immediate(
            shared.clone(),
            provider.clone(),
            channel_id,
            "background_supersede_drain",
        );
        return MailboxTakeNextSoftOutcome::default();
    }

    mailbox_take_next_soft_intervention(shared, provider, channel_id).await
}

async fn mailbox_requeue_intervention_front(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    intervention: Intervention,
) {
    let result: RequeueInterventionResult = shared
        .mailbox(channel_id)
        .requeue_front(
            intervention,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    if let Some(error) = result.persistence_error.as_ref() {
        tracing::warn!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            error = %error,
            "mailbox requeue-front failed durable pending-queue persistence; pending dispatch marker remains the durable backstop"
        );
    }
}

async fn mailbox_abandon_pending_dispatch(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_message_id: MessageId,
) {
    shared
        .mailbox(channel_id)
        .abandon_pending_dispatch(
            user_message_id,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await;
}

async fn mailbox_clear_pending_dispatch_reservation(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_message_id: MessageId,
) {
    shared
        .mailbox(channel_id)
        .clear_pending_dispatch_reservation(
            user_message_id,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await;
}

/// Re-queue the inflight message that a claude TUI follow-up could not submit
/// because the pane was busy at submit time. The follow-up busy-timeout is
/// PRE-submit (the prompt was never delivered), so retrying cannot double-send.
/// Enqueues to the BACK of the channel mailbox — matching
/// `enqueue_busy_tui_followup_for_retry` — so the message is retried after the
/// in-flight turn frees the pane rather than hot-looping. No-op for anchorless
/// (recovery) turns or empty text.
pub(in crate::services::discord) async fn mailbox_requeue_inflight_for_followup_retry(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: &InflightTurnState,
) -> MailboxEnqueueOutcome {
    let user_msg_id = inflight_state.user_msg_id;
    if user_msg_id == 0 || inflight_state.user_text.trim().is_empty() {
        return MailboxEnqueueOutcome::default();
    }
    let message_id = MessageId::new(user_msg_id);
    // FIX #6 (Codex P2): rebuild the retry Intervention from the persisted
    // follow-up requeue context instead of hardcoding empty values, so a
    // PRE-submit busy-timeout requeue preserves the originating turn's reply
    // context, attachments, and voice metadata. Legacy rows (pre-v9) default
    // these to None/empty/false, matching the previous behavior exactly.
    // #4247 FIX 2: rebuild the mark from the persisted `followup_preserve_on_cancel`
    // decision; leaving it unmarked would regress a genuine-human-marked instruction
    // to origin/main's drop-on-cancel behavior at the downstream preservation guards.
    let queued_generation = shared.restart.current_generation;
    let source_message_queued_generations = if inflight_state.followup_preserve_on_cancel {
        vec![
            crate::services::turn_orchestrator::SourceMessageQueuedGeneration::user_instruction(
                message_id,
                queued_generation,
            ),
        ]
    } else {
        Vec::new()
    };
    let intervention = Intervention {
        author_id: UserId::new(inflight_state.request_owner_user_id),
        author_is_bot: false,
        message_id,
        queued_generation,
        source_message_ids: vec![message_id],
        source_message_queued_generations,
        source_text_segments: Vec::new(),
        text: inflight_state.user_text.clone(),
        mode: crate::services::turn_orchestrator::InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: inflight_state.followup_reply_context.clone(),
        has_reply_boundary: inflight_state.followup_has_reply_boundary,
        merge_consecutive: inflight_state.followup_merge_consecutive,
        pending_uploads: inflight_state.followup_pending_uploads.clone(),
        voice_announcement: inflight_state.followup_voice_announcement.clone(),
    };
    mailbox_enqueue_intervention(shared, provider, channel_id, intervention).await
}

#[cfg(test)]
mod followup_retry_requeue_tests {
    use super::*;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    struct EnvGuard {
        previous: Option<String>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.as_deref() {
                Some(value) => unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, value) },
                None => unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) },
            }
        }
    }

    fn followup_inflight(
        channel_id: ChannelId,
        user_msg_id: MessageId,
        preserve_on_cancel: bool,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            channel_id.get(),
            Some("adk-cc".to_string()),
            42,
            user_msg_id.get(),
            user_msg_id.get() + 1,
            "please continue".to_string(),
            Some("session-3752".to_string()),
            Some("AgentDesk-claude-3752".to_string()),
            Some("/tmp/agentdesk-3752.jsonl".to_string()),
            None,
            0,
        );
        state.set_followup_requeue_context(
            Some("reply context".to_string()),
            true,
            false,
            vec!["attachment-a".to_string(), "attachment-b".to_string()],
            None,
            preserve_on_cancel,
        );
        state
    }

    #[test]
    fn pre_submit_requeue_preserves_context_and_returns_enqueue_outcome() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .expect("shared env lock poisoned");
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard {
            previous: std::env::var(AGENTDESK_ROOT_DIR_ENV).ok(),
        };
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path()) };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let shared = make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel_id = ChannelId::new(3_752_001);
            let user_msg_id = MessageId::new(3_752_101);
            let state = followup_inflight(channel_id, user_msg_id, false);

            let outcome =
                mailbox_requeue_inflight_for_followup_retry(&shared, &provider, channel_id, &state)
                    .await;

            assert!(outcome.enqueued);
            assert!(!outcome.merged);
            assert_eq!(outcome.refusal_reason, None);
            assert_eq!(outcome.persistence_error, None);

            let snapshot = mailbox_snapshot(&shared, channel_id).await;
            assert_eq!(snapshot.intervention_queue.len(), 1);
            let intervention = &snapshot.intervention_queue[0];
            assert_eq!(intervention.author_id, UserId::new(42));
            assert_eq!(intervention.message_id, user_msg_id);
            assert_eq!(intervention.source_message_ids, vec![user_msg_id]);
            assert_eq!(intervention.text, "please continue");
            assert_eq!(intervention.reply_context.as_deref(), Some("reply context"));
            assert!(intervention.has_reply_boundary);
            assert!(!intervention.merge_consecutive);
            assert_eq!(
                intervention.pending_uploads,
                vec!["attachment-a".to_string(), "attachment-b".to_string()]
            );
            assert!(intervention.voice_announcement.is_none());
        });
    }

    /// #4247 FIX 2 (mutation-provable): a PRE-submit busy-timeout requeue of a
    /// genuine-human turn whose `followup_preserve_on_cancel` decision was
    /// stored as `true` must reconstruct a MARKED `Intervention` (non-empty
    /// `source_message_queued_generations`, `preserve_on_cancel() == true`).
    /// Mutating `mailbox_requeue_inflight_for_followup_retry` back to the
    /// unconditional `Vec::new()` this fix replaced makes this assertion fail
    /// (not a compile error).
    #[test]
    fn pre_submit_requeue_of_marked_followup_reconstructs_marked_intervention() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .expect("shared env lock poisoned");
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard {
            previous: std::env::var(AGENTDESK_ROOT_DIR_ENV).ok(),
        };
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path()) };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let shared = make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel_id = ChannelId::new(3_752_003);
            let user_msg_id = MessageId::new(3_752_103);
            let state = followup_inflight(channel_id, user_msg_id, true);

            let outcome =
                mailbox_requeue_inflight_for_followup_retry(&shared, &provider, channel_id, &state)
                    .await;
            assert!(outcome.enqueued);

            let snapshot = mailbox_snapshot(&shared, channel_id).await;
            assert_eq!(snapshot.intervention_queue.len(), 1);
            let intervention = &snapshot.intervention_queue[0];
            assert!(
                !intervention.source_message_queued_generations.is_empty(),
                "a marked followup requeue must not reconstruct an unmarked (empty) intervention"
            );
            assert!(
                intervention.preserve_on_cancel(),
                "a marked followup requeue must carry preserve_on_cancel() == true"
            );
        });
    }

    #[test]
    fn pre_submit_requeue_reports_duplicate_refusal_outcome() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .expect("shared env lock poisoned");
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard {
            previous: std::env::var(AGENTDESK_ROOT_DIR_ENV).ok(),
        };
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path()) };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let shared = make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel_id = ChannelId::new(3_752_002);
            let user_msg_id = MessageId::new(3_752_102);
            let state = followup_inflight(channel_id, user_msg_id, false);

            let first =
                mailbox_requeue_inflight_for_followup_retry(&shared, &provider, channel_id, &state)
                    .await;
            let second =
                mailbox_requeue_inflight_for_followup_retry(&shared, &provider, channel_id, &state)
                    .await;

            assert!(first.enqueued);
            assert!(!second.enqueued);
            assert_eq!(
                second.refusal_reason,
                Some(
                    crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued
                )
            );
            let snapshot = mailbox_snapshot(&shared, channel_id).await;
            assert_eq!(
                snapshot.intervention_queue.len(),
                1,
                "duplicate pre-submit requeue must not create a second queued prompt"
            );
        });
    }
}

async fn mailbox_cancel_soft_intervention(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Option<Intervention> {
    let result: CancelQueuedMessageResult = shared
        .mailbox(channel_id)
        .cancel_queued_message(
            message_id,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    result.removed
}

async fn mailbox_clear_channel(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> ClearChannelResult {
    let result = shared
        .mailbox(channel_id)
        .clear(queue_persistence_context(shared, provider, channel_id))
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    // #2443 — `Clear` is the cancel/teardown exit path. Mark recovery_done so
    // a watcher that subscribed to the recovery latch is freed even when
    // recovery is aborted rather than completed.
    shared.mailboxes.recovery_done(channel_id).mark_done();
    result
}

/// #3864: in-actor merge of SIGTERM-restored disk queue items into the live
/// mailbox queue. Replaces the out-of-actor snapshot→build→`replace_queue`
/// read-modify-write the startup restore path used, which silently lost any
/// live reconcile-window `Enqueue` landing between its snapshot and its
/// replace. The actor reads, dedups, front-inserts and persists in one
/// serialized step (cf. `mailbox_hydrate_pending_queue_from_disk`, #1683).
async fn mailbox_merge_restored_queue_items(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    items: Vec<Intervention>,
) -> HydratePendingQueueResult {
    shared
        .mailbox(channel_id)
        .merge_restored_queue_items(
            items,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await
}

async fn mailbox_merge_restored_dispatch_marker(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    marker: Intervention,
    restored_override: Option<ChannelId>,
) -> HydratePendingQueueResult {
    shared
        .mailbox(channel_id)
        .merge_restored_dispatch_marker(
            marker,
            restored_override,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await
}

/// #1683: actor-local disk -> in-memory hydration helper. The mailbox
/// actor reads the queue file and merges it in one serialized message,
/// preventing stale out-of-actor disk snapshots from reintroducing an
/// item that another actor message already dequeued and removed from disk.
async fn mailbox_hydrate_pending_queue_from_disk(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> HydratePendingQueueResult {
    shared
        .mailbox(channel_id)
        .hydrate_pending_queue_from_disk(queue_persistence_context(shared, provider, channel_id))
        .await
}

async fn mailbox_restart_drain_all(
    shared: &SharedData,
    provider: &ProviderKind,
) -> crate::services::turn_orchestrator::RestartDrainAllResult {
    let result = shared
        .mailboxes
        .restart_drain_all(
            provider,
            &shared.token_hash,
            &shared.dispatch.role_overrides,
        )
        .await;
    for failure in &result.persistence_errors {
        tracing::error!(
            provider = provider.as_str(),
            channel_id = failure.channel_id.get(),
            error = %failure.error,
            "restart drain failed durable pending-queue persistence for mailbox"
        );
    }
    result
}

async fn mailbox_queue_snapshots(shared: &SharedData) -> HashMap<ChannelId, Vec<Intervention>> {
    shared
        .mailboxes
        .snapshot_all()
        .await
        .into_iter()
        .filter_map(|(channel_id, snapshot)| {
            if snapshot.intervention_queue.is_empty() {
                None
            } else {
                Some((channel_id, snapshot.intervention_queue))
            }
        })
        .collect()
}

/// Poise user data type
pub(super) struct Data {
    pub(super) shared: Arc<SharedData>,
    pub(super) token: String,
    pub(super) provider: ProviderKind,
    pub(super) voice_config: crate::voice::VoiceConfig,
    pub(super) voice_receiver: crate::voice::VoiceReceiver,
}

pub(super) fn mark_reconcile_complete(shared: &SharedData) {
    let duration_ms = shared.restart.recovery_started_at.elapsed().as_millis();
    let duration_ms = duration_ms.min(u64::MAX as u128) as u64;
    let _ = shared.restart.recovery_duration_ms.compare_exchange(
        0,
        duration_ms,
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Relaxed,
    );
    shared
        .restart
        .reconcile_done
        .store(true, std::sync::atomic::Ordering::Release);
}

pub(super) type Error = Box<dyn std::error::Error + Send + Sync>;
pub(super) type Context<'a> = poise::Context<'a, Data, Error>;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct IdleQueueKickoffChannelOutcome {
    pub(super) started: bool,
}

async fn kickoff_idle_queue_channel(
    ctx: &serenity::Context,
    shared: &Arc<SharedData>,
    token: &str,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> IdleQueueKickoffChannelOutcome {
    let settings_snapshot = shared.settings.read().await.clone();
    if let Err(reason) =
        validate_live_channel_routing(ctx, provider, &settings_snapshot, channel_id).await
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⚠ KICKOFF-GUARD: preserving queued item(s) for channel {} (reason={})",
            channel_id,
            reason
        );
        return IdleQueueKickoffChannelOutcome::default();
    }

    let fresh_snapshot = mailbox_snapshot(shared, channel_id).await;
    if !idle_queue_channel_has_kickable_backlog(shared, provider, channel_id, &fresh_snapshot).await
    {
        tracing::info!(
            channel_id = channel_id.get(),
            provider = provider.as_str(),
            "KICKOFF: skipped queued turn after fresh mailbox/TUI guard"
        );
        return IdleQueueKickoffChannelOutcome::default();
    }

    // #4270 A — pre-dequeue hosted-TUI readiness gate. A verifiably busy hosted
    // TUI defers the promotion BEFORE `take_next_soft` and BEFORE the queued-view
    // teardown below (turn-view started/⏳ flip + 📬 marker drain + merged-card
    // deletion), so a still-busy channel keeps its steady `📬 Queued` view with
    // zero churn. No-start here is fail-open: callers arm the slow (60s)
    // backstop on a no-start with backlog, and the watcher-idle re-drain
    // delivers the fast edge once the TUI reaches Idle.
    if router::hosted_tui_promote_readiness_blocked(shared, provider, channel_id).await {
        return IdleQueueKickoffChannelOutcome::default();
    }

    let take_next = idle_queue_take_next_soft_if_ready(shared, provider, channel_id).await;
    if let Some(error) = take_next.persistence_error.as_ref() {
        tracing::error!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            error = %error,
            "KICKOFF: preserving queued turn after pending-queue persistence failure"
        );
        return IdleQueueKickoffChannelOutcome::default();
    }
    let Some((intervention, has_more, dispatch_lease)) = take_next.into_intervention() else {
        return IdleQueueKickoffChannelOutcome::default();
    };

    let owner_name = if intervention.author_id.get() <= 1 {
        "system".to_string()
    } else {
        intervention
            .author_id
            .to_user(&ctx.http)
            .await
            .map(|u| u.name.clone())
            .unwrap_or_else(|_| format!("user-{}", intervention.author_id.get()))
    };

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🚀 KICKOFF: starting queued turn for channel {}",
        channel_id
    );

    let deps = router::IntakeDeps {
        http: &ctx.http,
        cache: Some(&ctx.cache),
        ctx_for_chained_dispatch: Some(ctx),
        shared,
        token,
    };
    let admitted = match router::admit_queued_intake(
        &deps,
        provider.clone(),
        channel_id,
        &intervention,
        intervention.author_id,
        owner_name,
        has_more,
        false,
        "intake_admission_pre_kickoff_defer",
    )
    .await
    {
        router::QueuedAdmissionDisposition::Admitted(admitted) => admitted,
        router::QueuedAdmissionDisposition::Deferred => {
            drop(dispatch_lease);
            return IdleQueueKickoffChannelOutcome::default();
        }
    };

    let source_message_generations = intervention.source_message_queued_generations();
    queue_marker::start_and_drain_kickoff_markers(
        shared,
        &ctx.http,
        channel_id,
        intervention.message_id,
        &source_message_generations,
    )
    .await;

    let drained_cards = gateway::drain_merged_queued_placeholders(
        shared,
        channel_id,
        intervention.message_id,
        &intervention.source_message_ids,
    )
    .await;
    for placeholder_msg_id in drained_cards {
        let _ = channel_id
            .delete_message(&ctx.http, placeholder_msg_id)
            .await;
    }

    let dispatch_result =
        router::finish_admitted_queued_intake(&deps, admitted, &intervention).await;
    match dispatch_result {
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}]   ⚠ KICKOFF: failed to start turn for channel {}: {e}",
                channel_id
            );
            mailbox_requeue_intervention_front(shared, provider, channel_id, intervention).await;
            drop(dispatch_lease);
            IdleQueueKickoffChannelOutcome { started: false }
        }
        Ok(()) => {
            mailbox_abandon_unclaimed_dispatch_after_success(
                shared,
                provider,
                channel_id,
                intervention.message_id,
            )
            .await;
            drop(dispatch_lease);
            IdleQueueKickoffChannelOutcome { started: true }
        }
    }
}

/// Kick off turns for channels that have queued interventions but no active
/// turn running. This bridges the gap where restored pending queues or
/// handoff injections sit idle because no turn-completion event triggers
/// the dequeue chain.
pub(super) async fn kickoff_idle_queues(
    ctx: &serenity::Context,
    shared: &Arc<SharedData>,
    token: &str,
    provider: &ProviderKind,
) -> usize {
    // Collect channels with queued items that are idle (no active turn). Dequeue only
    // after the routing guard passes so a rejected channel stays preserved on disk/in memory.
    let mailbox_snapshots = shared.mailboxes.snapshot_all().await;
    let mut channels_to_kick: Vec<ChannelId> = Vec::new();
    for (channel_id, snapshot) in mailbox_snapshots {
        if idle_queue_channel_has_kickable_backlog(shared, provider, channel_id, &snapshot).await {
            channels_to_kick.push(channel_id);
        }
    }

    if channels_to_kick.is_empty() {
        return 0;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🚀 KICKOFF: starting turns for {} idle channel(s) with queued messages",
        channels_to_kick.len()
    );

    let mut started_count = 0usize;
    for channel_id in channels_to_kick {
        let outcome = kickoff_idle_queue_channel(ctx, shared, token, provider, channel_id).await;
        if outcome.started {
            started_count += 1;
        }
    }
    started_count
}

/// Scan for provider-specific skills available to this bot.
pub(super) fn scan_skills(
    provider: &ProviderKind,
    project_path: Option<&str>,
) -> Vec<(String, String)> {
    if let Some(root) = crate::config::runtime_root() {
        let _ = crate::runtime_layout::sync_managed_skills(&root);
    }

    let mut skills: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    match provider {
        ProviderKind::Claude => {
            for (name, desc) in BUILTIN_SKILLS {
                seen.insert(name.to_string());
                skills.push((name.to_string(), desc.to_string()));
            }

            let dirs_to_scan = collect_provider_skill_roots(provider, project_path);

            for dir in dirs_to_scan {
                if !dir.is_dir() {
                    continue;
                }
                let Ok(entries) = fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.extension().map(|e| e == "md").unwrap_or(false) {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            let name = stem.to_string();
                            if seen.insert(name.clone()) {
                                let desc = fs::read_to_string(&path)
                                    .ok()
                                    .map(|content| extract_skill_description(&content))
                                    .unwrap_or_else(|| format!("Skill: {}", name));
                                skills.push((name, desc));
                            }
                        }
                    }
                }
            }
        }
        ProviderKind::Codex
        | ProviderKind::Gemini
        | ProviderKind::OpenCode
        | ProviderKind::Qwen => {
            scan_directory_skills(
                collect_provider_skill_roots(provider, project_path),
                &mut seen,
                &mut skills,
            );
        }
        ProviderKind::Unsupported(_) => {}
    }

    skills.sort_by(|a, b| a.0.cmp(&b.0));
    skills
}

/// Compute a lightweight fingerprint of skill directories: (file_count, max_mtime_epoch).
/// Used by the hot-reload poll to detect additions, modifications, and deletions.
fn skill_dir_fingerprint(provider: &ProviderKind) -> (usize, u64) {
    let mut count = 0usize;
    let mut max_mtime = 0u64;

    let mut dirs = collect_provider_skill_roots(provider, None);
    if provider_supports_directory_skills(provider) {
        if let Some(root) = crate::config::runtime_root() {
            dirs.push(crate::runtime_layout::managed_skills_root(&root));
        }
    }

    fn walk_mtime(dir: &Path, count: &mut usize, max_mtime: &mut u64) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_mtime(&path, count, max_mtime);
            } else if path.extension().map(|e| e == "md").unwrap_or(false) {
                *count += 1;
                if let Ok(meta) = fs::metadata(&path) {
                    if let Ok(mt) = meta.modified() {
                        let epoch = mt
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        if epoch > *max_mtime {
                            *max_mtime = epoch;
                        }
                    }
                }
            }
        }
    }

    for dir in &dirs {
        walk_mtime(dir, &mut count, &mut max_mtime);
    }

    (count, max_mtime)
}

/// Like `skill_dir_fingerprint` but also includes project-level skill directories.
fn skill_dir_fingerprint_with_projects(
    provider: &ProviderKind,
    project_paths: &[String],
) -> (usize, u64) {
    let (mut count, mut max_mtime) = skill_dir_fingerprint(provider);

    fn walk_mtime(dir: &Path, count: &mut usize, max_mtime: &mut u64) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_mtime(&path, count, max_mtime);
            } else if path.extension().map(|e| e == "md").unwrap_or(false) {
                *count += 1;
                if let Ok(meta) = fs::metadata(&path) {
                    if let Ok(mt) = meta.modified() {
                        let epoch = mt
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        if epoch > *max_mtime {
                            *max_mtime = epoch;
                        }
                    }
                }
            }
        }
    }

    for path in project_paths {
        let Some(proj_dir) = provider_project_skill_dir(provider, path) else {
            continue;
        };
        if proj_dir.is_dir() {
            walk_mtime(&proj_dir, &mut count, &mut max_mtime);
        }
    }

    (count, max_mtime)
}

fn provider_supports_directory_skills(provider: &ProviderKind) -> bool {
    matches!(
        provider,
        ProviderKind::Claude
            | ProviderKind::Codex
            | ProviderKind::Gemini
            | ProviderKind::OpenCode
            | ProviderKind::Qwen
    )
}

fn provider_home_skill_dir(provider: &ProviderKind, home: &Path) -> Option<std::path::PathBuf> {
    match provider {
        ProviderKind::Claude => Some(home.join(".claude").join("commands")),
        ProviderKind::Codex => Some(home.join(".codex").join("skills")),
        ProviderKind::Gemini => Some(home.join(".gemini").join("skills")),
        ProviderKind::OpenCode => Some(home.join(".opencode").join("skills")),
        ProviderKind::Qwen => Some(home.join(".qwen").join("skills")),
        ProviderKind::Unsupported(_) => None,
    }
}

fn provider_project_skill_dir(
    provider: &ProviderKind,
    project_path: &str,
) -> Option<std::path::PathBuf> {
    let project_root = Path::new(project_path);
    match provider {
        ProviderKind::Claude => Some(project_root.join(".claude").join("commands")),
        ProviderKind::Codex => Some(project_root.join(".codex").join("skills")),
        ProviderKind::Gemini => Some(project_root.join(".gemini").join("skills")),
        ProviderKind::OpenCode => Some(project_root.join(".opencode").join("skills")),
        ProviderKind::Qwen => Some(project_root.join(".qwen").join("skills")),
        ProviderKind::Unsupported(_) => None,
    }
}

fn collect_provider_skill_roots(
    provider: &ProviderKind,
    project_path: Option<&str>,
) -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        if let Some(path) = provider_home_skill_dir(provider, &home) {
            roots.push(path);
        }
    }
    if let Some(project_path) = project_path {
        if let Some(path) = provider_project_skill_dir(provider, project_path) {
            roots.push(path);
        }
    }
    roots
}

fn scan_directory_skills(
    roots: Vec<std::path::PathBuf>,
    seen: &mut std::collections::HashSet<String>,
    skills: &mut Vec<(String, String)>,
) {
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            collect_directory_skill(&path, seen, skills);

            if !path.is_dir() {
                continue;
            }
            let Ok(nested) = fs::read_dir(&path) else {
                continue;
            };
            for child in nested.filter_map(|e| e.ok()) {
                collect_directory_skill(&child.path(), seen, skills);
            }
        }
    }
}

fn collect_directory_skill(
    path: &Path,
    seen: &mut std::collections::HashSet<String>,
    skills: &mut Vec<(String, String)>,
) {
    let Some(skill_path) = resolve_codex_skill_file(path) else {
        return;
    };
    let Some(name) = skill_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
    else {
        return;
    };
    let name = name.to_string();
    if !seen.insert(name.clone()) {
        return;
    }
    let desc = fs::read_to_string(&skill_path)
        .ok()
        .map(|content| extract_skill_description(&content))
        .unwrap_or_else(|| format!("Skill: {}", name));
    skills.push((name, desc));
}

fn resolve_codex_skill_file(path: &Path) -> Option<std::path::PathBuf> {
    if path.is_dir() {
        let skill_path = path.join("SKILL.md");
        if skill_path.is_file() {
            return Some(skill_path);
        }
    }
    None
}

use discord_io::{check_auth, check_owner, rate_limit_wait, try_handle_pending_dm_reply};

// ─── Event handler ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleSessionWatcherCleanup {
    ExpireSession,
    DeferToTmuxLiveness,
}

fn idle_session_watcher_cleanup(has_watcher: bool) -> IdleSessionWatcherCleanup {
    if has_watcher {
        IdleSessionWatcherCleanup::DeferToTmuxLiveness
    } else {
        IdleSessionWatcherCleanup::ExpireSession
    }
}

/// Periodically clean up idle sessions and their associated data.
/// Called from handle_event; uses a static Mutex to track the last cleanup time.
async fn maybe_cleanup_sessions(shared: &Arc<SharedData>) {
    use std::sync::OnceLock;
    static LAST_CLEANUP: OnceLock<tokio::sync::Mutex<tokio::time::Instant>> = OnceLock::new();
    let last = LAST_CLEANUP.get_or_init(|| tokio::sync::Mutex::new(tokio::time::Instant::now()));
    let mut last_guard = last.lock().await;
    if last_guard.elapsed() < SESSION_CLEANUP_INTERVAL {
        return;
    }
    *last_guard = tokio::time::Instant::now();
    drop(last_guard);

    struct ExpiredSessionCleanup {
        channel_id: ChannelId,
        session_key: Option<String>,
    }

    let provider = shared.settings.read().await.provider.clone();
    let expired: Vec<ExpiredSessionCleanup> = {
        let data = shared.core.lock().await;
        let now = tokio::time::Instant::now();
        data.sessions
            .iter()
            .filter(|(channel_id, s)| {
                now.duration_since(s.last_active) > SESSION_MAX_IDLE
                    && matches!(
                        idle_session_watcher_cleanup(shared.tmux_watchers.contains_key(channel_id)),
                        IdleSessionWatcherCleanup::ExpireSession
                    )
            })
            .map(|(ch, s)| ExpiredSessionCleanup {
                channel_id: *ch,
                session_key: s.channel_name.as_ref().map(|name| {
                    let tmux_name = provider.build_tmux_session_name(name);
                    adk_session::build_namespaced_session_key(
                        &shared.token_hash,
                        &provider,
                        &tmux_name,
                    )
                }),
            })
            .collect()
    };
    if expired.is_empty() {
        return;
    }
    {
        let mut data = shared.core.lock().await;
        for expired_session in &expired {
            let ch = expired_session.channel_id;
            // Clean up worktree if session had one
            if let Some(session) = data.sessions.get(&ch) {
                if let Some(ref wt) = session.worktree {
                    cleanup_git_worktree(shared.pg_pool.as_ref(), wt);
                }
            }
            data.sessions.remove(&ch);
        }
    }
    // #3588: idle 정리는 in-memory/worktree 메모리 회수만 수행하고 provider
    // session(claude resume id)은 DB에 보존한다. 다음 턴에서
    // `fetch_provider_session_id`로 복원되어 `--resume`으로 transcript가 이어진다.
    // retry_context(session_retry_context_key) kv는 의도적으로 저장하지 않는다 —
    // 같은 키를 `take_session_retry_context`가 다음 턴에 무조건 take/주입하므로,
    // resume이 성공하는 idle 경로에서 저장하면 transcript 중복 + "새 세션 시작"
    // 레이블 오표시가 발생한다. (#3591에서 100턴 세션 리셋도 제거되어 reset 기반
    // 저장 경로는 없다; resume 실패 복구만 auto_retry_with_history가 별도로 저장한다.)
    // 명시적 세션 초기화는 idle recap의 `새 세션 시작` 버튼(idle_recap:clear)으로 한다.
    for expired_session in &expired {
        let cleared = mailbox_clear_channel(shared, &provider, expired_session.channel_id).await;
        if cleared.removed_token.is_some() {
            saturating_decrement_global_active(shared);
        }
        shared.api_timestamps.remove(&expired_session.channel_id);
    }
    // Record termination audit for cleaned-up sessions
    for expired_session in &expired {
        if let Some(session_key) = expired_session.session_key.as_deref() {
            let should_record =
                mark_session_disconnected_for_idle_cleanup(shared.pg_pool.as_ref(), session_key)
                    .await;
            if !should_record {
                continue;
            }

            crate::services::termination_audit::record_termination_with_handles(
                shared.pg_pool.as_ref(),
                session_key,
                None,
                "cleanup",
                "idle_session_expiry",
                Some("in-memory session expired due to idle timeout"),
                None,
                None,
                None,
            );
        }
    }
    tracing::info!("  [cleanup] Removed {} idle session(s)", expired.len());
}

async fn mark_session_disconnected_for_idle_cleanup(
    pg_pool: Option<&sqlx::PgPool>,
    session_key: &str,
) -> bool {
    let Some(pool) = pg_pool else {
        return false;
    };
    let prior_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
            .bind(session_key)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    let _ = sqlx::query(
        "UPDATE sessions
         SET status = 'disconnected', active_dispatch_id = NULL
         WHERE session_key = $1",
    )
    .bind(session_key)
    .execute(pool)
    .await;

    prior_status.as_deref() != Some("disconnected")
}

#[cfg(test)]
mod idle_cleanup_selector_tests {
    use super::mark_session_disconnected_for_idle_cleanup;

    struct TestPostgresDb {
        _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let lifecycle = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name =
                format!("agentdesk_idle_selector_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "idle selector tests",
            )
            .await
            .expect("create idle selector postgres test db");

            Self {
                _lifecycle: lifecycle,
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "idle selector tests",
            )
            .await
            .expect("apply idle selector postgres migrations")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "idle selector tests",
            )
            .await
            .expect("drop idle selector postgres test db");
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    #[tokio::test]
    async fn idle_cleanup_preserves_provider_selector_columns_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:idle-selector-preserve";

        sqlx::query(
            "INSERT INTO sessions
             (session_key, status, active_dispatch_id, claude_session_id,
              raw_provider_session_id, created_at)
             VALUES ($1, 'idle', 'dispatch-1841', 'claude-selector-1841',
                     'raw-selector-1841', NOW())",
        )
        .bind(session_key)
        .execute(&pool)
        .await
        .unwrap();

        assert!(mark_session_disconnected_for_idle_cleanup(Some(&pool), session_key).await);

        let row = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>)>(
            "SELECT status, active_dispatch_id, claude_session_id, raw_provider_session_id
             FROM sessions
             WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "disconnected");
        assert_eq!(row.1, None);
        assert_eq!(row.2.as_deref(), Some("claude-selector-1841"));
        assert_eq!(row.3.as_deref(), Some("raw-selector-1841"));

        pool.close().await;
        pg_db.drop().await;
    }
}

// ─── Slash commands (extracted to commands/ module) ──────────────────────────

// Command functions removed — see commands/ submodule.
// Remaining in mod.rs: detect_worktree_conflict, create_git_worktree, cleanup_git_worktree,
// send_file_to_channel, send_message_to_channel, send_message_to_user, auto_restore_session,
// bootstrap_thread_session, resolve_channel_category, and other non-command functions.

// ─── Text message → Claude AI ───────────────────────────────────────────────

/// Enrich role_map.json's byChannelName entries with channelId from byChannelId.
/// This enables reliable channel name → ID resolution without provider inference hacks.
fn enrich_role_map_with_channel_ids() {
    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return;
    };
    let path = root.join("config/role_map.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return;
    };

    let mut changed = false;

    // Build maps from byChannelId: channelId → (roleId, provider) and name→id lookup
    let by_id = json
        .get("byChannelId")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    // Pass 1: collect mappings (name → channelId) without mutating
    let mut mappings: Vec<(String, String)> = Vec::new();
    if let Some(by_name) = json.get("byChannelName").and_then(|v| v.as_object()) {
        // Collect already-assigned IDs to avoid duplicates
        let already_assigned: std::collections::HashSet<String> = by_name
            .iter()
            .filter_map(|(_, e)| {
                e.get("channelId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();

        for (name, entry) in by_name {
            if entry.get("channelId").is_some() {
                continue;
            }
            let role_id = entry.get("roleId").and_then(|v| v.as_str()).unwrap_or("");
            let entry_provider = entry.get("provider").and_then(|v| v.as_str());

            let candidates: Vec<(&String, &serde_json::Value)> = by_id
                .iter()
                .filter(|(_, e)| e.get("roleId").and_then(|v| v.as_str()) == Some(role_id))
                .collect();

            let ch_id = if candidates.len() == 1 {
                Some(candidates[0].0.clone())
            } else if candidates.len() > 1 {
                if let Some(p) = entry_provider {
                    // Explicit provider — exact match
                    candidates
                        .iter()
                        .find(|(_, e)| e.get("provider").and_then(|v| v.as_str()) == Some(p))
                        .map(|(id, _)| id.to_string())
                } else {
                    // No provider in byChannelName — match by expected provider type:
                    // Claude channels are the "primary" (cc suffix or no suffix)
                    // Codex channels are the "alt" (cdx suffix)
                    // This determines which byChannelId entry to pick.
                    let expected_provider = if name.ends_with("-cdx") {
                        "codex"
                    } else {
                        "claude"
                    };
                    candidates
                        .iter()
                        .find(|(_, e)| {
                            e.get("provider").and_then(|v| v.as_str()) == Some(expected_provider)
                        })
                        .map(|(id, _)| id.to_string())
                        .or_else(|| {
                            // Fallback: pick one not already assigned
                            candidates
                                .iter()
                                .find(|(id, _)| !already_assigned.contains(id.as_str()))
                                .map(|(id, _)| id.to_string())
                        })
                }
            } else {
                None
            };

            if let Some(id) = ch_id {
                mappings.push((name.clone(), id));
            }
        }
    }

    // Pass 2: apply mappings
    if let Some(by_name) = json
        .get_mut("byChannelName")
        .and_then(|v| v.as_object_mut())
    {
        for (name, ch_id) in &mappings {
            if let Some(entry) = by_name.get_mut(name) {
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert("channelId".to_string(), serde_json::json!(ch_id));
                    changed = true;
                }
            }
        }
    }

    if changed {
        if let Ok(pretty) = serde_json::to_string_pretty(&json) {
            let _ = runtime_store::atomic_write(&path, &pretty);
        }
    }
}

// #3167 — a queued external USER intervention must be kickable while a
// low-priority Background turn (monitor relay / self-paced TUI loop) holds the
// active-turn slot, and the dequeue gate must cancel the background token so
// the slot is released and the user turn can claim it.
#[cfg(test)]
mod idle_queue_background_supersede_tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
        }
    }

    fn user_intervention(message_id: u64, text: &str) -> Intervention {
        Intervention {
            author_id: UserId::new(7),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: crate::services::discord::runtime_store::load_generation(),
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    // SAFETY (await_holding_lock): the test-env Mutex is held across awaits to
    // serialize the process-global `AGENTDESK_ROOT_DIR` env mutation against
    // other tests in this crate. #3167 B3: this MUST be the single crate-wide
    // `test_support` lock shared with the turn_orchestrator env tests — a local
    // per-module Mutex would not serialize against them and would recreate the
    // parallel env-race. Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn queued_user_message_is_kickable_under_background_turn_and_cancels_token() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(3_167_900);

        // A background turn (monitor relay / TUI loop) holds the slot.
        let background_token = Arc::new(CancelToken::new());
        assert!(
            mailbox_try_start_turn_kinded(
                &shared,
                channel_id,
                background_token.clone(),
                UserId::new(1),
                MessageId::new(1),
                ActiveTurnKind::Background,
            )
            .await
        );

        // Queue an external user intervention behind it.
        shared
            .mailbox(channel_id)
            .replace_queue(
                vec![user_intervention(900, "user reply while loop runs")],
                queue_persistence_context(&shared, &provider, channel_id),
            )
            .await;

        // #3167 — the kickoff gate must treat the background turn as
        // non-blocking and report a kickable backlog.
        let snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert!(
            idle_queue_snapshot_has_kickable_backlog(&shared, &provider, channel_id, &snapshot),
            "#3167: a queued user message must be kickable under a background turn"
        );

        // The dequeue gate detects the background turn still holding the slot,
        // cancels its token, and defers (returns no intervention this pass).
        let outcome = idle_queue_take_next_soft_if_ready(&shared, &provider, channel_id).await;
        assert!(
            outcome.intervention.is_none(),
            "#3167: the supersede pass defers the dequeue until the background slot releases"
        );
        assert!(
            background_token.cancelled.load(Ordering::Relaxed),
            "#3167: the background token must be cancelled so the slot is released"
        );
    }

    #[test]
    fn stale_marker_only_dispatch_reservation_is_kickable() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let shared = make_shared_data_for_tests();
                let provider = ProviderKind::Claude;
                let channel_id = ChannelId::new(3_167_901);
                let head = user_intervention(901, "task died before claim");
                shared
                    .mailbox(channel_id)
                    .replace_queue(
                        vec![head],
                        queue_persistence_context(&shared, &provider, channel_id),
                    )
                    .await;
                let taken = shared
                    .mailbox(channel_id)
                    .take_next_soft(queue_persistence_context(&shared, &provider, channel_id))
                    .await;
                assert_eq!(
                    taken.intervention.as_ref().map(|item| item.message_id),
                    Some(MessageId::new(901))
                );
                drop(taken);
                shared
                    .mailbox(channel_id)
                    .age_pending_dispatch_for_test(
                        PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER
                            + std::time::Duration::from_secs(1),
                    )
                    .await;

                let snapshot = mailbox_snapshot(&shared, channel_id).await;

                assert!(
                    idle_queue_snapshot_has_kickable_backlog(
                        &shared, &provider, channel_id, &snapshot
                    ),
                    "stale marker-only reservations must wake the drain loop so TakeNextSoft can self-heal"
                );
            });
    }

    #[test]
    fn consume_without_claim_cleanup_clears_marker_and_unblocks_next_head() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let shared = make_shared_data_for_tests();
                let provider = ProviderKind::Claude;
                let channel_id = ChannelId::new(3_167_902);
                let consumed = user_intervention(902, "/goal done");
                let next = user_intervention(903, "next queued user reply");
                shared
                    .mailbox(channel_id)
                    .replace_queue(
                        vec![consumed.clone(), next.clone()],
                        queue_persistence_context(&shared, &provider, channel_id),
                    )
                    .await;
                let mut first = shared
                    .mailbox(channel_id)
                    .take_next_soft(queue_persistence_context(&shared, &provider, channel_id))
                    .await;
                let dispatch_lease = first
                    .dispatch_lease
                    .take()
                    .expect("dequeued consumed head should carry a lease");
                assert_eq!(
                    first.intervention.as_ref().map(|item| item.message_id),
                    Some(consumed.message_id)
                );
                assert_eq!(
                    load_channel_pending_dispatch_marker(&provider, &shared.token_hash, channel_id)
                        .map(|(marker, _)| marker.message_id),
                    Some(consumed.message_id)
                );

                mailbox_abandon_unclaimed_dispatch_after_success(
                    &shared,
                    &provider,
                    channel_id,
                    consumed.message_id,
                )
                .await;

                assert_eq!(
                    std::sync::Arc::strong_count(&dispatch_lease),
                    1,
                    "post-success abandon releases the actor-held lease"
                );
                let snapshot = mailbox_snapshot(&shared, channel_id).await;
                assert_eq!(snapshot.pending_user_dispatch, None);
                assert!(
                    load_channel_pending_dispatch_marker(&provider, &shared.token_hash, channel_id)
                        .is_none(),
                    "consumed-without-claim head marker must be cleared"
                );
                let hydrate = shared
                    .mailbox(channel_id)
                    .hydrate_pending_queue_from_disk(queue_persistence_context(
                        &shared, &provider, channel_id,
                    ))
                    .await;
                assert_eq!(hydrate.absorbed, 0);
                let second = shared
                    .mailbox(channel_id)
                    .take_next_soft(queue_persistence_context(&shared, &provider, channel_id))
                    .await;
                assert_eq!(
                    second.intervention.as_ref().map(|item| item.message_id),
                    Some(next.message_id),
                    "next queued head should dispatch instead of starving behind consumed head"
                );
                assert_eq!(
                    load_channel_pending_dispatch_marker(&provider, &shared.token_hash, channel_id)
                        .map(|(marker, _)| marker.message_id),
                    Some(next.message_id),
                    "next head receives the only remaining marker"
                );
            });
    }
}

// #3038 S0 — characterization tests for the queued-placeholder cluster (cluster
// C) method surface. These fix the observable behaviour (map round-trips,
// sidecar mirroring, ownership recheck branches, and per-channel persist-lock
// identity) BEFORE the field group is extracted into `QueuedPlaceholderState`,
// so the same tests passing unchanged after the move is the equivalence proof.
// The tests call only the method surface (never the fields directly).
#[cfg(test)]
mod queued_placeholder_cluster_characterization_tests {
    use super::*;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
        }
    }

    fn sidecar_path(
        root: &std::path::Path,
        subdir: &str,
        channel_id: ChannelId,
    ) -> std::path::PathBuf {
        // Mirrors queued_placeholders_store's
        // `<AGENTDESK_ROOT>/runtime/<subdir>/<provider>/<token_hash>/<channel>.json`
        // layout for the values `make_shared_data_for_tests` constructs
        // (`ProviderKind::Claude`, `token_hash == "test-token-hash"`).
        root.join("runtime")
            .join(subdir)
            .join("claude")
            .join("test-token-hash")
            .join(format!("{}.json", channel_id.get()))
    }

    // Build a current-thread tokio runtime so the async cluster methods can be
    // driven from a synchronous `#[test]`. Keeping the test fn synchronous means
    // the `test_support` env lock (a `std::sync::Mutex` guard) is never held
    // across an `.await` in this scope, so it needs no
    // `#[allow(clippy::await_holding_lock)]` and does not move the ratchet.
    fn test_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn insert_remove_queued_placeholder_round_trip_with_sidecar() {
        // #3167 B3: serialize the process-global `AGENTDESK_ROOT_DIR` mutation via
        // the single crate-wide `test_support` lock (no local per-module Mutex).
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let shared = make_shared_data_for_tests();
        let channel_id = ChannelId::new(3_038_100);
        let user_msg_id = MessageId::new(11);
        let placeholder_msg_id = MessageId::new(22);

        let path = sidecar_path(tmp.path(), "discord_queued_placeholders", channel_id);
        let removed = test_rt().block_on(async {
            // The locked insert variant runs under a caller-held persist lock.
            let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
            {
                let _guard = persist_lock.lock().await;
                shared.insert_queued_placeholder_locked(
                    channel_id,
                    user_msg_id,
                    placeholder_msg_id,
                );
            }

            // Memory: the mapping is owned by exactly the placeholder we inserted.
            assert!(shared.queued_placeholder_still_owned(
                channel_id,
                user_msg_id,
                placeholder_msg_id
            ));

            // Sidecar: the channel file mirrors the mapping.
            let contents = std::fs::read_to_string(&path).expect("sidecar must exist after insert");
            assert!(contents.contains("\"user_message_id\": 11"));
            assert!(contents.contains("\"placeholder_message_id\": 22"));

            // Remove (write-through) returns the placeholder id and clears memory + sidecar.
            shared
                .remove_queued_placeholder(channel_id, user_msg_id)
                .await
        });
        assert_eq!(removed, Some(placeholder_msg_id));
        assert!(!shared.queued_placeholder_still_owned(
            channel_id,
            user_msg_id,
            placeholder_msg_id
        ));
        assert!(!path.exists(), "empty channel sidecar must be removed");
    }

    #[test]
    fn queued_placeholder_still_owned_branches() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let shared = make_shared_data_for_tests();
        let channel_id = ChannelId::new(3_038_200);
        let user_msg_id = MessageId::new(31);
        let placeholder_msg_id = MessageId::new(32);
        let other_placeholder = MessageId::new(33);

        // Absent mapping → not owned.
        assert!(!shared.queued_placeholder_still_owned(
            channel_id,
            user_msg_id,
            placeholder_msg_id
        ));

        test_rt().block_on(async {
            let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
            let _guard = persist_lock.lock().await;
            shared.insert_queued_placeholder_locked(channel_id, user_msg_id, placeholder_msg_id);
        });

        // Owned by our placeholder, not by a different one.
        assert!(shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id));
        assert!(!shared.queued_placeholder_still_owned(channel_id, user_msg_id, other_placeholder));
    }

    #[test]
    fn queue_exit_placeholder_clears_round_trip_with_sidecar() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let shared = make_shared_data_for_tests();
        let channel_id = ChannelId::new(3_038_300);
        let user_msg_id = MessageId::new(41);
        let placeholder_msg_id = MessageId::new(42);

        let path = sidecar_path(
            tmp.path(),
            "discord_queue_exit_placeholder_clears",
            channel_id,
        );
        test_rt().block_on(async {
            shared
                .add_pending_queue_exit_placeholder_clear_one(
                    channel_id,
                    user_msg_id,
                    placeholder_msg_id,
                )
                .await;

            let pending = shared.pending_queue_exit_placeholder_clears();
            assert_eq!(pending, vec![(channel_id, user_msg_id, placeholder_msg_id)]);

            let contents = std::fs::read_to_string(&path).expect("clears sidecar must exist");
            assert!(contents.contains("\"user_message_id\": 41"));
            assert!(contents.contains("\"placeholder_message_id\": 42"));

            shared
                .remove_pending_queue_exit_placeholder_clears(
                    channel_id,
                    &[(user_msg_id, placeholder_msg_id)],
                )
                .await;
        });

        assert!(shared.pending_queue_exit_placeholder_clears().is_empty());
        assert!(!path.exists(), "empty clears sidecar must be removed");
    }

    #[test]
    fn queued_placeholders_persist_lock_identity() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let shared = make_shared_data_for_tests();
        let channel_a = ChannelId::new(3_038_400);
        let channel_b = ChannelId::new(3_038_401);

        let lock_a1 = shared.queued_placeholders_persist_lock(channel_a);
        let lock_a2 = shared.queued_placeholders_persist_lock(channel_a);
        let lock_b = shared.queued_placeholders_persist_lock(channel_b);

        assert!(
            Arc::ptr_eq(&lock_a1, &lock_a2),
            "same channel must reuse the same lock"
        );
        assert!(
            !Arc::ptr_eq(&lock_a1, &lock_b),
            "different channels must get distinct locks"
        );
    }

    // #3089 A0 — characterization of the `LeaseOutcome` failure-signal
    // representation and its I2 invariant (design §5 A0 item 3, signal #1 of 5).
    // The dormant `DeliveryLeaseCell` state machine is already pinned by
    // `turn_finalizer::tests::delivery_lease`; this pins the load-bearing I2
    // datum the controller must preserve — `commit` RECORDS the three-way
    // outcome verbatim and never collapses NotDelivered/Unknown into Delivered,
    // so the caller can refuse to advance the offset for the ambiguous arms.
    // Pinned inline in this `#[cfg(test)] mod` of the FROZEN (baseline 4944)
    // file => ZERO production LoC.
    mod a0_failure_signal_characterization_tests {
        use super::super::turn_finalizer::TurnKey;
        use super::super::{
            DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseOutcome, LeaseSnapshot,
        };
        use serenity::model::id::ChannelId;

        fn turn() -> DeliveryLeaseKey {
            DeliveryLeaseKey::from_turn_key(TurnKey::new(ChannelId::new(7), 11, 0))
        }

        #[test]
        fn a0_lease_outcome_has_exactly_three_distinct_arms() {
            assert_ne!(LeaseOutcome::Delivered, LeaseOutcome::NotDelivered);
            assert_ne!(LeaseOutcome::Delivered, LeaseOutcome::Unknown);
            assert_ne!(LeaseOutcome::NotDelivered, LeaseOutcome::Unknown);
        }

        #[test]
        fn a0_commit_records_each_outcome_verbatim_without_collapsing() {
            for outcome in [
                LeaseOutcome::Delivered,
                LeaseOutcome::NotDelivered,
                LeaseOutcome::Unknown,
            ] {
                let cell = DeliveryLeaseCell::new(ChannelId::new(7));
                let holder = LeaseHolder::Bridge;
                assert!(cell.try_acquire(turn(), holder, 100, 200, 1_000));
                assert!(
                    cell.commit(holder, turn(), 100, 200, outcome),
                    "identity-matched commit of {outcome:?} succeeds"
                );
                match cell.read() {
                    LeaseSnapshot::Committed {
                        outcome: got,
                        start,
                        end,
                        ..
                    } => {
                        assert_eq!(got, outcome, "committed outcome is recorded verbatim");
                        assert_eq!((start, end), (100, 200), "range is preserved on commit");
                    }
                    other => panic!("expected Committed{{{outcome:?}}}, got {other:?}"),
                }
            }
        }

        #[test]
        fn a0_unknown_and_not_delivered_are_distinguishable_after_commit() {
            // This pins ONLY that `DeliveryLeaseCell::commit` preserves each
            // distinct outcome (so the caller can tell them apart). The I2
            // advance rule itself — committed offset advances ONLY on Delivered
            // — is characterized against the REAL production advance path in
            // `turn_bridge::terminal_delivery`'s
            // `a0_i2_advance_characterization_tests` (driving
            // `BridgeDeliveryLease::commit_and_advance`), NOT a local closure.
            let delivered = committed_outcome_of(LeaseOutcome::Delivered);
            let not_delivered = committed_outcome_of(LeaseOutcome::NotDelivered);
            let unknown = committed_outcome_of(LeaseOutcome::Unknown);

            assert_eq!(delivered, LeaseOutcome::Delivered);
            assert_eq!(not_delivered, LeaseOutcome::NotDelivered);
            assert_eq!(unknown, LeaseOutcome::Unknown);
        }

        fn committed_outcome_of(outcome: LeaseOutcome) -> LeaseOutcome {
            let cell = DeliveryLeaseCell::new(ChannelId::new(7));
            let holder = LeaseHolder::Sink;
            assert!(cell.try_acquire(turn(), holder, 0, 5, 1_000));
            assert!(cell.commit(holder, turn(), 0, 5, outcome));
            match cell.read() {
                LeaseSnapshot::Committed { outcome, .. } => outcome,
                other => panic!("expected Committed, got {other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod hard_ceiling_tests {
    use super::{
        ProviderKind, clamp_auto_extend_deadline_ms, codex_turn_hard_ceiling_timeout,
        turn_hard_ceiling_deadline_ms, turn_hard_ceiling_timeout,
    };

    #[test]
    fn clamp_caps_proposal_above_ceiling() {
        let ceiling = 1_000_000;
        let (clamped, did_clamp) = clamp_auto_extend_deadline_ms(ceiling + 50_000, ceiling);
        assert_eq!(clamped, ceiling);
        assert!(did_clamp);
    }

    #[test]
    fn clamp_leaves_proposal_below_ceiling_untouched() {
        let ceiling = 1_000_000;
        let proposed = ceiling - 50_000;
        let (clamped, did_clamp) = clamp_auto_extend_deadline_ms(proposed, ceiling);
        assert_eq!(clamped, proposed);
        assert!(!did_clamp);
    }

    #[test]
    fn clamp_at_exact_ceiling_is_not_a_clamp() {
        let ceiling = 1_000_000;
        let (clamped, did_clamp) = clamp_auto_extend_deadline_ms(ceiling, ceiling);
        assert_eq!(clamped, ceiling);
        assert!(
            !did_clamp,
            "equal-to-ceiling must not be reported as clamped"
        );
    }

    #[test]
    fn codex_uses_tighter_ceiling_than_generic() {
        // Defaults: generic 6h, codex 4h. Codex's ceiling deadline must be
        // strictly earlier than the generic provider's for the same start.
        let start = 10_000_000;
        let codex = turn_hard_ceiling_deadline_ms(start, &ProviderKind::Codex);
        let claude = turn_hard_ceiling_deadline_ms(start, &ProviderKind::Claude);
        assert_eq!(
            codex,
            start + codex_turn_hard_ceiling_timeout().as_millis() as i64
        );
        assert_eq!(
            claude,
            start + turn_hard_ceiling_timeout().as_millis() as i64
        );
        // Only assert ordering when the env hasn't overridden defaults.
        if std::env::var("AGENTDESK_CODEX_TURN_HARD_CEILING_SECS").is_err()
            && std::env::var("AGENTDESK_TURN_HARD_CEILING_SECS").is_err()
        {
            assert!(
                codex < claude,
                "codex ceiling ({codex}) must be earlier than generic ceiling ({claude})"
            );
        }
    }

    /// #3557 (A) Codex-review fix: the INITIAL watchdog deadline must already be
    /// capped at the provider ceiling, not only the auto-extend clamp. This
    /// reproduces the `min(now + watchdog_timeout, ceiling_deadline)` the
    /// watchdog now applies at spawn. With a 6h watchdog timeout and the tighter
    /// 4h Codex ceiling, the initial deadline must land at 4h (the ceiling), so
    /// a hung Codex turn is reconciled at 4h instead of 6h.
    #[test]
    fn initial_deadline_is_capped_at_codex_ceiling() {
        // Only meaningful with default ceilings (codex 4h < generic/timeout 6h).
        if std::env::var("AGENTDESK_CODEX_TURN_HARD_CEILING_SECS").is_ok()
            || std::env::var("AGENTDESK_TURN_TIMEOUT_SECS").is_ok()
        {
            return;
        }
        let now_ms: i64 = 1_000_000_000;
        let watchdog_timeout_ms = super::turn_watchdog_timeout().as_millis() as i64; // 6h
        let proposed_initial_dl = now_ms + watchdog_timeout_ms;
        let codex_ceiling = turn_hard_ceiling_deadline_ms(now_ms, &ProviderKind::Codex);
        let initial = std::cmp::min(proposed_initial_dl, codex_ceiling);
        assert_eq!(
            initial, codex_ceiling,
            "Codex initial deadline must be capped at the 4h ceiling, not the 6h timeout"
        );
        assert!(
            initial < proposed_initial_dl,
            "the cap must actually lower the initial deadline below the 6h timeout"
        );
        // The cap binds => the init-time warn condition (`proposed > ceiling`)
        // is true, so the operator gets the one-shot ceiling warning.
        assert!(proposed_initial_dl > codex_ceiling);
    }

    /// For a non-Codex provider whose ceiling equals the watchdog timeout (the
    /// non-destructive default), the initial cap is a no-op: `min` leaves the
    /// timeout-based deadline untouched and the init warn does NOT fire.
    #[test]
    fn initial_deadline_uncapped_when_ceiling_equals_timeout() {
        if std::env::var("AGENTDESK_TURN_HARD_CEILING_SECS").is_ok()
            || std::env::var("AGENTDESK_TURN_TIMEOUT_SECS").is_ok()
        {
            return;
        }
        let now_ms: i64 = 2_000_000_000;
        let watchdog_timeout_ms = super::turn_watchdog_timeout().as_millis() as i64;
        let proposed_initial_dl = now_ms + watchdog_timeout_ms;
        let claude_ceiling = turn_hard_ceiling_deadline_ms(now_ms, &ProviderKind::Claude);
        let initial = std::cmp::min(proposed_initial_dl, claude_ceiling);
        // Defaults: generic ceiling 6h == watchdog timeout 6h.
        assert_eq!(initial, proposed_initial_dl);
        assert!(
            proposed_initial_dl <= claude_ceiling,
            "with equal defaults the init warn (proposed > ceiling) must not fire"
        );
    }
}
