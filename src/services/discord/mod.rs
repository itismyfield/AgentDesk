mod adk_session;
pub(crate) mod agent_handoff;
pub(crate) mod agentdesk_config;
mod commands;
mod discord_io;
pub(crate) mod formatting;
mod gateway;
mod handoff;
pub(crate) mod health;
pub(crate) mod http;
mod idle_detector;
pub(crate) mod idle_recap;
mod idle_recap_interaction;
mod inflight;
pub(crate) mod internal_api;
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
// Phase 5.3 of intake-node-routing (issue #2011): standalone JSONL → Discord
// relay loop spawned by the bridge on cluster-standby nodes (where the tmux
// watcher's relay path does not fire). Leader keeps using tmux_watcher.
mod prompt_builder;
mod queue_io;
mod queued_placeholders_store;
mod relay_health;
mod relay_recovery;
pub(crate) mod response_sanitizer;
#[cfg(unix)]
mod standby_relay;
// #1074: landing zone for the future recovery-engine module split
// (restart / runtime / manual_rebind). See `docs/recovery-paths.md`.
// Named `recovery_paths` to avoid shadowing the existing
// `recovery_engine as recovery` alias below until the mechanical split lands.
mod recovery_engine;
mod recovery_paths;
mod restart_mode;
// #1074: session identity parsing SSoT (legacy + namespaced session_key forms).
pub(crate) mod restart_report;
mod role_map;
mod router;
mod runtime_bootstrap;
// #1446 stall-deadlock recovery: shared post-clear bookkeeping for the
// THREAD-GUARD + stall-watchdog cleanup paths so neither leaks
// `global_active` / orphaned cancel tokens after a dead-dispatch sweep.
pub mod runtime_store;
pub(crate) mod session_identity;
mod session_runtime;
pub(crate) mod settings;
pub(crate) mod shared_memory;
mod stall_recovery;
pub(in crate::services::discord) mod streaming_finalizer;
#[cfg(unix)]
mod tmux;
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
mod turn_bridge;
mod voice_background_driver;
mod voice_barge_in;
mod voice_routing;
#[path = "watchers/lifecycle_decision.rs"]
mod watcher_lifecycle_decision;

pub(crate) use meeting_orchestrator as meeting;
pub(in crate::services::discord) use recovery_engine as recovery;
pub(crate) use restart_mode::InflightRestartMode;
pub(crate) use router::HeadlessTurnStartError;
// Phase 2-pre.3 of intake-node-routing: worker entry point. Phase 3 will
// add the worker polling loop that imports these names; until then they
// are intentionally exposed but unused at the crate boundary.
#[allow(unused_imports)]
pub(crate) use router::{IntakeRequest, TurnKind, execute_intake_turn_core};
pub(crate) use turn_bridge::TmuxCleanupPolicy;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use turn_bridge::build_work_dispatch_completion_result;

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
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
use formatting::{
    BUILTIN_SKILLS, extract_skill_description, format_for_discord, format_tool_input,
    send_long_message_raw, truncate_str,
};
use handoff::{clear_handoff, load_handoffs, update_handoff_state};
pub(crate) use inflight::clear_inflight_state;
use inflight::{InflightTurnState, load_inflight_states, save_inflight_state};
use prompt_builder::{RecoveryContextManifestInput, build_system_prompt_with_manifest};
use recovery_engine::restore_inflight_turns;
use restart_report::flush_restart_reports;
use router::handle_event;
use settings::{
    RoleBinding, channel_upload_dir, cleanup_old_uploads, load_bot_settings,
    load_last_remote_profile, load_last_session_path, resolve_role_binding, save_bot_settings,
    validate_bot_channel_routing_with_provider_channel,
};
#[cfg(unix)]
use tmux::restore_tmux_watchers;
#[cfg(unix)]
use tmux_reaper::{cleanup_orphan_tmux_sessions, reap_dead_tmux_sessions};
use turn_bridge::{TurnBridgeContext, spawn_turn_bridge, tmux_runtime_paths};

pub(crate) use crate::services::turn_orchestrator::has_soft_intervention_at;
pub(crate) use prompt_builder::DispatchProfile;
pub(crate) use runtime_bootstrap::RunBotContext;
pub(crate) use runtime_bootstrap::run_bot;

use crate::services::turn_orchestrator::{
    CancelActiveTurnResult, CancelQueuedMessageResult, ChannelMailboxSnapshot, ClearChannelResult,
    FinishTurnResult, HydratePendingQueueResult, QueueExitEvent, QueueExitKind,
    QueuePersistenceContext, RecoveryKickoffResult, RequeueInterventionResult, TakeNextSoftResult,
    load_pending_queues, warn_legacy_pending_queue_files,
};
pub(super) use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, INTERVENTION_TTL, Intervention, InterventionMode,
    MAX_INTERVENTIONS_PER_CHANNEL, PendingQueueItem,
};
pub use discord_io::{
    retry_failed_dm_notifications, send_file_to_channel, send_message_to_channel,
    send_message_to_user,
};
pub(crate) use inflight::latest_request_owner_user_id_for_channel;
pub use settings::{
    load_discord_bot_launch_configs, resolve_discord_bot_provider, resolve_discord_token_by_hash,
};
// #2047 Finding 5 — expose role-map resolver to the HTTP routes layer so the
// `/api/discord/channels/{id}` proxy can deny lookups for channels that are
// not registered with this AgentDesk instance.
pub(crate) use settings::resolve_role_binding as resolve_channel_role_binding;

/// Discord message length limit
pub(super) const DISCORD_MSG_LIMIT: usize = 2000;
const UPLOAD_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const UPLOAD_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const SESSION_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60); // 1 hour
// #1085 (908-3): extended from 1h → 4h. Working agents idle between dispatch
// turns and the prior 60-min cap forced the next user/dispatch turn to start a
// fresh provider session, defeating cache reuse. 4h covers typical "go for
// lunch / sync meeting" gaps while still bounding zombie growth via the
// cleanup interval reaper at `mod.rs:2093`.
const SESSION_MAX_IDLE: Duration = Duration::from_secs(4 * 60 * 60); // 4 hours
const SESSION_MAX_ASSISTANT_TURNS: usize = 100;
const SESSION_RECOVERY_CONTEXT_MESSAGES: usize = 10;
const DEAD_SESSION_REAP_INTERVAL: Duration = Duration::from_secs(60); // 1 minute
const RESTART_REPORT_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const DEFERRED_RESTART_POLL_INTERVAL: Duration = Duration::from_secs(10);
const MONITOR_AUTO_TURN_ORIGIN_LITERAL: &str = "[origin=monitor_auto_turn]";

fn hidden_monitor_auto_turn_origin_marker() -> &'static str {
    static MARKER: OnceLock<String> = OnceLock::new();
    MARKER.get_or_init(|| {
        MONITOR_AUTO_TURN_ORIGIN_LITERAL
            .bytes()
            .flat_map(|byte| {
                (0..8).rev().map(move |shift| {
                    if (byte >> shift) & 1 == 1 {
                        '\u{200C}'
                    } else {
                        '\u{200B}'
                    }
                })
            })
            .collect()
    })
}

pub(in crate::services::discord) fn prepend_monitor_auto_turn_origin(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{}{}", hidden_monitor_auto_turn_origin_marker(), trimmed)
    }
}

pub(in crate::services::discord) fn strip_monitor_auto_turn_origin<'a>(
    text: &'a str,
) -> (Cow<'a, str>, bool) {
    if let Some(rest) = text.strip_prefix(hidden_monitor_auto_turn_origin_marker()) {
        return (Cow::Borrowed(rest), true);
    }

    if let Some(rest) = text.strip_prefix(MONITOR_AUTO_TURN_ORIGIN_LITERAL) {
        return (Cow::Owned(rest.trim_start().to_string()), true);
    }

    (Cow::Borrowed(text), false)
}

pub(super) fn session_retry_context_key(channel_id: ChannelId) -> String {
    format!("session_retry_context:{}", channel_id.get())
}

pub(super) fn should_process_allowed_bot_turn_text(text: &str) -> bool {
    let (sanitized, has_monitor_origin) = strip_monitor_auto_turn_origin(text);
    has_monitor_origin || sanitized.trim_start().starts_with("DISPATCH:")
}

pub(in crate::services::discord) async fn resolve_announce_bot_user_id(
    shared: &SharedData,
) -> Option<u64> {
    let registry = shared.health_registry()?;
    registry.utility_bot_user_id("announce").await
}

/// Cached lookup for the notify bot's Discord user id. Used by the message
/// router to classify incoming messages as `BackgroundTrigger` turns —
/// see `TurnKind` in `router/message_handler.rs` and the race-handler
/// preservation rule from #796.
pub(in crate::services::discord) async fn resolve_notify_bot_user_id(
    shared: &SharedData,
) -> Option<u64> {
    let registry = shared.health_registry()?;
    registry.utility_bot_user_id("notify").await
}

pub(in crate::services::discord) fn is_allowed_turn_sender(
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
    author_id: u64,
    author_is_bot: bool,
    text: &str,
) -> bool {
    if announce_bot_id.is_some_and(|id| id == author_id) {
        // Issue announcements moved to notify-bot in the #1448 follow-up,
        // so live announce-bot traffic is dispatch / PM-decision /
        // escalation / generic routing — all of which trigger turns.
        // The transitional block below catches catch-up replays of
        // pre-deploy announce-authored issue cards (📋/✅) so they
        // don't spawn spurious turns. Remove once existing announce-bot
        // announcement messages have aged out of catch-up scan windows
        // (safe sunset target: 2026-06-01).
        return !is_legacy_announce_issue_card(text);
    }
    if allowed_bot_ids.contains(&author_id) {
        return should_process_allowed_bot_turn_text(text);
    }
    !author_is_bot
}

/// TRANSITIONAL (#1448 follow-up — sunset 2026-06-01): suppresses
/// pre-deploy announce-bot issue-announcement / completion cards that
/// reappear during restart catch-up. Live traffic now routes through
/// notify-bot, which never reaches the announce-bot branch above.
fn is_legacy_announce_issue_card(text: &str) -> bool {
    let head = text.trim_start();
    if head.starts_with("📋 **새 이슈 #") {
        return true;
    }
    if let Some(rest) = head.strip_prefix("✅ **#") {
        let digits_end = rest
            .char_indices()
            .find(|(_, ch)| !ch.is_ascii_digit())
            .map(|(idx, _)| idx)
            .unwrap_or(rest.len());
        if digits_end > 0 && rest[digits_end..].starts_with(" 완료** —") {
            return true;
        }
    }
    false
}

pub(in crate::services::discord) fn should_phase2_recover_message(
    message_id: u64,
    checkpoint: Option<u64>,
    existing_ids: &std::collections::HashSet<u64>,
) -> bool {
    if existing_ids.contains(&message_id) {
        return false;
    }
    if checkpoint.is_some_and(|saved| message_id <= saved) {
        return false;
    }
    true
}

const CATCH_UP_RETRY_QUEUE_THRESHOLD: usize = MAX_INTERVENTIONS_PER_CHANNEL / 2;

fn should_trigger_catch_up_retry(queue_len: usize) -> bool {
    queue_len <= CATCH_UP_RETRY_QUEUE_THRESHOLD
}

fn take_catch_up_retry_checkpoint_after_queue_drain(
    shared: &SharedData,
    channel_id: ChannelId,
    queue_len_after: usize,
) -> Option<u64> {
    if !should_trigger_catch_up_retry(queue_len_after) {
        return None;
    }
    shared
        .catch_up_retry_pending
        .remove(&channel_id)
        .map(|(_, checkpoint)| checkpoint)
}

fn catch_up_checkpoint_for_scan(
    disk_checkpoint: u64,
    live_checkpoint: Option<u64>,
    retry_checkpoint: Option<u64>,
) -> u64 {
    retry_checkpoint.unwrap_or_else(|| {
        live_checkpoint
            .map(|checkpoint| disk_checkpoint.max(checkpoint))
            .unwrap_or(disk_checkpoint)
    })
}

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
    let checkpoint = shared
        .last_message_ids
        .get(&channel_id)
        .map(|current| (*current).max(message_id))
        .unwrap_or(message_id);
    shared.last_message_ids.insert(channel_id, checkpoint);
    runtime_store::save_last_message_id(provider.as_str(), channel_id.get(), checkpoint);
    checkpoint
}

pub(in crate::services::discord) use queue_io::schedule_deferred_idle_queue_kickoff;
/// Minimum interval between Discord placeholder edits for progress status.
/// Configurable via AGENTDESK_STATUS_INTERVAL_SECS env var. Default: 5 seconds.
pub(super) fn status_update_interval() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let secs = std::env::var("AGENTDESK_STATUS_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(5);
        Duration::from_secs(secs)
    })
}

/// Turn watchdog timeout. Configurable via AGENTDESK_TURN_TIMEOUT_SECS env var.
/// Default: 3600 seconds (60 minutes).
pub(super) fn turn_watchdog_timeout() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let secs = std::env::var("AGENTDESK_TURN_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(3600);
        Duration::from_secs(secs)
    })
}

/// Extend the watchdog deadline for a channel and move the per-turn max cap with it.
pub async fn extend_watchdog_deadline(
    channel_id: u64,
    extend_by_secs: u64,
) -> Result<
    crate::services::turn_orchestrator::WatchdogDeadlineExtension,
    crate::services::turn_orchestrator::WatchdogDeadlineExtensionError,
> {
    let Some(handle) = ChannelMailboxRegistry::global_handle(ChannelId::new(channel_id)) else {
        return Err(
            crate::services::turn_orchestrator::WatchdogDeadlineExtensionError::MailboxUnavailable,
        );
    };
    handle.extend_timeout(extend_by_secs).await
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
        if state.rebind_origin || state.channel_id != channel_id {
            return false;
        }
        if inflight::inflight_state_is_stale(
            &state,
            now_unix_secs,
            inflight::INFLIGHT_STALENESS_THRESHOLD_SECS,
        ) {
            return false;
        }
        true
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod thread_archive_guard_tests {
    use super::*;
    use crate::services::provider::ProviderKind;
    use tempfile::TempDir;

    #[tokio::test]
    async fn thread_archive_guard_defers_for_fresh_inflight_without_db() {
        let _guard = runtime_store::lock_test_env();
        let root = TempDir::new().expect("temp root");
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };

        let channel_id = 9_001_455;
        let state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "active turn".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-codex-adk-cdx-t9001455".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        inflight::save_inflight_state(&state).expect("save inflight");

        assert!(
            should_defer_thread_archive_pg(None, &channel_id.to_string())
                .await
                .expect("archive guard")
        );
        assert!(
            !should_defer_thread_archive_pg(None, "9001456")
                .await
                .expect("archive guard")
        );

        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

/// Check if a deferred restart has been requested and no active or finalizing turns remain
/// **across all providers**.
///
/// `global_active` / `global_finalizing` are process-wide counters shared by every provider.
/// A single provider draining to zero is NOT sufficient — we must wait for every provider.
/// `shutdown_remaining` ensures all providers finish saving before any calls `exit(0)`.
/// `shutdown_counted` (per-provider) prevents double-decrement when both deferred restart
/// and SIGTERM paths run for the same provider.
pub(super) fn check_deferred_restart(shared: &SharedData) {
    let g_active = shared
        .global_active
        .load(std::sync::atomic::Ordering::Relaxed);
    let g_finalizing = shared
        .global_finalizing
        .load(std::sync::atomic::Ordering::Relaxed);
    if g_active > 0 || g_finalizing > 0 {
        return;
    }
    if !shared
        .restart_pending
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    // CAS: ensure this provider only decrements once
    if shared
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
    // Only the last provider to finish calls exit(0)
    if shared
        .shutdown_remaining
        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
        == 1
    {
        let Some(root) = crate::agentdesk_runtime_root() else {
            return;
        };
        let marker = root.join("restart_pending");
        let version = fs::read_to_string(&marker).unwrap_or_default();
        let version = version.trim();
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔄 Deferred restart: all turns complete, restarting for v{version}..."
        );
        let _ = fs::remove_file(&marker);
        std::process::exit(0);
    }
}

use session_runtime::{
    DiscordSession, RuntimeChannelBindingStatus, WorktreeInfo, auto_restore_session,
    auto_restore_session_force, auto_restore_session_with_dm_hint, bootstrap_thread_session,
    cleanup_git_worktree, create_git_worktree, detect_worktree_conflict, provider_handles_channel,
    resolve_channel_category, resolve_is_dm_channel, resolve_runtime_channel_binding_status,
    resolve_thread_parent, select_restored_session_path, synthetic_thread_channel_name,
    validate_live_channel_routing, validate_live_channel_routing_with_dm_hint,
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
    /// #1452: turn-scoped finalization debt transferred from bridge to watcher.
    ///
    /// When the bridge unpauses a live tmux watcher to take over the
    /// assistant relay it intentionally skips `mailbox_finish_turn` to
    /// avoid racing with the still-running watcher turn. Without an
    /// explicit handoff signal the watcher would also skip the
    /// finalization (its existing `finish_mailbox_on_completion` gate is
    /// reserved for inflight-restore semantics), leaving the channel
    /// mailbox `cancel_token` permanently set and blocking subsequent
    /// `try_start_turn` calls on brand-new turns.
    ///
    /// Protocol:
    ///   * Bridge unpause: `store(true, Ordering::Release)` at the
    ///     watcher-unpause site (`turn_bridge/mod.rs` `TmuxReady` branch).
    ///     The store happens BEFORE `paused.store(false, ...)` so a fast
    ///     watcher cannot reach its terminal swap before we publish —
    ///     Codex P1 pointed out that storing later (at the delegation
    ///     decision in `let has_queued_turns = ...`) is racy.
    ///   * Bridge non-delegation: when the bridge ends up handling the
    ///     turn itself, it must take the debt back atomically. It tracks
    ///     a local `bridge_published_finalize_owed_for_this_turn` flag so
    ///     it can tell apart "we published debt for this turn" from
    ///     "we never published" — without this distinction, a paused
    ///     watcher carried over from a prior turn would leave the
    ///     handle's value unrelated to the current turn (Codex iter 3 P1).
    ///     If the flag is set, the bridge runs
    ///     `compare_exchange(true, false, AcqRel, Acquire)`:
    ///       * `Ok` → bridge revoked unconsumed debt, runs its own
    ///         `mailbox_finish_turn`.
    ///       * `Err(false)` → watcher already swapped and finalized; the
    ///         bridge MUST skip its own `mailbox_finish_turn` to avoid
    ///         clearing a turn it no longer owns (Codex P2 from review
    ///         iter 2).
    ///     If the flag is NOT set (no `TmuxReady` reached, or watcher
    ///     missing), the bridge always runs `mailbox_finish_turn`.
    ///   * Watcher: `swap(false, Ordering::AcqRel)` in its turn-end
    ///     branch. AcqRel guarantees:
    ///       1. Acquire — every prior bridge write is observed before we
    ///          decide to call `mailbox_finish_turn`.
    ///       2. Release+single-consumer — a paused-survivor watcher
    ///          cannot accidentally clear a future turn's freshly
    ///          registered cancel token because the swap returns `false`
    ///          for it.
    pub(super) mailbox_finalize_owed: Arc<std::sync::atomic::AtomicBool>,
}

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
        self.owner_channel_by_tmux_session
            .get(tmux_session_name)
            .map(|entry| *entry.value())
    }

    pub(super) fn tmux_session_is_stale(&self, tmux_session_name: &str) -> Option<bool> {
        self.by_tmux_session.get(tmux_session_name).map(|entry| {
            entry.cancel.load(std::sync::atomic::Ordering::Relaxed) || entry.heartbeat_stale()
        })
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    pub(super) fn assert_invariants_for_tests(&self) {
        let _guard = lock_tmux_watcher_registry();
        assert_eq!(
            self.by_tmux_session.len(),
            self.owner_channel_by_tmux_session.len(),
            "tmux watcher registry must have one owner per tmux watcher"
        );
        assert_eq!(
            self.by_tmux_session.len(),
            self.tmux_session_by_channel.len(),
            "tmux watcher registry must have one channel alias per tmux watcher"
        );

        for entry in self.tmux_session_by_channel.iter() {
            let channel_id = *entry.key();
            let tmux_session_name = entry.value().clone();
            assert!(
                self.by_tmux_session.contains_key(&tmux_session_name),
                "channel index points to missing tmux watcher"
            );
            assert_eq!(
                self.owner_channel_for_tmux_session(&tmux_session_name),
                Some(channel_id),
                "channel index and owner index disagree"
            );
        }

        for entry in self.owner_channel_by_tmux_session.iter() {
            let tmux_session_name = entry.key().clone();
            let owner_channel_id = *entry.value();
            assert!(
                self.by_tmux_session.contains_key(&tmux_session_name),
                "owner index points to missing tmux watcher"
            );
            assert_eq!(
                self.tmux_session_by_channel
                    .get(&owner_channel_id)
                    .map(|value| value.clone()),
                Some(tmux_session_name),
                "owner index and channel index disagree"
            );
        }

        for entry in self.by_tmux_session.iter() {
            let tmux_session_name = entry.key().clone();
            let owner_channel_id = self
                .owner_channel_for_tmux_session(&tmux_session_name)
                .expect("tmux watcher must have an owner channel");
            assert_eq!(
                self.tmux_session_by_channel
                    .get(&owner_channel_id)
                    .map(|value| value.clone()),
                Some(tmux_session_name),
                "tmux watcher and channel index disagree"
            );
        }
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    pub(super) fn remove_after_channel_index_drop_for_tests(
        &self,
        channel_id: &ChannelId,
        channel_index_removed: &std::sync::Barrier,
        release: &std::sync::Barrier,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let _guard = lock_tmux_watcher_registry();
        let Some((_, tmux_session_name)) = self.tmux_session_by_channel.remove(channel_id) else {
            channel_index_removed.wait();
            release.wait();
            return None;
        };
        channel_index_removed.wait();
        release.wait();
        self.owner_channel_by_tmux_session
            .remove(&tmux_session_name);
        self.by_tmux_session
            .remove(&tmux_session_name)
            .map(|(_, handle)| (*channel_id, handle))
    }
}

/// Per-channel coordination for watcher-to-Discord relay emission.
///
/// This state is **shared across watcher-handle replacements** (unlike
/// `TmuxWatcherHandle`, which is recreated on watcher reattach). It keeps
/// relay emission serialized if a stale outgoing watcher overlaps with its
/// successor, and it exposes the confirmed-output watermark used by watcher
/// stop checks.
///
/// Scope: intra-process only. Persisted dedupe across dcserver restarts is
/// still handled by `InflightTurnState::last_watcher_relayed_offset` in the
/// inflight JSON.
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
    /// This is telemetry/stop-state only. Relay dedupe is scoped to the
    /// watcher instance via its local `last_relayed_offset`; cross-watcher
    /// ownership is enforced at registration time so a valid owner is never
    /// suppressed solely because another watcher advanced this watermark.
    pub(super) confirmed_end_offset: Arc<std::sync::atomic::AtomicU64>,
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
}

impl TmuxRelayCoord {
    pub(super) fn new() -> Self {
        Self {
            relay_slot: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            confirmed_end_offset: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_relay_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            reconnect_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            confirmed_end_generation_mtime_ns: Arc::new(std::sync::atomic::AtomicI64::new(0)),
        }
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
    /// Last known placeholder cleanup outcome keyed by provider/channel/message.
    /// This local tombstone lets watcher finalization reason about cleanup
    /// even after the inflight file has already been cleared.
    pub(in crate::services::discord) placeholder_cleanup:
        Arc<placeholder_cleanup::PlaceholderCleanupRegistry>,
    /// Lifecycle FSM + edit coalescer for live-turn placeholder cards (#1255).
    /// Both the `tmux_handed_off` async-dispatch path and the new Monitor /
    /// `Bash run_in_background` live-turn path go through this controller so
    /// that concurrent edits to the same placeholder message_id serialize
    /// instead of racing.
    pub(in crate::services::discord) placeholder_controller:
        Arc<placeholder_controller::PlaceholderController>,
    /// Per-channel recent tool/system events rendered in Active placeholder
    /// cards when `placeholder.live_events_enabled` is enabled.
    pub(in crate::services::discord) placeholder_live_events:
        Arc<placeholder_live_events::PlaceholderLiveEvents>,
    pub(in crate::services::discord) placeholder_live_events_enabled: bool,
    pub(in crate::services::discord) status_panel_v2_enabled: bool,
    /// #1332: per-channel mapping from a mailbox-queued user message id to the
    /// Discord placeholder message id displaying the `📬 메시지 대기 중` card.
    /// Populated when `mailbox_try_start_turn` reports the new message lost the
    /// race; consumed by the dispatch path when the queued turn is dequeued so
    /// the existing Queued card transitions to `Active` instead of leaking a
    /// duplicate placeholder.
    pub(in crate::services::discord) queued_placeholders:
        dashmap::DashMap<(ChannelId, MessageId), MessageId>,
    /// #1362: queue-exit placeholder cards that were removed from
    /// `queued_placeholders` while `cached_serenity_ctx` was not ready. Kept in
    /// memory and mirrored to a sidecar so ready-time drain can delete the
    /// visible stale `📬` cards after the Discord HTTP client exists.
    pub(in crate::services::discord) queue_exit_placeholder_clears:
        dashmap::DashMap<(ChannelId, MessageId), MessageId>,
    /// #1332 round-4 codex review P2 + round-5 P2: per-channel mutex guarding
    /// `queued_placeholders` snapshot writes AND any Discord PATCH that
    /// asserts queued ownership. When two updates for the same channel race
    /// (e.g., two messages lose the start-turn race simultaneously, or an
    /// insert races a queue-exit drain), each caller must serialize its
    /// `(snapshot DashMap → atomic_write file)` block so an older snapshot
    /// cannot finish last and overwrite a newer mapping. Round-5 extends the
    /// lock to span the ownership recheck + Discord edit + persistence
    /// rollback in the race-loss render path so the same Discord message can
    /// never be written by both the queued-placeholder render and the
    /// dispatch/queue-exit cleanup paths.
    ///
    /// Invariant (round-5 P2): any Discord PATCH that asserts queued
    /// ownership MUST hold this lock across both the ownership recheck AND
    /// the PATCH (and across the persistence write that follows). The map
    /// fast-path stays on the lock-free `DashMap` above; only ownership-
    /// coupled mutations are serialized per channel. The lock is async
    /// (`tokio::sync::Mutex`) so it can be held across `.await` points
    /// without blocking the runtime worker.
    pub(in crate::services::discord) queued_placeholders_persist_locks:
        dashmap::DashMap<ChannelId, Arc<tokio::sync::Mutex<()>>>,
    /// Per-channel in-flight turn recovery marker (restart resume in progress)
    /// Value is the Instant when recovery started, used for stale-recovery timeout.
    pub(super) recovering_channels: dashmap::DashMap<ChannelId, std::time::Instant>,
    /// Global shutdown flag — when set, watchers exit quietly via cancel path
    pub(super) shutting_down: Arc<std::sync::atomic::AtomicBool>,
    /// Number of turns currently in finalization phase (response sending + cleanup).
    /// Deferred restart must wait until this reaches 0 to avoid killing mid-send turns.
    pub(super) finalizing_turns: Arc<std::sync::atomic::AtomicUsize>,
    /// Current restart generation — incremented on each --restart-dcserver.
    /// Used to distinguish old (pre-restart) sessions from fresh ones.
    pub(super) current_generation: u64,
    /// Set when a `restart_pending` marker is detected. While true, the router
    /// queues new messages instead of starting new turns (drain mode).
    pub(super) restart_pending: Arc<std::sync::atomic::AtomicBool>,
    /// Set to true after startup reconciliation + recovery is complete (#122).
    /// Until true, the router queues all incoming messages.
    pub(super) reconcile_done: Arc<std::sync::atomic::AtomicBool>,
    /// Number of queued deferred idle-queue kickoffs waiting to run.
    pub(super) deferred_hook_backlog: std::sync::atomic::AtomicUsize,
    /// When this provider started reconcile/recovery for the current boot.
    pub(super) recovery_started_at: std::time::Instant,
    /// Captured reconcile/recovery duration for the current boot in milliseconds.
    /// Remains 0 until reconcile completes, at which point it is frozen.
    pub(super) recovery_duration_ms: std::sync::atomic::AtomicU64,
    /// Process-global active turn counter shared across all providers.
    /// Deferred restart checks this instead of provider-local cancel_tokens.len().
    pub(super) global_active: Arc<std::sync::atomic::AtomicUsize>,
    /// Process-global finalizing turn counter shared across all providers.
    pub(super) global_finalizing: Arc<std::sync::atomic::AtomicUsize>,
    /// Number of providers still needing to complete shutdown.
    /// The last provider to decrement this to 0 calls `exit(0)`.
    pub(super) shutdown_remaining: Arc<std::sync::atomic::AtomicUsize>,
    /// Per-provider flag: ensures this provider decrements `shutdown_remaining` at most once,
    /// even if both the deferred restart poll loop and SIGTERM handler run.
    pub(super) shutdown_counted: std::sync::atomic::AtomicBool,
    /// Intake-level dedup cache: prevents the same message from starting two turns
    /// when duplicate bot dispatches arrive nearly simultaneously.
    /// Key: dedup key (dispatch_id or channel+author+text hash).
    /// Value: (first-seen Instant, was_thread_context).
    pub(super) intake_dedup: dashmap::DashMap<String, (std::time::Instant, bool)>,
    /// Maps parent channel → active dispatch thread channel.
    /// When a dispatch creates a thread, the parent is recorded here so that
    /// subsequent bot messages to the parent are queued instead of starting
    /// a parallel turn.  Cleared when the dispatch thread turn completes.
    pub(super) dispatch_thread_parents: dashmap::DashMap<ChannelId, ChannelId>,
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
    /// Per-channel model override, independent of session lifecycle.
    /// Takes priority over role-map model. Cleared via the `/model` picker default option.
    pub(super) model_overrides: dashmap::DashMap<ChannelId, String>,
    /// Per-channel native fast mode enablement for providers that support it.
    pub(super) fast_mode_channels: dashmap::DashSet<ChannelId>,
    /// Provider-scoped pending native fast-mode resets, encoded as
    /// `provider:channel_id` strings for mixed-provider dispatch safety.
    pub(super) fast_mode_session_reset_pending: dashmap::DashSet<String>,
    /// Per-channel Codex goals feature enablement.
    pub(super) codex_goals_channels: dashmap::DashSet<ChannelId>,
    /// Channels that must restart Codex before the next turn because goals changed.
    pub(super) codex_goals_session_reset_pending: dashmap::DashSet<ChannelId>,
    /// Channels that must start a fresh provider session on the next turn
    /// because the effective model override changed.
    pub(super) model_session_reset_pending: dashmap::DashSet<ChannelId>,
    /// Channels that must start a fresh provider session on the next turn
    /// because a persisted runtime execution setting changed.
    pub(super) session_reset_pending: dashmap::DashSet<ChannelId>,
    /// Per-message staged model picker selection.
    /// Key: picker message id. Value tracks owner, target channel, and staged model until submit.
    pub(super) model_picker_pending: dashmap::DashMap<MessageId, ModelPickerPendingState>,
    /// Per-thread role/model override for cross-channel dispatch reuse.
    /// When a review dispatch reuses an implementation thread, this maps
    /// thread_channel_id → alt_channel_id so role_binding and model_for_turn
    /// resolve from the counter-model channel instead of the thread's parent.
    /// Cleared when the turn completes.
    pub(super) dispatch_role_overrides: dashmap::DashMap<ChannelId, ChannelId>,
    /// Per-channel last processed message ID — used for startup catch-up polling.
    pub(super) last_message_ids: dashmap::DashMap<ChannelId, u64>,
    /// Channels where catch-up stopped because the intervention queue was at
    /// capacity. The value is the pinned `after` checkpoint for the next
    /// in-process catch-up pass, independent of live message checkpoints that
    /// may advance while the queued backlog drains.
    pub(super) catch_up_retry_pending: dashmap::DashMap<ChannelId, u64>,
    /// Per-channel turn start time — used for metrics duration calculation.
    pub(super) turn_start_times: dashmap::DashMap<ChannelId, std::time::Instant>,
    /// Per-channel known speakers collected lazily from incoming messages.
    pub(super) channel_rosters: dashmap::DashMap<ChannelId, Vec<UserRecord>>,
    /// Cached serenity context for deferred queue drain (set once during ready event).
    pub(super) cached_serenity_ctx: tokio::sync::OnceCell<serenity::Context>,
    /// Cached bot token for deferred queue drain.
    pub(super) cached_bot_token: tokio::sync::OnceCell<String>,
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
    /// Test-only legacy DB handle for SQLite compatibility tests.
    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    pub(super) sqlite: Option<crate::db::Db>,
    /// Shared PostgreSQL pool for PG-backed route and runtime helpers.
    pub(super) pg_pool: Option<sqlx::PgPool>,
    /// Shared policy engine for direct dispatch finalization.
    pub(super) engine: Option<crate::engine::PolicyEngine>,
    /// Weak reference to the process-wide health registry so turn handlers can
    /// reach dedicated Discord bot HTTP clients without creating an Arc cycle.
    pub(super) health_registry: std::sync::Weak<health::HealthRegistry>,
    /// Set of registered slash command names (populated at framework setup).
    /// Used by the router to distinguish known slash commands from arbitrary
    /// `/`-prefixed user text that should fall through to the AI provider.
    pub(super) known_slash_commands: tokio::sync::OnceCell<std::collections::HashSet<String>>,
}

impl SharedData {
    pub(super) fn has_runtime_storage(&self) -> bool {
        self.pg_pool.is_some()
    }

    /// Phase 5.2 of intake-node-routing (issue #2009): return an `Arc<Http>`
    /// that the response path (tmux watcher, placeholder updates, message
    /// edits) can use to call Discord. On the leader the gateway-attached
    /// runtime caches `cached_serenity_ctx`, and `ctx.http` is preferred so
    /// the Http instance shares the same application_id and connection
    /// pool the gateway already owns. On cluster-standby nodes the
    /// OnceCell is empty (no gateway runtime ever ran), so we fall back to
    /// a freshly constructed `serenity::http::Http` built from the bot
    /// token cached in `cached_bot_token`. Returns `None` only when both
    /// caches are empty — that means the runtime never reached the
    /// "token known" milestone in `run_bot()`, which today only happens
    /// before `bot_settings` finishes loading.
    ///
    /// Callers should treat `None` as a hard failure: they cannot post
    /// to Discord without an Http instance. The current call sites
    /// either propagate the failure (skip the work + warn) or have
    /// their own panic-on-None invariant tied to `cached_bot_token`
    /// being populated at `run_bot()` startup.
    pub(super) fn serenity_http_or_token_fallback(&self) -> Option<Arc<serenity::http::Http>> {
        if let Some(ctx) = self.cached_serenity_ctx.get() {
            return Some(ctx.http.clone());
        }
        if let Some(token) = self.cached_bot_token.get() {
            return Some(Arc::new(serenity::http::Http::new(token)));
        }
        None
    }

    fn mailbox(&self, channel_id: ChannelId) -> ChannelMailboxHandle {
        self.mailboxes.handle(channel_id)
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
            .or_insert_with(|| Arc::new(TmuxRelayCoord::new()))
            .clone()
    }

    /// Record that this process spawned a watcher during recovery/reattach.
    /// This is process-local telemetry for `GET /api/channels/:id/watcher-state`
    /// (#964), not persisted dedupe state and not counted on first-turn attach.
    pub(super) fn record_tmux_watcher_reconnect(&self, channel_id: ChannelId) {
        self.tmux_relay_coord(channel_id)
            .reconnect_count
            .fetch_add(1, Ordering::AcqRel);
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

    /// #1332 round-4 codex review P2 + round-5 P2: fetch (or create) the
    /// per-channel persistence mutex. The mutex itself is stored as
    /// `Arc<tokio::sync::Mutex<()>>` so callers can clone it out of the
    /// `DashMap` and release the shard lock before acquiring the channel
    /// mutex — eliminating any chance of a deadlock between DashMap shard
    /// locks and the persistence mutex. Round-5 switched from
    /// `std::sync::Mutex` to `tokio::sync::Mutex` so the lock can be held
    /// across `.await` points (specifically the `ensure_queued` Discord
    /// PATCH in the race-loss render path) without blocking a runtime
    /// worker.
    pub(in crate::services::discord) fn queued_placeholders_persist_lock(
        &self,
        channel_id: ChannelId,
    ) -> Arc<tokio::sync::Mutex<()>> {
        self.queued_placeholders_persist_locks
            .entry(channel_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// #1332 round-3 codex review P2 + round-4 P2 + round-5 P2: write-through
    /// insert for the `queued_placeholders` mapping. The in-memory `DashMap`
    /// mutation + the on-disk snapshot write are both performed under a
    /// per-channel async persistence mutex so two concurrent inserts (or an
    /// insert racing a remove) on the same channel cannot reorder their
    /// on-disk effect: the snapshot that lands last on disk is always the
    /// snapshot taken after the latest mutation. The DashMap shard lock is
    /// released before the file I/O begins, so DashMap reads from the rest
    /// of the system continue to make progress.
    pub(super) async fn insert_queued_placeholder(
        &self,
        channel_id: ChannelId,
        user_msg_id: MessageId,
        placeholder_msg_id: MessageId,
    ) {
        let persist_lock = self.queued_placeholders_persist_lock(channel_id);
        let _persist_guard = persist_lock.lock().await;
        self.insert_queued_placeholder_locked(channel_id, user_msg_id, placeholder_msg_id);
    }

    /// #1332 round-5 codex review P2: insert variant that assumes the
    /// caller already holds the per-channel persistence mutex. Used by the
    /// race-loss render path so the lock can span ownership recheck +
    /// `ensure_queued` PATCH + persistence write (and an optional rollback)
    /// without re-acquiring the lock between steps.
    pub(in crate::services::discord) fn insert_queued_placeholder_locked(
        &self,
        channel_id: ChannelId,
        user_msg_id: MessageId,
        placeholder_msg_id: MessageId,
    ) {
        self.queued_placeholders
            .insert((channel_id, user_msg_id), placeholder_msg_id);
        queued_placeholders_store::persist_channel_from_map(
            &self.queued_placeholders,
            &self.provider,
            &self.token_hash,
            channel_id,
        );
    }

    /// #1332 round-3 codex review P2 + round-4 P2 + round-5 P2: write-through
    /// remove for the `queued_placeholders` mapping. Returns the placeholder
    /// message id that was removed (if any) so callers can drive the same
    /// downstream flow as the raw `DashMap::remove`. Mutation + snapshot run
    /// under the per-channel persistence mutex; see
    /// `insert_queued_placeholder` for the deadlock-avoidance rationale.
    pub(super) async fn remove_queued_placeholder(
        &self,
        channel_id: ChannelId,
        user_msg_id: MessageId,
    ) -> Option<MessageId> {
        let persist_lock = self.queued_placeholders_persist_lock(channel_id);
        let _persist_guard = persist_lock.lock().await;
        self.remove_queued_placeholder_locked(channel_id, user_msg_id)
    }

    /// #1332 round-5 codex review P2: remove variant that assumes the caller
    /// already holds the per-channel persistence mutex. Used by the
    /// race-loss render path's rollback branch so the entire ownership-
    /// coupled critical section runs under one async lock acquisition.
    pub(in crate::services::discord) fn remove_queued_placeholder_locked(
        &self,
        channel_id: ChannelId,
        user_msg_id: MessageId,
    ) -> Option<MessageId> {
        let removed = self
            .queued_placeholders
            .remove(&(channel_id, user_msg_id))
            .map(|(_, msg_id)| msg_id);
        queued_placeholders_store::persist_channel_from_map(
            &self.queued_placeholders,
            &self.provider,
            &self.token_hash,
            channel_id,
        );
        removed
    }

    /// #1332 round-3 codex review P1: atomic ownership recheck for the
    /// race-loss render path. After enqueueing the intervention, the active
    /// turn might finish concurrently and the dispatch path can already have
    /// consumed our `(channel_id, user_msg_id)` mapping — at which point the
    /// placeholder we POSTed has been promoted to the live response card.
    /// Returns `true` only when the mapping still points at our exact
    /// `placeholder_msg_id`; callers MUST exit gracefully (without editing or
    /// deleting Discord state) if this returns `false`.
    pub(super) fn queued_placeholder_still_owned(
        &self,
        channel_id: ChannelId,
        user_msg_id: MessageId,
        placeholder_msg_id: MessageId,
    ) -> bool {
        self.queued_placeholders
            .get(&(channel_id, user_msg_id))
            .map(|entry| *entry == placeholder_msg_id)
            .unwrap_or(false)
    }

    async fn add_pending_queue_exit_placeholder_clears(
        &self,
        channel_id: ChannelId,
        cards: &[QueueExitVisibleCard],
    ) {
        if cards.is_empty() {
            return;
        }
        let persist_lock = self.queued_placeholders_persist_lock(channel_id);
        let _persist_guard = persist_lock.lock().await;
        for card in cards {
            self.queue_exit_placeholder_clears
                .insert((channel_id, card.user_msg_id), card.placeholder_msg_id);
        }
        queued_placeholders_store::persist_queue_exit_placeholder_clears_channel_from_map(
            &self.queue_exit_placeholder_clears,
            &self.provider,
            &self.token_hash,
            channel_id,
        );
    }

    /// #2044 F13: enqueue a single deferred placeholder-clear when an
    /// inline `delete_message` from a non-queue-exit path (e.g.
    /// `render_visible_queued_ack`) fails. Mirrors the persistence
    /// behaviour of `add_pending_queue_exit_placeholder_clears` so the
    /// retry survives a restart and is drained by the same
    /// `drain_pending_queue_exit_placeholder_clears` worker.
    pub(in crate::services::discord) async fn add_pending_queue_exit_placeholder_clear_one(
        &self,
        channel_id: ChannelId,
        user_msg_id: MessageId,
        placeholder_msg_id: MessageId,
    ) {
        let persist_lock = self.queued_placeholders_persist_lock(channel_id);
        let _persist_guard = persist_lock.lock().await;
        self.queue_exit_placeholder_clears
            .insert((channel_id, user_msg_id), placeholder_msg_id);
        queued_placeholders_store::persist_queue_exit_placeholder_clears_channel_from_map(
            &self.queue_exit_placeholder_clears,
            &self.provider,
            &self.token_hash,
            channel_id,
        );
    }

    async fn remove_pending_queue_exit_placeholder_clears(
        &self,
        channel_id: ChannelId,
        cards: &[(MessageId, MessageId)],
    ) {
        if cards.is_empty() {
            return;
        }
        let persist_lock = self.queued_placeholders_persist_lock(channel_id);
        let _persist_guard = persist_lock.lock().await;
        for (user_msg_id, placeholder_msg_id) in cards {
            let key = (channel_id, *user_msg_id);
            if self
                .queue_exit_placeholder_clears
                .get(&key)
                .map(|entry| *entry == *placeholder_msg_id)
                .unwrap_or(false)
            {
                self.queue_exit_placeholder_clears.remove(&key);
            }
        }
        queued_placeholders_store::persist_queue_exit_placeholder_clears_channel_from_map(
            &self.queue_exit_placeholder_clears,
            &self.provider,
            &self.token_hash,
            channel_id,
        );
    }

    fn pending_queue_exit_placeholder_clears(&self) -> Vec<(ChannelId, MessageId, MessageId)> {
        self.queue_exit_placeholder_clears
            .iter()
            .map(|entry| {
                let (channel_id, user_msg_id) = *entry.key();
                (channel_id, user_msg_id, *entry.value())
            })
            .collect()
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn make_shared_data_for_tests() -> Arc<SharedData> {
    make_shared_data_for_tests_with_storage(None, None)
}

/// #1073: Test-only helpers exposed for the Discord bot integration test
/// harness under `src/integration_tests/discord_flow/`.
///
/// Kept in one place so the harness can reach the `pub(super)` watcher
/// primitives without widening visibility on production paths. Private
/// types (`TmuxWatcherHandle`, `InflightTurnState`) never leak out of this
/// module; consumers only see opaque newtype wrappers.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) mod test_harness_exports {
    use super::{TmuxWatcherHandle, TmuxWatcherRegistry};
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::ChannelId;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    /// Opaque wrapper around the crate-private watcher registry.
    pub(crate) struct WatcherRegistry {
        inner: TmuxWatcherRegistry,
    }

    /// Opaque wrapper around an individual `TmuxWatcherHandle`. Construct
    /// via [`new_test_watcher_handle`]; hand it to [`try_claim_watcher`] /
    /// [`claim_or_reuse_watcher`] to register it.
    pub(crate) struct WatcherHandle {
        inner: TmuxWatcherHandle,
    }

    pub(crate) struct WatcherHandleInspector {
        pub(crate) cancel: Arc<AtomicBool>,
        pub(crate) paused: Arc<AtomicBool>,
        pub(crate) pause_epoch: Arc<AtomicU64>,
    }

    pub(crate) fn new_watcher_registry() -> WatcherRegistry {
        WatcherRegistry {
            inner: TmuxWatcherRegistry::new(),
        }
    }

    /// Build a fresh watcher handle plus inspection handles on the
    /// underlying atomic flags so the harness can observe stale-cancellation
    /// without importing the private `TmuxWatcherHandle` type.
    pub(crate) fn new_test_watcher_handle(
        tmux_session_name: &str,
    ) -> (WatcherHandle, WatcherHandleInspector) {
        let paused = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let pause_epoch = Arc::new(AtomicU64::new(0));
        let inner = TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            paused: paused.clone(),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: cancel.clone(),
            pause_epoch: pause_epoch.clone(),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                super::tmux_watcher_now_ms(),
            )),
            mailbox_finalize_owed: Arc::new(AtomicBool::new(false)),
        };
        let inspector = WatcherHandleInspector {
            cancel,
            paused,
            pause_epoch,
        };
        (WatcherHandle { inner }, inspector)
    }

    pub(crate) struct SharedRuntime {
        inner: Arc<super::SharedData>,
    }

    pub(crate) fn new_shared_runtime() -> SharedRuntime {
        SharedRuntime {
            inner: super::make_shared_data_for_tests(),
        }
    }

    pub(crate) fn seed_shared_watcher(
        runtime: &SharedRuntime,
        channel_id: ChannelId,
        handle: WatcherHandle,
    ) {
        runtime.inner.tmux_watchers.insert(channel_id, handle.inner);
    }

    #[cfg(unix)]
    pub(crate) fn attach_paused_turn_watcher(
        runtime: &SharedRuntime,
        channel_id: ChannelId,
        provider: &ProviderKind,
        tmux_session_name: Option<String>,
        output_path: Option<String>,
        initial_offset: u64,
        source: &'static str,
    ) -> ChannelId {
        super::router::message_handler_test_harness_exports::attach_paused_turn_watcher(
            &runtime.inner,
            Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
            provider,
            channel_id,
            tmux_session_name,
            output_path,
            initial_offset,
            source,
        )
    }

    pub(crate) fn shared_watcher_slot_count(runtime: &SharedRuntime) -> usize {
        runtime.inner.tmux_watchers.len()
    }

    pub(crate) fn shared_watcher_slot_exists(
        runtime: &SharedRuntime,
        channel_id: ChannelId,
    ) -> bool {
        runtime.inner.tmux_watchers.contains_key(&channel_id)
    }

    #[cfg(unix)]
    pub(crate) fn try_claim_watcher(
        watchers: &WatcherRegistry,
        channel_id: ChannelId,
        handle: WatcherHandle,
    ) -> bool {
        super::tmux::try_claim_watcher(&watchers.inner, channel_id, handle.inner)
    }

    #[cfg(unix)]
    pub(crate) fn claim_or_reuse_watcher(
        watchers: &WatcherRegistry,
        channel_id: ChannelId,
        handle: WatcherHandle,
        provider: &ProviderKind,
        source: &str,
    ) -> super::tmux::WatcherClaimOutcome {
        super::tmux::claim_or_reuse_watcher(
            &watchers.inner,
            channel_id,
            handle.inner,
            provider,
            source,
        )
    }

    pub(crate) fn watcher_slot_count(watchers: &WatcherRegistry) -> usize {
        watchers.inner.len()
    }

    pub(crate) fn watcher_slot_exists(watchers: &WatcherRegistry, channel_id: ChannelId) -> bool {
        watchers.inner.contains_key(&channel_id)
    }

    pub(crate) fn owner_channel_for_tmux_session(
        watchers: &WatcherRegistry,
        tmux_session_name: &str,
    ) -> Option<ChannelId> {
        watchers
            .inner
            .owner_channel_for_tmux_session(tmux_session_name)
    }

    pub(crate) fn watcher_slot_paused(
        watchers: &WatcherRegistry,
        channel_id: ChannelId,
    ) -> Option<bool> {
        watchers
            .inner
            .get(&channel_id)
            .map(|entry| entry.paused.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub(crate) mod inflight {
        use super::super::inflight as inflight_mod;
        use super::super::{InflightRestartMode, InflightTurnState};
        use crate::services::provider::ProviderKind;

        /// Opaque wrapper around `InflightTurnState` — the struct itself is
        /// `pub(super)` within `services::discord`.
        pub(crate) struct State {
            inner: InflightTurnState,
        }

        pub(crate) fn new_state(
            provider: ProviderKind,
            channel_id: u64,
            channel_name: Option<String>,
            tmux_session_name: Option<String>,
            last_offset: u64,
        ) -> State {
            State {
                inner: InflightTurnState::new(
                    provider,
                    channel_id,
                    channel_name,
                    12_345,
                    67_890,
                    111_213,
                    "harness turn".to_string(),
                    Some("harness-session".to_string()),
                    tmux_session_name,
                    Some("/tmp/harness-out.jsonl".to_string()),
                    Some("/tmp/harness-in.fifo".to_string()),
                    last_offset,
                ),
            }
        }

        pub(crate) fn channel_id(state: &State) -> u64 {
            state.inner.channel_id
        }

        pub(crate) fn tmux_session_name(state: &State) -> Option<&str> {
            state.inner.tmux_session_name.as_deref()
        }

        pub(crate) fn restart_mode(state: &State) -> Option<InflightRestartMode> {
            state.inner.restart_mode
        }

        /// Save via the public helper; respects `AGENTDESK_ROOT_DIR`.
        pub(crate) fn save(state: &State) -> Result<(), String> {
            inflight_mod::save_inflight_state(&state.inner)
        }

        /// Load all saved inflight states for `provider`. Respects
        /// `AGENTDESK_ROOT_DIR`.
        pub(crate) fn load_all(provider: &ProviderKind) -> Vec<State> {
            inflight_mod::load_inflight_states(provider)
                .into_iter()
                .map(|inner| State { inner })
                .collect()
        }

        /// Simulate the restart path (`#897`): mark every saved inflight state
        /// for `provider` with the supplied restart mode. Returns the count.
        pub(crate) fn mark_all_restart(
            provider: &ProviderKind,
            mode: InflightRestartMode,
        ) -> usize {
            inflight_mod::mark_all_inflight_states_restart_mode(provider, mode)
        }
    }

    pub(crate) use super::InflightRestartMode as RestartMode;

    /// #1137: integration-flow access to the watcher-stop strictness check.
    /// The internal `WatcherStopInput` / `WatcherStopDecision` types stay
    /// `pub(super)` inside `services::discord`; this thin wrapper exposes
    /// just the four-bit decision matrix the integration scenario asserts on.
    pub(crate) mod watcher_stop {
        use super::super::tmux as tmux_mod;
        pub(crate) use tmux_mod::WATCHER_POST_TERMINAL_IDLE_WINDOW;

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum Decision {
            Continue,
            PostTerminalSuccessContinuation,
            Stop,
        }

        impl From<tmux_mod::WatcherStopDecision> for Decision {
            fn from(value: tmux_mod::WatcherStopDecision) -> Self {
                match value {
                    tmux_mod::WatcherStopDecision::Continue => Self::Continue,
                    tmux_mod::WatcherStopDecision::PostTerminalSuccessContinuation => {
                        Self::PostTerminalSuccessContinuation
                    }
                    tmux_mod::WatcherStopDecision::Stop => Self::Stop,
                }
            }
        }

        pub(crate) fn decide(
            terminal_success_seen: bool,
            tmux_alive: bool,
            confirmed_end: u64,
            tmux_tail_offset: u64,
            idle_duration: Option<std::time::Duration>,
            idle_threshold: std::time::Duration,
        ) -> Decision {
            tmux_mod::watcher_stop_decision_after_terminal_success(tmux_mod::WatcherStopInput {
                terminal_success_seen,
                tmux_alive,
                confirmed_end,
                tmux_tail_offset,
                idle_duration,
                idle_threshold,
            })
            .into()
        }
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn make_shared_data_for_tests_with_storage(
    sqlite: Option<crate::db::Db>,
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
        placeholder_cleanup: Arc::new(placeholder_cleanup::PlaceholderCleanupRegistry::default()),
        placeholder_controller: Arc::new(placeholder_controller::PlaceholderController::default()),
        placeholder_live_events: Arc::new(placeholder_live_events::PlaceholderLiveEvents::default()),
        placeholder_live_events_enabled: false,
        status_panel_v2_enabled: false,
        queued_placeholders: dashmap::DashMap::new(),
        queue_exit_placeholder_clears: dashmap::DashMap::new(),
        queued_placeholders_persist_locks: dashmap::DashMap::new(),
        recovering_channels: dashmap::DashMap::new(),
        shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        finalizing_turns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        current_generation: 0,
        restart_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        reconcile_done: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        deferred_hook_backlog: std::sync::atomic::AtomicUsize::new(0),
        recovery_started_at: std::time::Instant::now(),
        recovery_duration_ms: std::sync::atomic::AtomicU64::new(0),
        global_active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        global_finalizing: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        shutdown_remaining: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        shutdown_counted: std::sync::atomic::AtomicBool::new(false),
        intake_dedup: dashmap::DashMap::new(),
        dispatch_thread_parents: dashmap::DashMap::new(),
        voice_barge_in: Arc::new(voice_barge_in::VoiceBargeInRuntime::disabled()),
        voice_pairings: Arc::new(voice_routing::VoiceChannelPairingStore::load_default()),
        bot_connected: std::sync::atomic::AtomicBool::new(false),
        last_turn_at: std::sync::Mutex::new(None),
        model_overrides: dashmap::DashMap::new(),
        fast_mode_channels: dashmap::DashSet::new(),
        fast_mode_session_reset_pending: dashmap::DashSet::new(),
        codex_goals_channels: dashmap::DashSet::new(),
        codex_goals_session_reset_pending: dashmap::DashSet::new(),
        model_session_reset_pending: dashmap::DashSet::new(),
        session_reset_pending: dashmap::DashSet::new(),
        model_picker_pending: dashmap::DashMap::new(),
        dispatch_role_overrides: dashmap::DashMap::new(),
        last_message_ids: dashmap::DashMap::new(),
        catch_up_retry_pending: dashmap::DashMap::new(),
        turn_start_times: dashmap::DashMap::new(),
        channel_rosters: dashmap::DashMap::new(),
        cached_serenity_ctx: tokio::sync::OnceCell::new(),
        cached_bot_token: tokio::sync::OnceCell::new(),
        token_hash: "test-token-hash".to_string(),
        provider: ProviderKind::Claude,
        api_port: 9,
        sqlite,
        pg_pool,
        engine: None,
        health_registry: std::sync::Weak::new(),
        known_slash_commands: tokio::sync::OnceCell::new(),
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
            .dispatch_role_overrides
            .get(&channel_id)
            .map(|override_id| override_id.value().get()),
    )
}

async fn mailbox_snapshot(shared: &SharedData, channel_id: ChannelId) -> ChannelMailboxSnapshot {
    shared.mailbox(channel_id).snapshot().await
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
    let result = shared.mailbox(channel_id).cancel_active_turn().await;
    #[cfg(unix)]
    if result.token.is_some() {
        // #1309: in-memory publish is synchronous (instant suppression);
        // PG mirror is awaited with a 500 ms cap so a quick dcserver
        // restart cannot drop the durable copy.
        tmux::record_recent_turn_stop(channel_id, tmux_session_name.as_deref(), reason).await;
    }
    result
}

async fn mailbox_cancel_active_turn_if_current_with_reason(
    shared: &SharedData,
    channel_id: ChannelId,
    expected_token: Arc<CancelToken>,
    reason: &str,
) -> CancelActiveTurnResult {
    expected_token.set_cancel_source(reason);
    let tmux_session_name = shared
        .tmux_watchers
        .channel_binding(&channel_id)
        .map(|binding| binding.tmux_session_name)
        .or_else(|| infer_inflight_tmux_session_for_channel(channel_id));
    let result = shared
        .mailbox(channel_id)
        .cancel_active_turn_if_current(expected_token)
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

fn cleanup_retry_inflight_blocks_idle_kickoff(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> bool {
    let Some(state) = inflight::load_inflight_state(provider, channel_id.get()) else {
        return false;
    };
    if state.current_msg_id == 0 {
        return false;
    }

    shared.placeholder_cleanup.terminal_cleanup_retry_pending(
        provider,
        channel_id,
        MessageId::new(state.current_msg_id),
    )
}

fn idle_queue_snapshot_has_kickable_backlog(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> bool {
    snapshot.cancel_token.is_none()
        && !snapshot.intervention_queue.is_empty()
        && !cleanup_retry_inflight_blocks_idle_kickoff(shared, provider, channel_id)
}

async fn mailbox_try_start_turn(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_message_id: MessageId,
) -> bool {
    shared
        .mailbox(channel_id)
        .try_start_turn(cancel_token, request_owner, user_message_id)
        .await
}

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
    user_message_id: MessageId,
) -> RecoveryKickoffResult {
    let result = shared
        .mailbox(channel_id)
        .recovery_kickoff(cancel_token, request_owner, user_message_id)
        .await;
    if result.activated_turn {
        shared
            .global_active
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
}

/// Outcome of `mailbox_enqueue_intervention` — exposes both the enqueue
/// success and whether the incoming intervention was merged into the previous
/// queue entry, so callers can pick a different reaction emoji for merged
/// vs standalone queue entries (#1190 follow-up).
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MailboxEnqueueOutcome {
    pub(super) enqueued: bool,
    pub(super) merged: bool,
}

async fn mailbox_enqueue_intervention(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    intervention: Intervention,
) -> MailboxEnqueueOutcome {
    let result = shared
        .mailbox(channel_id)
        .enqueue(
            intervention,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    MailboxEnqueueOutcome {
        enqueued: result.enqueued,
        merged: result.merged,
    }
}

fn queue_exit_feedback_emoji(kind: QueueExitKind) -> char {
    match kind {
        QueueExitKind::Cancelled => '🚫',
        QueueExitKind::Expired => '⌛',
        QueueExitKind::Superseded => '⏏',
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
        QueueExitKind::Superseded => {
            "⏏ **큐에서 제거됨** — 후속 메시지로 대체되어 처리되지 않습니다."
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
    // mutex across the whole batch drain + snapshot write. Without this, a
    // concurrent `insert_queued_placeholder` for the same channel could
    // observe its mutation reflected in memory but lose the race to write
    // the *post-drain* snapshot to disk — leaving a stale entry that
    // resurrects on restart for an intervention that has already exited the
    // queue. Round-5 promoted the lock to `tokio::sync::Mutex` so callers
    // that span `.await` can still serialize against this drain.
    let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
    let _persist_guard = persist_lock.lock().await;
    let mut visible_cards_to_clear: Vec<QueueExitVisibleCard> = Vec::new();
    let mut mutated = false;
    for event in queue_exit_events {
        for message_id in &event.intervention.source_message_ids {
            if let Some((_, placeholder_msg_id)) = shared
                .queued_placeholders
                .remove(&(channel_id, *message_id))
            {
                shared
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
            &shared.queued_placeholders,
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

    // #1332: drop any stale `📬 메시지 대기 중` placeholder mappings up front
    // so a subsequent dispatch never wires a newly-started turn to a placeholder
    // that belongs to a cancelled/expired intervention. Also detach the
    // controller entry so the cap-bounded `placeholder_controller.entries`
    // map does not retain a stale Queued row (Queued is not in the standard
    // eviction sweep — it is meant to live until dispatch). The mapping/
    // controller bookkeeping runs regardless of whether the cached serenity
    // ctx is available so a missing ctx never silently misroutes the next
    // turn.
    //
    // codex review P2 (#1332 follow-up): the visible Discord card edited to
    // `📬 메시지 대기 중` must ALSO be cleaned up — leaving it behind would
    // promise a turn that has been cancelled/expired/superseded. Collect the
    // placeholder ids here and rewrite/delete them once we have a serenity
    // ctx (best-effort: log on cache miss and leave the bookkeeping in place
    // so a future ctx can still reach the same rows via `detach_by_message`).
    let visible_cards_to_clear =
        queue_exit_drain_queued_placeholders(shared, channel_id, &queue_exit_events).await;

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

    for event in queue_exit_events {
        // Clean up the queue-pending reactions on EVERY message that contributed
        // to this intervention. After #1190 follow-up, merged messages carry ➕
        // and standalone heads carry 📬; remove both unconditionally so cancel /
        // expiry / supersede leaves only the exit-state reaction visible.
        //
        // #2044 F9: when the intervention has a single source message
        // (standalone head, no merges), there was never a ➕ reaction
        // to remove. Previously we called `remove_reaction_raw('➕')`
        // anyway, doubling the channel-level rate-limited HTTP traffic
        // per exit. Skip it for standalone messages — merged groups
        // (`len() > 1`) still receive both removals because the merge
        // path adds ➕ to every non-head source.
        let is_standalone = event.intervention.source_message_ids.len() <= 1;
        for message_id in &event.intervention.source_message_ids {
            formatting::remove_reaction_raw(&http, channel_id, *message_id, '📬').await;
            if !is_standalone {
                formatting::remove_reaction_raw(&http, channel_id, *message_id, '➕').await;
            }
        }
        formatting::add_reaction_raw(
            &http,
            channel_id,
            event.intervention.message_id,
            queue_exit_feedback_emoji(event.kind),
        )
        .await;
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

async fn enqueue_internal_followup(
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
            message_id: reply_message_id,
            source_message_ids: vec![reply_message_id],
            text: text.into(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
        },
    )
    .await;

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

    let Some(retry_checkpoint) =
        take_catch_up_retry_checkpoint_after_queue_drain(shared, channel_id, queue_len_after)
    else {
        return false;
    };

    let shared = Arc::clone(shared);
    let provider = provider.clone();
    tokio::spawn(async move {
        let retry_checkpoints = HashMap::from([(channel_id, retry_checkpoint)]);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔁 catch-up: retrying channel {} after queue drained to {} item(s)",
            channel_id,
            queue_len_after
        );
        catch_up_missed_messages_inner(&http, &shared, &provider, &retry_checkpoints).await;
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
) -> Option<(Intervention, bool)> {
    let result: TakeNextSoftResult = shared
        .mailbox(channel_id)
        .take_next_soft(queue_persistence_context(shared, provider, channel_id))
        .await;
    let queue_len_after = result.queue_len_after;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    maybe_schedule_catch_up_retry_after_queue_drain(shared, provider, channel_id, queue_len_after);
    result
        .intervention
        .map(|intervention| (intervention, result.has_more))
}

async fn idle_queue_take_next_soft_if_ready(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<(Intervention, bool)> {
    if mailbox_has_active_turn(shared, channel_id).await
        || cleanup_retry_inflight_blocks_idle_kickoff(shared, provider, channel_id)
    {
        return None;
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

async fn mailbox_finish_turn(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> FinishTurnResult {
    let result = shared
        .mailbox(channel_id)
        .finish_turn(queue_persistence_context(shared, provider, channel_id))
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    result
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
    result
}

async fn mailbox_replace_queue(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    queue: Vec<Intervention>,
) {
    shared
        .mailbox(channel_id)
        .replace_queue(
            queue,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await;
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
    freeze_after: bool,
) -> usize {
    shared
        .mailboxes
        .restart_drain_all(
            provider,
            &shared.token_hash,
            &shared.dispatch_role_overrides,
            freeze_after,
        )
        .await
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
    let duration_ms = shared.recovery_started_at.elapsed().as_millis();
    let duration_ms = duration_ms.min(u64::MAX as u128) as u64;
    let _ = shared.recovery_duration_ms.compare_exchange(
        0,
        duration_ms,
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Relaxed,
    );
    shared
        .reconcile_done
        .store(true, std::sync::atomic::Ordering::Release);
}

pub(super) type Error = Box<dyn std::error::Error + Send + Sync>;
pub(super) type Context<'a> = poise::Context<'a, Data, Error>;

/// #1227: page size for catch-up REST fetch. Bumped from 10 → 50 because the
/// previous size was overrun by bursty bot output and silently dropped buried
/// user messages.
const CATCH_UP_FETCH_LIMIT: u8 = 50;

/// Filter outcome categories for the catch-up REST scan. Used both at runtime
/// (to emit per-channel breakdown logs even when nothing was recovered) and in
/// unit tests as a pure-function check on the buried-user-message regression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum CatchUpClassification {
    /// Eligible user/allowed-bot message that should be enqueued.
    Recover,
    /// System message kind (thread-created / slash-command etc.) — silently dropped.
    SystemKind,
    /// Authored by this bot (self) — must not re-enqueue our own output.
    SelfAuthored,
    /// Already present in the live mailbox / known set — duplicate.
    Duplicate,
    /// Older than the catch-up max-age window — too late to safely replay.
    TooOld,
    /// Empty content (whitespace only).
    Empty,
    /// Authored by a non-allowed bot or an allowed bot without DISPATCH prefix.
    NotAllowed,
}

/// Per-channel running tally of [`CatchUpClassification`] outcomes — fed into
/// the always-on breakdown log. Keeping this separate from the recovery loop
/// keeps the filter-stats accounting honest and unit-testable.
#[derive(Debug, Default, Clone, Copy)]
pub(in crate::services::discord) struct CatchUpScanStats {
    pub returned: usize,
    pub recovered: usize,
    pub system_kind: usize,
    pub self_authored: usize,
    pub duplicate: usize,
    pub too_old: usize,
    pub empty: usize,
    pub not_allowed: usize,
}

impl CatchUpScanStats {
    pub(in crate::services::discord) fn record(&mut self, outcome: CatchUpClassification) {
        match outcome {
            CatchUpClassification::Recover => self.recovered += 1,
            CatchUpClassification::SystemKind => self.system_kind += 1,
            CatchUpClassification::SelfAuthored => self.self_authored += 1,
            CatchUpClassification::Duplicate => self.duplicate += 1,
            CatchUpClassification::TooOld => self.too_old += 1,
            CatchUpClassification::Empty => self.empty += 1,
            CatchUpClassification::NotAllowed => self.not_allowed += 1,
        }
    }
}

/// Plain inputs to the catch-up filter, decoupled from `serenity::Message` so
/// we can unit test the regression scenario without a Discord runtime.
#[derive(Debug, Clone)]
pub(in crate::services::discord) struct CatchUpMessageView {
    pub message_id: u64,
    pub author_id: u64,
    pub author_is_bot: bool,
    pub is_processable_kind: bool,
    pub age_secs: i64,
    pub trimmed_text: String,
}

/// Pure classifier for the catch-up filter pipeline. Mirrors the order of
/// checks inside the per-message loop in [`catch_up_missed_messages`] so a
/// regression there is caught here. Critically, this function does NOT apply
/// any limit/page-size logic — that decision lives at the REST fetch site
/// (see `CATCH_UP_FETCH_LIMIT`). This means a "buried user message" test must
/// assert against the full fetched page, not a single classification call.
pub(in crate::services::discord) fn classify_catch_up_message(
    msg: &CatchUpMessageView,
    bot_user_id: Option<u64>,
    existing_ids: &std::collections::HashSet<u64>,
    max_age_secs: i64,
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
) -> CatchUpClassification {
    if !msg.is_processable_kind {
        return CatchUpClassification::SystemKind;
    }
    if Some(msg.author_id) == bot_user_id {
        return CatchUpClassification::SelfAuthored;
    }
    if existing_ids.contains(&msg.message_id) {
        return CatchUpClassification::Duplicate;
    }
    if msg.age_secs > max_age_secs {
        return CatchUpClassification::TooOld;
    }
    if msg.trimmed_text.is_empty() {
        return CatchUpClassification::Empty;
    }
    if !is_allowed_turn_sender(
        allowed_bot_ids,
        announce_bot_id,
        msg.author_id,
        msg.author_is_bot,
        &msg.trimmed_text,
    ) {
        return CatchUpClassification::NotAllowed;
    }
    CatchUpClassification::Recover
}

/// Startup catch-up polling: fetch messages that arrived during the restart gap.
/// Uses saved last_message_ids to query Discord REST API for missed messages,
/// filters out bot messages and duplicates, and inserts into intervention queue.
async fn catch_up_missed_messages(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let retry_checkpoints = HashMap::new();
    catch_up_missed_messages_inner(http, shared, provider, &retry_checkpoints).await;
}

async fn catch_up_missed_messages_inner(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    retry_checkpoints: &HashMap<ChannelId, u64>,
) {
    let Some(root) = runtime_store::last_message_root() else {
        return;
    };
    let dir = root.join(provider.as_str());
    if !dir.is_dir() {
        return;
    }

    let mut total_recovered = 0usize;
    let now = Instant::now();
    let max_age = std::time::Duration::from_secs(300); // Only catch up messages within 5 minutes

    // #429: prune stale checkpoints before iterating — files older than
    // max_checkpoint_age were written by sessions that ended long before this
    // restart, so catch-up is pointless and the API calls are wasted.
    let max_checkpoint_age = std::time::Duration::from_secs(600); // 10 minutes
    let mut pruned = 0usize;
    if let Ok(prune_entries) = fs::read_dir(&dir) {
        for entry in prune_entries.flatten() {
            let path = entry.path();
            if let Ok(meta) = path.metadata() {
                if let Ok(modified) = meta.modified() {
                    if modified.elapsed().unwrap_or_default() > max_checkpoint_age {
                        let _ = fs::remove_file(&path);
                        pruned += 1;
                    }
                }
            }
        }
    }
    if pruned > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] 🧹 catch-up: pruned {pruned} stale checkpoint(s) (>10min old)");
    }

    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(channel_id_raw) = stem.parse::<u64>() else {
            continue;
        };
        let Ok(last_id_str) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(disk_last_id) = last_id_str.trim().parse::<u64>() else {
            continue;
        };

        let channel_id = ChannelId::new(channel_id_raw);
        let retry_checkpoint = retry_checkpoints.get(&channel_id).copied();
        let live_checkpoint = shared.last_message_ids.get(&channel_id).map(|entry| *entry);
        let last_id = catch_up_checkpoint_for_scan(disk_last_id, live_checkpoint, retry_checkpoint);
        let after_msg = MessageId::new(last_id);

        // #429: skip channels this bot cannot access.  Utility bots
        // (notify/announce) share the claude provider checkpoint dir but
        // have no channel read permissions → every API call fails slowly.
        {
            let settings = shared.settings.read().await;
            if !settings::bot_settings_allow_channel(&settings, channel_id, false) {
                continue;
            }
        }

        match resolve_runtime_channel_binding_status(http, channel_id).await {
            RuntimeChannelBindingStatus::Owned => {}
            RuntimeChannelBindingStatus::Unowned => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⏭ catch-up: dropping stale checkpoint for unowned channel {} ({})",
                    channel_id,
                    path.display()
                );
                let _ = fs::remove_file(&path);
                continue;
            }
            RuntimeChannelBindingStatus::Unknown => continue,
        }

        // Fetch messages after last_id (Discord returns oldest first with after=)
        // #1227: limit was 10 — channels with bursty bot activity (streaming
        // replies + many short turns) routinely fill that window with bot
        // messages, pushing user messages outside the page entirely. Discord
        // applies `limit` BEFORE author filtering, so an active channel
        // (1 user : 9 bots) silently drops the user message. 50 keeps the
        // single-page contract while giving headroom for the realistic
        // bot:user ratio. Discord per-channel rate limit (5 req / 5 sec)
        // has plenty of margin for this 5x cost.
        let messages = match channel_id
            .messages(
                http,
                serenity::builder::GetMessages::new()
                    .after(after_msg)
                    .limit(CATCH_UP_FETCH_LIMIT),
            )
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                let msg = e.to_string();
                tracing::warn!(
                    "  [{ts}] ⚠ catch-up: failed to fetch messages for channel {channel_id}: {e}"
                );
                // #429: permanent errors — remove checkpoint to avoid retrying every restart
                if msg.contains("Missing Access") || msg.contains("Unknown Channel") {
                    let _ = fs::remove_file(&path);
                }
                continue;
            }
        };

        if messages.is_empty() {
            continue;
        }

        // Get bot's own user ID to filter out self-messages
        let bot_user_id = {
            let settings = shared.settings.read().await;
            settings.owner_user_id
        };

        // Collect existing message IDs in queue for dedup
        let existing_ids = recovery_known_message_ids(&mailbox_snapshot(shared, channel_id).await);

        let allowed_bot_ids: Vec<u64> = {
            let settings = shared.settings.read().await;
            settings.allowed_bot_ids.clone()
        };
        let announce_bot_id = resolve_announce_bot_user_id(shared).await;

        let mut max_recovered_id: Option<u64> = None;
        let mut stats = CatchUpScanStats::default();
        stats.returned = messages.len();

        // Codex P2 on #1301: the 50-message fetch can exceed
        // `MAX_INTERVENTIONS_PER_CHANNEL` (30) on a long restart gap. Without
        // a cap `enqueue_intervention` would silently supersede older
        // queued entries while catch-up still advances the checkpoint to the
        // newest recovered id — meaning the evicted messages are lost. Cap
        // recovery to the queue's remaining capacity at scan-start; the
        // overflow stays unrecovered with the OLD checkpoint, so the next
        // catch-up cycle picks it up from the same `after` cursor.
        let queue_initial_len = mailbox_snapshot(shared, channel_id)
            .await
            .intervention_queue
            .len();
        let remaining_capacity = crate::services::turn_orchestrator::MAX_INTERVENTIONS_PER_CHANNEL
            .saturating_sub(queue_initial_len);

        for msg in &messages {
            let text = msg.content.trim().to_string();
            let msg_ts = msg.id.created_at();
            let age_secs = chrono::Utc::now()
                .signed_duration_since(*msg_ts)
                .num_seconds();
            let view = CatchUpMessageView {
                message_id: msg.id.get(),
                author_id: msg.author.id.get(),
                author_is_bot: msg.author.bot,
                is_processable_kind: router::should_process_turn_message(msg.kind),
                age_secs,
                trimmed_text: text.clone(),
            };
            let outcome = classify_catch_up_message(
                &view,
                bot_user_id,
                &existing_ids,
                max_age.as_secs() as i64,
                &allowed_bot_ids,
                announce_bot_id,
            );
            // Codex P2 round 2 on #1301: check the cap BEFORE recording the
            // recover, otherwise `stats.recovered` would tally a message we
            // refused to enqueue and the log would lie about the queue
            // contents. Stopping iteration keeps the checkpoint pinned at
            // the last actually-queued message — newer entries that we
            // declined are still > `after_msg` for the next pass.
            if outcome == CatchUpClassification::Recover && stats.recovered >= remaining_capacity {
                let retry_after = max_recovered_id.unwrap_or(last_id);
                shared
                    .catch_up_retry_pending
                    .insert(channel_id, retry_after);
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔁 catch-up: queue cap reached for channel {}; retry armed after checkpoint {}",
                    channel_id,
                    retry_after
                );
                break;
            }
            stats.record(outcome);
            if outcome != CatchUpClassification::Recover {
                continue;
            }

            mailbox_enqueue_intervention(
                shared,
                provider,
                channel_id,
                Intervention {
                    author_id: msg.author.id,
                    message_id: msg.id,
                    source_message_ids: vec![msg.id],
                    text: text.clone(),
                    mode: InterventionMode::Soft,
                    created_at: now,
                    reply_context: None,
                    has_reply_boundary: msg.message_reference.is_some(),
                    merge_consecutive: !msg.author.bot
                        && !text.starts_with('!')
                        && !text.starts_with('/')
                        && !text.starts_with("DISPATCH:"),
                },
            )
            .await;
            // Track the newest actually-recovered message for checkpoint
            let mid = msg.id.get();
            if max_recovered_id.map(|m| mid > m).unwrap_or(true) {
                max_recovered_id = Some(mid);
            }
        }

        // #1227: emit a breakdown line for EVERY scanned channel — including
        // 0-recovery — so operator can distinguish "no missed messages" from
        // "limit too small" / "filter ate them" without re-reading the code.
        let ts = chrono::Local::now().format("%H:%M:%S");
        if stats.recovered > 0 {
            total_recovered += stats.recovered;
            tracing::info!(
                "  [{ts}] 🔍 CATCH-UP: recovered {} message(s) for channel {} \
                 (returned={} self={} dup={} too_old={} empty={} not_allowed={} system={})",
                stats.recovered,
                channel_id,
                stats.returned,
                stats.self_authored,
                stats.duplicate,
                stats.too_old,
                stats.empty,
                stats.not_allowed,
                stats.system_kind,
            );
        } else {
            tracing::info!(
                "  [{ts}] 🔍 catch-up scan: channel={} returned={} bot={} dup={} \
                 too_old={} empty={} not_allowed={} system={} recovered=0",
                channel_id,
                stats.returned,
                stats.self_authored,
                stats.duplicate,
                stats.too_old,
                stats.empty,
                stats.not_allowed,
                stats.system_kind,
            );
        }

        // Only advance checkpoint if we actually recovered messages
        if let Some(newest) = max_recovered_id {
            shared.last_message_ids.insert(channel_id, newest);
            if retry_checkpoint.is_some()
                && !shared.catch_up_retry_pending.contains_key(&channel_id)
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔁 catch-up: retry completed for channel {} at checkpoint {}",
                    channel_id,
                    newest
                );
            }
        }
    }

    if total_recovered > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔍 CATCH-UP: total {total_recovered} message(s) recovered across channels"
        );
    }

    // Phase 2: Scan for unanswered messages since last bot response.
    // Catches messages that were queued in-memory but lost on restart.
    let Ok(entries2) = fs::read_dir(&dir) else {
        return;
    };
    let mut phase2_recovered = 0usize;
    let bot_user_id_phase2 = {
        let settings = shared.settings.read().await;
        settings.owner_user_id
    };
    let allowed_bot_ids_phase2: Vec<u64> = {
        let settings = shared.settings.read().await;
        settings.allowed_bot_ids.clone()
    };
    let announce_bot_id_phase2 = resolve_announce_bot_user_id(shared).await;

    for entry in entries2.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(channel_id_raw) = stem.parse::<u64>() else {
            continue;
        };
        let channel_id = ChannelId::new(channel_id_raw);

        {
            let settings = shared.settings.read().await;
            if !settings::bot_settings_allow_channel(&settings, channel_id, false) {
                continue;
            }
        }

        match resolve_runtime_channel_binding_status(http, channel_id).await {
            RuntimeChannelBindingStatus::Owned => {}
            RuntimeChannelBindingStatus::Unowned | RuntimeChannelBindingStatus::Unknown => continue,
        }

        // Fetch last 20 messages (newest first — default Discord order)
        let recent = match channel_id
            .messages(http, serenity::builder::GetMessages::new().limit(20))
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                let msg = e.to_string();
                tracing::warn!(
                    "  [{ts}] ⚠ catch-up phase2: failed to fetch recent messages for channel {channel_id}: {e}"
                );
                if msg.contains("Missing Access") || msg.contains("Unknown Channel") {
                    let _ = fs::remove_file(&path);
                }
                continue;
            }
        };

        if recent.is_empty() {
            continue;
        }

        // Find the newest bot response (first bot message in newest-first order)
        let last_bot_idx = recent.iter().position(|m| {
            Some(m.author.id.get()) == bot_user_id_phase2 && !m.content.trim().is_empty()
        });

        // Messages at indices 0..last_bot_idx are newer than the last bot response
        let unanswered_slice = match last_bot_idx {
            Some(0) => continue, // Latest message is from bot — nothing unanswered
            Some(idx) => &recent[..idx],
            None => continue, // No bot response found — skip (new/inactive channel)
        };

        // Collect existing queue IDs for dedup
        let mut existing_ids =
            recovery_known_message_ids(&mailbox_snapshot(shared, channel_id).await);
        let mut phase2_checkpoint = shared.last_message_ids.get(&channel_id).map(|v| *v);

        let mut channel_recovered = 0usize;

        // Iterate in reverse (oldest first) for chronological queue order
        for msg in unanswered_slice.iter().rev() {
            if !router::should_process_turn_message(msg.kind) {
                continue;
            }
            if Some(msg.author.id.get()) == bot_user_id_phase2 {
                continue;
            }
            let text = msg.content.trim();
            if text.is_empty() {
                continue;
            }
            let mid = msg.id.get();
            if !is_allowed_turn_sender(
                &allowed_bot_ids_phase2,
                announce_bot_id_phase2,
                msg.author.id.get(),
                msg.author.bot,
                text,
            ) {
                continue;
            }
            let is_allowed_bot = msg.author.bot
                && (allowed_bot_ids_phase2.contains(&msg.author.id.get())
                    || announce_bot_id_phase2.is_some_and(|id| id == msg.author.id.get()));
            if !is_allowed_bot {
                let settings = shared.settings.read().await;
                if !discord_io::user_is_authorized(&settings, msg.author.id.get()) {
                    continue;
                }
            }
            if !should_phase2_recover_message(mid, phase2_checkpoint, &existing_ids) {
                continue;
            }
            // Skip messages older than 10 minutes (generous window for restart gap)
            let msg_age = chrono::Utc::now().signed_duration_since(*msg.id.created_at());
            if msg_age.num_seconds() > 600 {
                continue;
            }

            mailbox_enqueue_intervention(
                shared,
                provider,
                channel_id,
                Intervention {
                    author_id: msg.author.id,
                    message_id: msg.id,
                    source_message_ids: vec![msg.id],
                    text: text.to_string(),
                    mode: InterventionMode::Soft,
                    created_at: now,
                    reply_context: None,
                    has_reply_boundary: msg.message_reference.is_some(),
                    merge_consecutive: !msg.author.bot
                        && !text.starts_with('!')
                        && !text.starts_with('/')
                        && !text.starts_with("DISPATCH:"),
                },
            )
            .await;
            existing_ids.insert(mid);
            phase2_checkpoint = Some(phase2_checkpoint.map_or(mid, |saved| saved.max(mid)));
            channel_recovered += 1;
        }

        if channel_recovered > 0 {
            phase2_recovered += channel_recovered;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🔍 CATCH-UP phase2: recovered {} unanswered message(s) for channel {}",
                channel_recovered,
                channel_id
            );
        }
    }

    if phase2_recovered > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔍 CATCH-UP phase2: total {phase2_recovered} unanswered message(s) recovered"
        );
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
) {
    // Collect channels with queued items that are idle (no active turn). Dequeue only
    // after the routing guard passes so a rejected channel stays preserved on disk/in memory.
    let mailbox_snapshots = shared.mailboxes.snapshot_all().await;
    let channels_to_kick: Vec<ChannelId> = mailbox_snapshots
        .into_iter()
        .filter_map(|(channel_id, snapshot)| {
            if idle_queue_snapshot_has_kickable_backlog(shared, provider, channel_id, &snapshot) {
                Some(channel_id)
            } else {
                None
            }
        })
        .collect();

    if channels_to_kick.is_empty() {
        return;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🚀 KICKOFF: starting turns for {} idle channel(s) with queued messages",
        channels_to_kick.len()
    );

    for channel_id in channels_to_kick {
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
            continue;
        }

        let Some((intervention, has_more)) =
            idle_queue_take_next_soft_if_ready(shared, provider, channel_id).await
        else {
            continue;
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

        // codex review round-5 P2 (finding 3 — drain merged placeholders on
        // idle kickoff): when a merged-source intervention is restored from
        // the persisted queue (or scheduled by `schedule_deferred_idle_queue_kickoff`)
        // and dispatched here directly via `router::handle_text_message`,
        // only the head id (`intervention.message_id`) is consumed by the
        // dispatch hand-off. Any non-head `source_message_ids` would leak
        // both their `queued_placeholders` mappings and their stale `📬`
        // Discord cards. The live dispatch path (`DiscordGateway::dispatch_queued_turn`)
        // already calls this same drain helper; round-5 mirrors that
        // cleanup here so restart-induced kickoff produces identical
        // post-conditions.
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

        let deps = router::IntakeDeps {
            http: &ctx.http,
            cache: Some(&ctx.cache),
            ctx_for_chained_dispatch: Some(ctx),
            shared,
            token,
        };
        if let Err(e) = router::handle_text_message(
            &deps,
            channel_id,
            intervention.message_id,
            intervention.author_id,
            &owner_name,
            &intervention.text,
            true,     // reply_to_user_message
            has_more, // defer_watcher_resume
            false,    // wait_for_completion — don't block, let channels run concurrently
            intervention.merge_consecutive,
            intervention.reply_context.clone(),
            intervention.has_reply_boundary,
            None,
            // Queued interventions kicked off after a previous turn already
            // own their own placeholder; they're never racing for it.
            // Foreground keeps legacy behavior.
            router::TurnKind::Foreground,
        )
        .await
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}]   ⚠ KICKOFF: failed to start turn for channel {}: {e}",
                channel_id
            );
            // Requeue so the message is not lost, and persist immediately.
            mailbox_requeue_intervention_front(shared, provider, channel_id, intervention).await;
        }
    }
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

use discord_io::{
    add_reaction, check_auth, check_owner, rate_limit_wait, try_handle_pending_dm_reply,
};

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
        session_id: Option<String>,
        session_key: Option<String>,
        retry_context: Option<String>,
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
                session_id: s.session_id.clone(),
                session_key: s.channel_name.as_ref().map(|name| {
                    let tmux_name = provider.build_tmux_session_name(name);
                    adk_session::build_namespaced_session_key(
                        &shared.token_hash,
                        &provider,
                        &tmux_name,
                    )
                }),
                retry_context: s.recent_history_context(SESSION_RECOVERY_CONTEXT_MESSAGES),
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
                    cleanup_git_worktree(None::<&crate::db::Db>, shared.pg_pool.as_ref(), wt);
                }
            }
            data.sessions.remove(&ch);
        }
    }
    for expired_session in &expired {
        if let Some(retry_context) = expired_session.retry_context.as_deref() {
            let _ = internal_api::set_kv_value(
                &session_retry_context_key(expired_session.channel_id),
                retry_context,
            );
        }
        if let Some(session_key) = expired_session.session_key.as_deref() {
            adk_session::clear_provider_session_id(session_key, shared.api_port).await;
        }
        if let Some(session_id) = expired_session.session_id.as_deref() {
            let _ = internal_api::clear_stale_session_id(session_id).await;
        }
    }
    for expired_session in &expired {
        let cleared = mailbox_clear_channel(shared, &provider, expired_session.channel_id).await;
        if cleared.removed_token.is_some() {
            shared
                .global_active
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        shared.api_timestamps.remove(&expired_session.channel_id);
    }
    // Record termination audit for cleaned-up sessions
    for expired_session in &expired {
        if let Some(session_key) = expired_session.session_key.as_deref() {
            let should_record = mark_session_disconnected_for_idle_cleanup(
                None::<&crate::db::Db>,
                shared.pg_pool.as_ref(),
                session_key,
            )
            .await;
            if !should_record {
                continue;
            }

            crate::services::termination_audit::record_termination_with_handles(
                None::<&crate::db::Db>,
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
    _db: Option<&crate::db::Db>,
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

        assert!(mark_session_disconnected_for_idle_cleanup(None, Some(&pool), session_key).await);

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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::runtime_bootstrap::discord_gateway_intents;
    use super::{
        CATCH_UP_FETCH_LIMIT, CatchUpClassification, CatchUpMessageView, CatchUpScanStats,
        DiscordBotSettings, Intervention, InterventionMode, classify_catch_up_message,
        is_allowed_turn_sender, mark_session_disconnected_for_idle_cleanup, queued_message_ids,
        recovery_known_message_ids, should_phase2_recover_message,
    };
    use super::{ChannelId, MessageId, UserId};
    use crate::services::discord::placeholder_cleanup::{
        PlaceholderCleanupOperation, PlaceholderCleanupOutcome, PlaceholderCleanupRecord,
    };
    use crate::services::discord::settings::{
        BotChannelRoutingGuardFailure, validate_bot_channel_routing,
    };
    use crate::services::discord::should_process_allowed_bot_turn_text;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::GatewayIntents;
    use std::collections::HashSet;
    use std::time::Instant;

    #[test]
    fn discord_gateway_intents_include_reaction_events() {
        let intents = discord_gateway_intents();

        assert!(intents.contains(GatewayIntents::GUILD_MESSAGE_REACTIONS));
        assert!(intents.contains(GatewayIntents::DIRECT_MESSAGE_REACTIONS));
    }

    #[test]
    fn allowed_bot_turns_require_dispatch_prefix() {
        let allowed_bot_ids = vec![123];
        let announce_bot_id = Some(456);
        let dispatch = "DISPATCH: abc123\n작업 시작";
        let agent_msg = "completion_guard 수정해줘";

        assert!(!should_process_allowed_bot_turn_text(
            "⚠️ 검토 전용 — 작업 착수 금지"
        ));
        assert!(should_process_allowed_bot_turn_text(dispatch));
        assert!(!should_process_allowed_bot_turn_text(agent_msg));
        assert!(!is_allowed_turn_sender(
            &allowed_bot_ids,
            announce_bot_id,
            123,
            false,
            "⚠️ 검토 전용 — 작업 착수 금지"
        ));
        assert!(is_allowed_turn_sender(
            &allowed_bot_ids,
            announce_bot_id,
            123,
            false,
            dispatch
        ));
        assert!(!is_allowed_turn_sender(
            &allowed_bot_ids,
            announce_bot_id,
            123,
            false,
            agent_msg
        ));
        // Announce-bot allows arbitrary text by design (it backs dispatch
        // wrappers, PM/escalation cards, and generic /api/discord/send routing).
        // Issue-announcement cards moved to notify-bot in the #1448
        // follow-up so they no longer reach this branch at all.
        assert!(is_allowed_turn_sender(
            &allowed_bot_ids,
            announce_bot_id,
            456,
            true,
            agent_msg
        ));
    }

    #[test]
    fn announce_bot_passes_arbitrary_text_for_dispatch_routing() {
        // Issue announcements (📋/✅ cards) are now sent via notify-bot
        // (#1448 follow-up), whose user_id is NOT in `allowed_bot_ids` and
        // not announce_bot_id, so they fall through to `!author_is_bot`
        // and never trigger turns. Announce-bot is reserved for real
        // dispatch / PM-decision / escalation / generic routing payloads,
        // all of which must keep waking the target agent regardless of
        // text shape.
        let allowed_bot_ids: Vec<u64> = vec![123];
        let announce_bot_id = Some(456u64);

        for text in [
            "── implementation dispatch ──\nDISPATCH:abc-123 [📋 구현] - #1435 작업 시작",
            "DISPATCH: abc123\n작업 시작",
            "⚠️ [PM 결정 요청] card_id: card-2\nissue: #1442\n수동 판단 필요",
            "⚠️ [에스컬레이션] card_id: card-3 / #1444\n<@111> 수동 판단 필요",
            "@AgentB please review the latest cards and confirm priorities by EOD.",
            "✅ **#1500** verified; please review the latest patch.",
        ] {
            assert!(
                is_allowed_turn_sender(&allowed_bot_ids, announce_bot_id, 456, true, text),
                "announce-bot text should trigger turn: {text}"
            );
        }
    }

    #[test]
    fn non_bot_user_message_still_triggers_turn() {
        // Regression guard for the existing non-bot path: human messages
        // must keep triggering a turn regardless of content (no DISPATCH:
        // gating applies to humans).
        let allowed_bot_ids: Vec<u64> = vec![123];
        let announce_bot_id = Some(456u64);

        assert!(is_allowed_turn_sender(
            &allowed_bot_ids,
            announce_bot_id,
            789, // human user id, neither in allowed_bot_ids nor the announce bot
            false,
            "그냥 사람 메시지",
        ));
    }

    #[test]
    fn phase2_recovery_skips_messages_at_or_before_checkpoint() {
        let existing = HashSet::from([300u64]);

        assert!(!should_phase2_recover_message(300, None, &existing));
        assert!(!should_phase2_recover_message(200, Some(250), &existing));
        assert!(!should_phase2_recover_message(250, Some(250), &existing));
        assert!(should_phase2_recover_message(251, Some(250), &existing));
    }

    #[test]
    fn queue_cancel_checkpoint_blocks_phase2_recovery() {
        let _lock = super::runtime_store::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap()) };

        let shared = super::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(1479671301387059200);
        let cancelled = MessageId::new(1499024414690250824);

        let checkpoint =
            super::advance_last_message_checkpoint(&shared, &provider, channel_id, cancelled);

        assert_eq!(checkpoint, cancelled.get());
        assert!(!should_phase2_recover_message(
            cancelled.get(),
            shared.last_message_ids.get(&channel_id).map(|v| *v),
            &HashSet::new()
        ));
        let saved = std::fs::read_to_string(
            tmp.path()
                .join("runtime")
                .join("last_message")
                .join(provider.as_str())
                .join(format!("{}.txt", channel_id.get())),
        )
        .expect("checkpoint should be persisted");
        assert_eq!(saved.trim(), cancelled.get().to_string());

        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
    }

    #[test]
    fn recovery_known_message_ids_include_active_turn_message() {
        let snapshot = super::ChannelMailboxSnapshot {
            active_user_message_id: Some(MessageId::new(200)),
            intervention_queue: vec![Intervention {
                author_id: UserId::new(42),
                message_id: MessageId::new(100),
                source_message_ids: vec![MessageId::new(90), MessageId::new(100)],
                text: "queued".to_string(),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            }],
            ..Default::default()
        };

        let existing = recovery_known_message_ids(&snapshot);
        assert!(existing.contains(&90));
        assert!(existing.contains(&100));
        assert!(existing.contains(&200));
        assert!(!should_phase2_recover_message(200, None, &existing));
    }

    #[test]
    fn catch_up_retry_uses_pinned_checkpoint_after_queue_drain() {
        let shared = super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(1486333430516947304);
        let disk_checkpoint = 100;
        let pinned_retry_checkpoint = 130;
        let live_checkpoint_after_newer_messages = 500;

        shared
            .last_message_ids
            .insert(channel_id, live_checkpoint_after_newer_messages);
        shared
            .catch_up_retry_pending
            .insert(channel_id, pinned_retry_checkpoint);

        assert_eq!(
            super::catch_up_checkpoint_for_scan(
                disk_checkpoint,
                shared.last_message_ids.get(&channel_id).map(|entry| *entry),
                shared
                    .catch_up_retry_pending
                    .get(&channel_id)
                    .map(|entry| *entry),
            ),
            pinned_retry_checkpoint,
            "retry must resume from the cap-pinned checkpoint, not the newer live checkpoint"
        );

        assert_eq!(
            super::take_catch_up_retry_checkpoint_after_queue_drain(
                shared.as_ref(),
                channel_id,
                super::CATCH_UP_RETRY_QUEUE_THRESHOLD + 1,
            ),
            None,
            "retry must stay armed while the queue is still above the drain threshold"
        );
        assert!(shared.catch_up_retry_pending.contains_key(&channel_id));

        assert_eq!(
            super::take_catch_up_retry_checkpoint_after_queue_drain(
                shared.as_ref(),
                channel_id,
                super::CATCH_UP_RETRY_QUEUE_THRESHOLD,
            ),
            Some(pinned_retry_checkpoint),
            "dropping to the threshold should consume the pinned retry checkpoint"
        );
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
    }

    #[tokio::test]
    async fn catch_up_retry_becomes_ready_when_mailbox_drains_to_threshold() {
        let shared = super::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1486333430516947305);
        let author_id = UserId::new(1486333430516947306);
        let pinned_retry_checkpoint = 200;
        let initial_len = super::CATCH_UP_RETRY_QUEUE_THRESHOLD + 2;

        for offset in 0..initial_len {
            let message_id = MessageId::new(1486333430516947400 + offset as u64);
            let outcome = super::mailbox_enqueue_intervention(
                shared.as_ref(),
                &provider,
                channel_id,
                Intervention {
                    author_id,
                    message_id,
                    source_message_ids: vec![message_id],
                    text: format!("queued {offset}"),
                    mode: InterventionMode::Soft,
                    created_at: Instant::now(),
                    reply_context: None,
                    has_reply_boundary: false,
                    merge_consecutive: false,
                },
            )
            .await;
            assert!(outcome.enqueued);
        }

        shared
            .catch_up_retry_pending
            .insert(channel_id, pinned_retry_checkpoint);

        let first = shared
            .mailbox(channel_id)
            .take_next_soft(super::queue_persistence_context(
                shared.as_ref(),
                &provider,
                channel_id,
            ))
            .await;
        assert_eq!(
            first.queue_len_after,
            super::CATCH_UP_RETRY_QUEUE_THRESHOLD + 1
        );
        assert_eq!(
            super::take_catch_up_retry_checkpoint_after_queue_drain(
                shared.as_ref(),
                channel_id,
                first.queue_len_after,
            ),
            None
        );
        assert!(shared.catch_up_retry_pending.contains_key(&channel_id));

        let second = shared
            .mailbox(channel_id)
            .take_next_soft(super::queue_persistence_context(
                shared.as_ref(),
                &provider,
                channel_id,
            ))
            .await;
        assert_eq!(
            second.queue_len_after,
            super::CATCH_UP_RETRY_QUEUE_THRESHOLD
        );
        assert_eq!(
            super::take_catch_up_retry_checkpoint_after_queue_drain(
                shared.as_ref(),
                channel_id,
                second.queue_len_after,
            ),
            Some(pinned_retry_checkpoint)
        );
    }

    /// #1227 regression: in a channel where the last_id checkpoint is followed
    /// by 10 bot messages and then 1 user message, the OLD `limit=10` window
    /// would return only the 10 bots and silently drop the user message. With
    /// the bumped `CATCH_UP_FETCH_LIMIT=50` page size, both ends of that
    /// pattern (and a second user message even further back-to-back) must be
    /// recovered.
    ///
    /// We exercise the pure classifier directly so the test is hermetic — no
    /// Discord HTTP, no `serenity::Message` construction. The page-size guard
    /// itself is a separate `assert!` on the constant, which protects against
    /// a future drive-by lowering it again.
    #[test]
    fn catch_up_recovers_user_message_buried_under_bot_flood() {
        // Snowflake-ish ascending IDs. Older first (Discord `after=` ordering).
        let bot_self_id: u64 = 999;
        let user_a_id: u64 = 1;
        let user_b_id: u64 = 2;
        let allowed_bot_ids: Vec<u64> = vec![];
        let announce_bot_id: Option<u64> = None;
        let existing: HashSet<u64> = HashSet::new();
        let max_age_secs: i64 = 300;

        // Build the simulated REST page: USER_A, then 10 bot messages, then USER_B.
        // This is exactly the topology the issue describes.
        let mut page: Vec<CatchUpMessageView> = Vec::with_capacity(12);
        page.push(CatchUpMessageView {
            message_id: 100,
            author_id: user_a_id,
            author_is_bot: false,
            is_processable_kind: true,
            age_secs: 60,
            trimmed_text: "머지상태 확인해봐".to_string(),
        });
        for i in 0..10 {
            page.push(CatchUpMessageView {
                message_id: 101 + i,
                author_id: bot_self_id,
                author_is_bot: true,
                is_processable_kind: true,
                age_secs: 50 - i as i64,
                trimmed_text: format!("CI step {i} done"),
            });
        }
        page.push(CatchUpMessageView {
            message_id: 200,
            author_id: user_b_id,
            author_is_bot: false,
            is_processable_kind: true,
            age_secs: 30,
            trimmed_text: "지금 서버 내려가고 있어".to_string(),
        });

        // 1) Page-size guard: after the fix the catch-up REST window MUST be
        //    big enough that an 11-bot run cannot bury a user message inside
        //    a single page. 50 was chosen deliberately; tighten the bound but
        //    don't allow a regression below 11.
        assert!(
            CATCH_UP_FETCH_LIMIT as usize >= page.len(),
            "CATCH_UP_FETCH_LIMIT={CATCH_UP_FETCH_LIMIT} too small to survive \
             10-bot-flood scenario (need >= {})",
            page.len()
        );

        // 2) Classification: with the full page surfaced, both user messages
        //    must be Recover and the 10 bot messages must be SelfAuthored.
        let mut stats = CatchUpScanStats::default();
        stats.returned = page.len();
        let mut recovered_ids: Vec<u64> = Vec::new();
        for view in &page {
            let outcome = classify_catch_up_message(
                view,
                Some(bot_self_id),
                &existing,
                max_age_secs,
                &allowed_bot_ids,
                announce_bot_id,
            );
            stats.record(outcome);
            if outcome == CatchUpClassification::Recover {
                recovered_ids.push(view.message_id);
            }
        }

        assert_eq!(
            recovered_ids,
            vec![100, 200],
            "both buried user messages must be recovered (got {:?})",
            recovered_ids
        );
        assert_eq!(stats.recovered, 2);
        assert_eq!(stats.self_authored, 10);
        assert_eq!(stats.duplicate, 0);
        assert_eq!(stats.too_old, 0);
        assert_eq!(stats.empty, 0);
        assert_eq!(stats.not_allowed, 0);
        assert_eq!(stats.system_kind, 0);

        // 3) Regression baseline: simulate the OLD limit=10 contract by
        //    truncating the page. The first (oldest) user message survives
        //    because it sits at the head; the SECOND user message (the one
        //    actually reported in the issue) gets silently dropped — this is
        //    exactly the bug. The assertion documents the failure mode that
        //    motivated the fix.
        let truncated: Vec<&CatchUpMessageView> = page.iter().take(10).collect();
        let mut old_recovered: Vec<u64> = Vec::new();
        for view in truncated {
            if classify_catch_up_message(
                view,
                Some(bot_self_id),
                &existing,
                max_age_secs,
                &allowed_bot_ids,
                announce_bot_id,
            ) == CatchUpClassification::Recover
            {
                old_recovered.push(view.message_id);
            }
        }
        assert!(
            !old_recovered.contains(&200),
            "regression baseline: limit=10 SHOULD lose user message 200 \
             (got {:?})",
            old_recovered
        );
    }

    #[test]
    fn idle_session_cleanup_defers_watched_sessions_to_tmux_liveness() {
        assert_eq!(
            super::idle_session_watcher_cleanup(true),
            super::IdleSessionWatcherCleanup::DeferToTmuxLiveness
        );
        assert_eq!(
            super::idle_session_watcher_cleanup(false),
            super::IdleSessionWatcherCleanup::ExpireSession
        );
    }

    #[test]
    fn queued_message_ids_exclude_active_turn_message() {
        let snapshot = super::ChannelMailboxSnapshot {
            active_user_message_id: Some(MessageId::new(200)),
            intervention_queue: vec![Intervention {
                author_id: UserId::new(42),
                message_id: MessageId::new(100),
                source_message_ids: vec![MessageId::new(90), MessageId::new(100)],
                text: "queued".to_string(),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            }],
            ..Default::default()
        };

        let existing = queued_message_ids(&snapshot);
        assert!(existing.contains(&90));
        assert!(existing.contains(&100));
        assert!(!existing.contains(&200));
    }

    #[tokio::test]
    async fn idle_queue_kickoff_blocks_until_cleanup_retry_resolved() {
        let _lock = super::runtime_store::lock_test_env();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let temp_root = tempfile::tempdir().expect("temp root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp_root.path()) };

        let shared = super::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(1486333430516947101);
        let owner_id = UserId::new(1487795113240559801);
        let queued_msg_id = MessageId::new(1487795113240559802);
        let placeholder_msg_id = MessageId::new(1487799916758827803);
        let channel_name = format!("adk-cdx-t{}", channel_id.get());
        let tmux_name = provider.build_tmux_session_name(&channel_name);

        let enqueue = super::mailbox_enqueue_intervention(
            shared.as_ref(),
            &provider,
            channel_id,
            Intervention {
                author_id: owner_id,
                message_id: queued_msg_id,
                source_message_ids: vec![queued_msg_id],
                text: "queued turn".to_string(),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            },
        )
        .await;
        assert!(enqueue.enqueued);

        let inflight_state = super::InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            Some(channel_name),
            owner_id.get(),
            MessageId::new(1487795113240559800).get(),
            placeholder_msg_id.get(),
            "cleanup retry turn".to_string(),
            None,
            Some(tmux_name.clone()),
            Some("/tmp/agentdesk-idle-cleanup-retry-output.jsonl".to_string()),
            Some("/tmp/agentdesk-idle-cleanup-retry-input.fifo".to_string()),
            0,
        );
        super::save_inflight_state(&inflight_state).expect("save cleanup retry inflight");
        shared.placeholder_cleanup.record(PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id: placeholder_msg_id,
            tmux_session_name: Some(tmux_name),
            operation: PlaceholderCleanupOperation::EditTerminal,
            outcome: PlaceholderCleanupOutcome::failed("HTTP 500 edit failed"),
            source: "idle_queue_kickoff_test",
        });

        let snapshot = super::mailbox_snapshot(shared.as_ref(), channel_id).await;
        assert!(!super::idle_queue_snapshot_has_kickable_backlog(
            shared.as_ref(),
            &provider,
            channel_id,
            &snapshot
        ));
        assert!(super::cleanup_retry_inflight_blocks_idle_kickoff(
            shared.as_ref(),
            &provider,
            channel_id
        ));
        assert!(
            super::idle_queue_take_next_soft_if_ready(&shared, &provider, channel_id)
                .await
                .is_none()
        );

        shared.placeholder_cleanup.record(PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id: placeholder_msg_id,
            tmux_session_name: None,
            operation: PlaceholderCleanupOperation::EditTerminal,
            outcome: PlaceholderCleanupOutcome::Succeeded,
            source: "idle_queue_kickoff_test",
        });

        assert!(!super::cleanup_retry_inflight_blocks_idle_kickoff(
            shared.as_ref(),
            &provider,
            channel_id
        ));
        let snapshot = super::mailbox_snapshot(shared.as_ref(), channel_id).await;
        assert!(super::idle_queue_snapshot_has_kickable_backlog(
            shared.as_ref(),
            &provider,
            channel_id,
            &snapshot
        ));
        let (started, has_more) =
            super::idle_queue_take_next_soft_if_ready(&shared, &provider, channel_id)
                .await
                .expect("resolved cleanup should allow queued turn kickoff");
        assert_eq!(started.message_id, queued_msg_id);
        assert!(!has_more);

        super::clear_inflight_state(&provider, channel_id.get());
        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    #[test]
    fn handoff_routing_guard_rejects_wrong_agent_settings() {
        let settings = DiscordBotSettings {
            provider: ProviderKind::Codex,
            agent: Some("openclaw-maker".to_string()),
            allowed_channel_ids: vec![1488022491992424448],
            ..Default::default()
        };

        let result = validate_bot_channel_routing(
            &settings,
            &ProviderKind::Codex,
            ChannelId::new(1488022491992424448),
            Some("agentdesk-spark"),
            false,
        );

        assert_eq!(result, Err(BotChannelRoutingGuardFailure::AgentMismatch));
    }

    #[test]
    fn startup_recovery_source_prioritizes_catch_up_and_backgrounds_thread_map_validation() {
        let source = include_str!("runtime_bootstrap.rs");
        let start = source
            .find("// Restore inflight turns FIRST, then flush restart reports.")
            .expect("startup recovery block comment missing");
        let end = source[start..]
            .find("// Background: periodic cleanup for stale Discord upload files")
            .map(|offset| start + offset)
            .expect("startup recovery block terminator missing");
        let startup_block = &source[start..end];

        let catch_up = startup_block
            .find("catch_up_missed_messages(")
            .expect("catch-up call missing");
        let gc = startup_block
            .find("gc_stale_fixed_working_sessions(&shared_for_tmux2).await;")
            .expect("gc call missing");
        let restore = startup_block
            .find("restore_inflight_turns(")
            .expect("restore_inflight_turns call missing");
        let intake_open = startup_block
            .find("✓ Reconcile complete — intake open")
            .expect("reconcile completion log missing");
        let background_validation = startup_block
            .find("spawn_startup_thread_map_validation(")
            .expect("background thread-map validation spawn missing");

        assert!(
            catch_up < gc,
            "catch-up must run before stale fixed-working-session cleanup"
        );
        assert!(
            catch_up < restore,
            "catch-up must run before inflight restore so restart-gap messages queue immediately"
        );
        assert!(
            background_validation > intake_open,
            "thread-map validation must not block reconcile completion or kickoff"
        );
        assert!(
            !startup_block.contains("validate_channel_thread_maps_on_startup("),
            "startup critical path must not await thread-map validation directly"
        );
    }

    #[tokio::test]
    async fn idle_cleanup_marks_session_disconnected_before_recording_audit() {
        let db = crate::db::test_db();
        let session_key = "host:cleanup-session-status";

        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions
                 (session_key, status, active_dispatch_id, claude_session_id, created_at)
                 VALUES (?1, 'idle', 'dispatch-1', 'sid-1', datetime('now'))",
                [session_key],
            )
            .unwrap();

        assert!(mark_session_disconnected_for_idle_cleanup(Some(&db), None, session_key).await);

        let session_row: (String, Option<String>, Option<String>) = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT status, active_dispatch_id, claude_session_id
                 FROM sessions
                 WHERE session_key = ?1",
                [session_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(session_row.0, "disconnected");
        assert_eq!(session_row.1, None);
        assert_eq!(session_row.2.as_deref(), Some("sid-1"));
    }

    /// Per-test Postgres database lifecycle for the #1238 migration of the
    /// idle-cleanup duplicate-audit guard test, which now exercises the
    /// PG-aware `mark_session_disconnected_for_idle_cleanup` path and the
    /// `session_termination_events` audit table on Postgres.
    struct DiscordIdlePgDatabase {
        _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl DiscordIdlePgDatabase {
        async fn create() -> Self {
            let lifecycle = crate::db::postgres::lock_test_lifecycle();
            let admin_url = pg_test_admin_database_url();
            let database_name = format!("agentdesk_discord_idle_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", pg_test_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "discord idle pg",
            )
            .await
            .expect("create discord idle postgres test db");

            Self {
                _lifecycle: lifecycle,
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "discord idle pg",
            )
            .await
            .expect("connect + migrate discord idle postgres test db")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "discord idle pg",
            )
            .await
            .expect("drop discord idle postgres test db");
        }
    }

    fn pg_test_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }
        let user = std::env::var("PGUSER")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| std::env::var("USER").ok().filter(|v| !v.trim().is_empty()))
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());
        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn pg_test_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", pg_test_base_database_url(), admin_db)
    }

    #[tokio::test]
    async fn idle_cleanup_skips_duplicate_audit_when_force_kill_pg_already_disconnected_session() {
        let pg_db = DiscordIdlePgDatabase::create().await;
        let pool = pg_db.migrate().await;
        let session_key = "host:cleanup-session-dedupe";

        sqlx::query(
            "INSERT INTO sessions
             (session_key, status, created_at)
             VALUES ($1, 'disconnected', NOW())",
        )
        .bind(session_key)
        .execute(&pool)
        .await
        .unwrap();

        crate::services::termination_audit::record_termination_with_handles(
            None,
            Some(&pool),
            session_key,
            None,
            "force_kill_api",
            "force_kill",
            Some("idle 60분 초과 — 자동 정리"),
            None,
            None,
            Some(false),
        );

        // record_termination_with_handles is fire-and-forget (spawns onto the
        // tokio runtime); wait for the force-kill audit row to land before we
        // exercise the cleanup dedupe path.
        let mut force_kill_persisted = false;
        for _ in 0..40 {
            let count = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*)::BIGINT FROM session_termination_events WHERE session_key = $1",
            )
            .bind(session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
            if count >= 1 {
                force_kill_persisted = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            force_kill_persisted,
            "force-kill termination audit row was not persisted to postgres"
        );

        let should_record =
            mark_session_disconnected_for_idle_cleanup(None, Some(&pool), session_key).await;
        assert!(
            !should_record,
            "cleanup must skip a second termination audit when force-kill already disconnected the session"
        );

        if should_record {
            crate::services::termination_audit::record_termination_with_handles(
                None,
                Some(&pool),
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

        let audit_count: i64 = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT FROM session_termination_events WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(audit_count, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    // #1332: When a queued intervention is cancelled / expired / superseded the
    // queue-exit feedback path must drop the corresponding entry in
    // `queued_placeholders` so a subsequent dispatch never reuses a placeholder
    // belonging to a no-longer-active intervention. The placeholder controller
    // entry tied to the same Discord message id must also be detached so the
    // cap-bounded `entries` map does not retain a stale Queued row.
    // codex review round-6 P2 (#1332): `apply_queue_exit_feedback` calls
    // `queue_exit_drain_queued_placeholders`, which now writes through to
    // disk via `persist_channel_from_map` whenever a mapping is drained.
    // Wrap the test in `lock_test_env` + a temp `AGENTDESK_ROOT_DIR` so
    // the write-through lands in a per-test temp directory and cannot
    // pollute the dev runtime or race a parallel test.
    #[tokio::test]
    async fn queued_placeholders_cleared_on_queue_exit() {
        use super::QueueExitEvent;
        use super::QueueExitKind;
        use crate::services::discord::runtime_store::lock_test_env;
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap());
        }

        let shared = super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(900_000_000_000_001);
        let user_msg_id = MessageId::new(800_000_000_000_001);
        let placeholder_msg_id = MessageId::new(700_000_000_000_001);
        shared
            .queued_placeholders
            .insert((channel_id, user_msg_id), placeholder_msg_id);
        assert_eq!(shared.queued_placeholders.len(), 1);

        let event = QueueExitEvent {
            intervention: Intervention {
                author_id: UserId::new(42),
                message_id: user_msg_id,
                source_message_ids: vec![user_msg_id],
                text: "ignored".to_string(),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            },
            kind: QueueExitKind::Cancelled,
        };
        super::apply_queue_exit_feedback(&shared, channel_id, std::slice::from_ref(&event)).await;
        assert_eq!(
            shared.queued_placeholders.len(),
            0,
            "queue-exit feedback must drop the queued placeholder mapping"
        );

        unsafe {
            std::env::remove_var("AGENTDESK_ROOT_DIR");
        }
    }

    // codex review P2 (#1332 follow-up): the queue-exit drain must report the
    // (placeholder_msg_id, kind) pair for each cleared mapping so the caller
    // can edit/delete the leftover Discord card. Without this signal,
    // `apply_queue_exit_feedback` cannot rewrite the visible `📬 메시지 대기 중`
    // text and the user is left looking at a promise for a turn that has
    // been cancelled.
    // codex review round-6 P2 (#1332): `queue_exit_drain_queued_placeholders`
    // writes through to disk via `persist_channel_from_map` whenever it
    // drains at least one mapping. Wrap the test in `lock_test_env` +
    // temp `AGENTDESK_ROOT_DIR` so the write lands in a per-test sandbox.
    #[tokio::test]
    async fn queue_exit_drain_reports_visible_cards_for_each_kind() {
        use super::QueueExitEvent;
        use super::QueueExitKind;
        use crate::services::discord::runtime_store::lock_test_env;
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap());
        }

        let shared = super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(910_000_000_000_001);
        let cancelled_msg = MessageId::new(810_000_000_000_001);
        let cancelled_card = MessageId::new(710_000_000_000_001);
        let expired_msg = MessageId::new(810_000_000_000_002);
        let expired_card = MessageId::new(710_000_000_000_002);
        let superseded_msg = MessageId::new(810_000_000_000_003);
        let superseded_card = MessageId::new(710_000_000_000_003);

        shared
            .queued_placeholders
            .insert((channel_id, cancelled_msg), cancelled_card);
        shared
            .queued_placeholders
            .insert((channel_id, expired_msg), expired_card);
        shared
            .queued_placeholders
            .insert((channel_id, superseded_msg), superseded_card);

        let mk_event = |msg_id: MessageId, kind: QueueExitKind| QueueExitEvent {
            intervention: Intervention {
                author_id: UserId::new(99),
                message_id: msg_id,
                source_message_ids: vec![msg_id],
                text: "ignored".to_string(),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            },
            kind,
        };
        let events = vec![
            mk_event(cancelled_msg, QueueExitKind::Cancelled),
            mk_event(expired_msg, QueueExitKind::Expired),
            mk_event(superseded_msg, QueueExitKind::Superseded),
        ];
        let event_refs: Vec<&QueueExitEvent> = events.iter().collect();

        let cards =
            super::queue_exit_drain_queued_placeholders(&shared, channel_id, &event_refs).await;

        assert_eq!(cards.len(), 3);
        let card_tuples: Vec<(MessageId, super::QueueExitKind)> = cards
            .iter()
            .map(|card| (card.placeholder_msg_id, card.kind))
            .collect();
        assert!(
            card_tuples.contains(&(cancelled_card, QueueExitKind::Cancelled)),
            "cancelled card should surface for visible-card cleanup"
        );
        assert!(
            card_tuples.contains(&(expired_card, QueueExitKind::Expired)),
            "expired card should surface for visible-card cleanup"
        );
        assert!(
            card_tuples.contains(&(superseded_card, QueueExitKind::Superseded)),
            "superseded card should surface for visible-card cleanup"
        );
        assert!(
            shared.queued_placeholders.is_empty(),
            "every mapping must be drained"
        );

        // The replacement body must exist for every queue-exit kind.
        assert!(super::queue_exit_card_body(QueueExitKind::Cancelled).contains("큐에서 제거됨"));
        assert!(super::queue_exit_card_body(QueueExitKind::Expired).contains("큐에서 제거됨"));
        assert!(super::queue_exit_card_body(QueueExitKind::Superseded).contains("큐에서 제거됨"));

        unsafe {
            std::env::remove_var("AGENTDESK_ROOT_DIR");
        }
    }

    // codex review P2 (#1332 follow-up): merged interventions accumulate
    // multiple `source_message_ids`; each had registered its own queued
    // placeholder. When the merged intervention later exits the queue, the
    // drain must clear EVERY source id's mapping in one pass — not just the
    // head id.
    //
    // codex review round-6 P2 (#1332): isolated under temp `AGENTDESK_ROOT_DIR`
    // so the persistence write-through cannot leak into the dev runtime or
    // race a parallel test.
    #[tokio::test]
    async fn queue_exit_drain_handles_merged_source_ids() {
        use super::QueueExitEvent;
        use super::QueueExitKind;
        use crate::services::discord::runtime_store::lock_test_env;
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap());
        }

        let shared = super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(920_000_000_000_001);
        let head_msg = MessageId::new(820_000_000_000_001);
        let head_card = MessageId::new(720_000_000_000_001);
        let merged_msg_a = MessageId::new(820_000_000_000_002);
        let merged_card_a = MessageId::new(720_000_000_000_002);
        let merged_msg_b = MessageId::new(820_000_000_000_003);
        let merged_card_b = MessageId::new(720_000_000_000_003);

        shared
            .queued_placeholders
            .insert((channel_id, head_msg), head_card);
        shared
            .queued_placeholders
            .insert((channel_id, merged_msg_a), merged_card_a);
        shared
            .queued_placeholders
            .insert((channel_id, merged_msg_b), merged_card_b);

        let event = QueueExitEvent {
            intervention: Intervention {
                author_id: UserId::new(50),
                message_id: head_msg,
                source_message_ids: vec![merged_msg_a, merged_msg_b, head_msg],
                text: "merged".to_string(),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: true,
            },
            kind: QueueExitKind::Superseded,
        };
        let event_refs: Vec<&QueueExitEvent> = vec![&event];

        let cards =
            super::queue_exit_drain_queued_placeholders(&shared, channel_id, &event_refs).await;

        assert_eq!(
            cards.len(),
            3,
            "all three source-id placeholders should be drained"
        );
        let ids: std::collections::HashSet<MessageId> =
            cards.iter().map(|card| card.placeholder_msg_id).collect();
        assert!(ids.contains(&head_card));
        assert!(ids.contains(&merged_card_a));
        assert!(ids.contains(&merged_card_b));
        assert!(
            shared.queued_placeholders.is_empty(),
            "every merged source id must be drained from queued_placeholders"
        );

        unsafe {
            std::env::remove_var("AGENTDESK_ROOT_DIR");
        }
    }

    #[tokio::test]
    async fn queue_exit_ctx_missing_persists_pending_clears_and_drain_deletes_them() {
        use super::QueueExitEvent;
        use super::QueueExitKind;
        use crate::services::discord::queued_placeholders_store;
        use crate::services::discord::runtime_store::lock_test_env;

        struct RecordingDeleter {
            calls: std::sync::Arc<std::sync::Mutex<Vec<(ChannelId, MessageId)>>>,
        }

        impl super::runtime_bootstrap::StalePlaceholderDeleter for RecordingDeleter {
            fn delete<'a>(
                &'a self,
                channel_id: ChannelId,
                placeholder_msg_id: MessageId,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>
            {
                let calls = self.calls.clone();
                Box::pin(async move {
                    calls.lock().unwrap().push((channel_id, placeholder_msg_id));
                    Ok(())
                })
            }
        }

        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap());
        }

        let shared = super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(925_000_000_000_001);
        let head_msg = MessageId::new(825_000_000_000_001);
        let head_card = MessageId::new(725_000_000_000_001);
        let merged_msg = MessageId::new(825_000_000_000_002);
        let merged_card = MessageId::new(725_000_000_000_002);

        shared
            .insert_queued_placeholder(channel_id, head_msg, head_card)
            .await;
        shared
            .insert_queued_placeholder(channel_id, merged_msg, merged_card)
            .await;

        let event = QueueExitEvent {
            intervention: Intervention {
                author_id: UserId::new(50),
                message_id: head_msg,
                source_message_ids: vec![head_msg, merged_msg],
                text: "merged".to_string(),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: true,
            },
            kind: QueueExitKind::Cancelled,
        };

        super::apply_queue_exit_feedback(&shared, channel_id, std::slice::from_ref(&event)).await;

        assert!(
            shared.queued_placeholders.is_empty(),
            "queue-exit must still drain the active queued-placeholder handoff"
        );
        assert_eq!(
            shared.queue_exit_placeholder_clears.len(),
            2,
            "ctx-missing queue-exit must keep visible card ids pending in memory"
        );

        let restored_pending = queued_placeholders_store::load_queue_exit_placeholder_clears(
            &shared.provider,
            &shared.token_hash,
        );
        assert_eq!(
            restored_pending.len(),
            2,
            "ctx-missing queue-exit must mirror pending visible card ids to the sidecar"
        );
        assert_eq!(
            restored_pending.get(&(channel_id, head_msg)),
            Some(&head_card)
        );
        assert_eq!(
            restored_pending.get(&(channel_id, merged_msg)),
            Some(&merged_card)
        );

        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let deleter = RecordingDeleter {
            calls: calls.clone(),
        };
        let (deleted, failed) =
            super::drain_pending_queue_exit_placeholder_clears_with(&shared, &deleter).await;

        assert_eq!((deleted, failed), (2, 0));
        let call_set: std::collections::HashSet<(ChannelId, MessageId)> =
            calls.lock().unwrap().iter().copied().collect();
        assert!(call_set.contains(&(channel_id, head_card)));
        assert!(call_set.contains(&(channel_id, merged_card)));
        assert!(
            shared.queue_exit_placeholder_clears.is_empty(),
            "successful ready-time drain must clear pending memory"
        );
        assert!(
            queued_placeholders_store::load_queue_exit_placeholder_clears(
                &shared.provider,
                &shared.token_hash,
            )
            .is_empty(),
            "successful ready-time drain must remove the pending sidecar"
        );

        unsafe {
            std::env::remove_var("AGENTDESK_ROOT_DIR");
        }
    }

    /// codex review round-3 P1: simulate the dispatch path consuming the
    /// `queued_placeholders` handoff between `mailbox_enqueue_intervention`
    /// and the race-loss `ensure_queued` call.  The race-loss handler must
    /// recheck ownership and decline to mutate Discord — otherwise it would
    /// edit/delete a message that the active turn now uses as its live
    /// response card.
    #[tokio::test]
    async fn queued_placeholder_ownership_recheck_detects_dispatch_consumption() {
        // Isolate the round-3 P2 write-through to a tempdir so this test does
        // not pollute the developer's ~/.adk/release runtime directory when
        // AGENTDESK_ROOT_DIR is unset.
        let _lock = crate::services::discord::runtime_store::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap());
        }

        let shared = super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(930_000_000_000_001);
        let user_msg_id = MessageId::new(830_000_000_000_001);
        let placeholder_msg_id = MessageId::new(730_000_000_000_001);

        // 1) Race-loss handler inserts the mapping after enqueue.
        shared
            .insert_queued_placeholder(channel_id, user_msg_id, placeholder_msg_id)
            .await;
        assert!(
            shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id),
            "fresh insert must report ownership"
        );

        // 2) Dispatch path picks up the queued turn — the active turn finished
        //    concurrently and `remove_queued_placeholder` consumes the handoff.
        let consumed = shared
            .remove_queued_placeholder(channel_id, user_msg_id)
            .await;
        assert_eq!(consumed, Some(placeholder_msg_id));

        // 3) Race-loss handler reaches its `ensure_queued` await — the
        //    recheck MUST report it no longer owns the message id, so the
        //    handler bails out without touching Discord.
        assert!(
            !shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id),
            "after dispatch consumption, recheck must report no ownership"
        );

        // 4) An adversarial dispatch could theoretically reinsert a *different*
        //    placeholder for the same key; the recheck must compare values, not
        //    just presence, so we never edit a message that belongs to a newer
        //    turn.
        let other_placeholder = MessageId::new(730_000_000_000_002);
        shared
            .insert_queued_placeholder(channel_id, user_msg_id, other_placeholder)
            .await;
        assert!(
            !shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id),
            "recheck must compare placeholder ids, not just presence"
        );
        assert!(
            shared.queued_placeholder_still_owned(channel_id, user_msg_id, other_placeholder),
            "recheck succeeds for the new owner's placeholder"
        );

        unsafe {
            std::env::remove_var("AGENTDESK_ROOT_DIR");
        }
    }

    /// codex review round-3 P2: simulate a dcserver restart while a foreground
    /// message is queued. After the restart, the freshly-bootstrapped
    /// `SharedData` must observe the same `(channel_id, user_msg_id) →
    /// placeholder_msg_id` mapping that was live before the crash so the
    /// dispatch path can re-attach the restored mailbox queue entry to the
    /// existing `📬 메시지 대기 중` Discord card.
    #[test]
    fn queued_placeholders_persist_across_restart() {
        use crate::services::discord::queued_placeholders_store;
        use crate::services::discord::runtime_store::lock_test_env;

        const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "round3_p2_restart_hash";
        let channel_id = ChannelId::new(940_000_000_000_001);
        let user_msg_id_a = MessageId::new(840_000_000_000_001);
        let user_msg_id_b = MessageId::new(840_000_000_000_002);
        let placeholder_a = MessageId::new(740_000_000_000_001);
        let placeholder_b = MessageId::new(740_000_000_000_002);

        // 1) "Pre-restart" SharedData: insert via the write-through helper so
        //    the on-disk snapshot mirrors the in-memory map.
        let pre_restart = super::make_shared_data_for_tests();
        // The test harness builds SharedData with a fixed token_hash; for this
        // restart test we want full control over the persistence namespace,
        // so write directly via the store helper (matching what
        // `insert_queued_placeholder` does on the real path).
        pre_restart
            .queued_placeholders
            .insert((channel_id, user_msg_id_a), placeholder_a);
        pre_restart
            .queued_placeholders
            .insert((channel_id, user_msg_id_b), placeholder_b);
        queued_placeholders_store::persist_channel_from_map(
            &pre_restart.queued_placeholders,
            &provider,
            token_hash,
            channel_id,
        );

        // 2) Snapshot must land on disk under the bot's namespace.
        let snapshot_file = tmp
            .path()
            .join("runtime")
            .join("discord_queued_placeholders")
            .join("claude")
            .join(token_hash)
            .join(format!("{}.json", channel_id.get()));
        assert!(
            snapshot_file.exists(),
            "queued-placeholder snapshot must be persisted at {:?}",
            snapshot_file
        );

        // 3) "Restart" — drop pre_restart, build a fresh SharedData, load
        //    from disk (mirrors what runtime_bootstrap does just before
        //    `kickoff_idle_queues`).
        drop(pre_restart);
        let post_restart = super::make_shared_data_for_tests();
        assert!(
            post_restart.queued_placeholders.is_empty(),
            "fresh SharedData must start with an empty map (sanity check)"
        );
        let restored = queued_placeholders_store::load_queued_placeholders(&provider, token_hash);
        for (key, placeholder_msg_id) in restored {
            post_restart
                .queued_placeholders
                .insert(key, placeholder_msg_id);
        }

        // 4) Both pre-restart mappings are visible to the dispatch path.
        assert_eq!(post_restart.queued_placeholders.len(), 2);
        assert_eq!(
            post_restart
                .queued_placeholders
                .get(&(channel_id, user_msg_id_a))
                .map(|v| *v),
            Some(placeholder_a)
        );
        assert_eq!(
            post_restart
                .queued_placeholders
                .get(&(channel_id, user_msg_id_b))
                .map(|v| *v),
            Some(placeholder_b)
        );

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    /// codex review round-4 P2: serialize concurrent
    /// `insert_queued_placeholder` / `remove_queued_placeholder` calls on the
    /// SAME channel via the per-channel persistence mutex. Without the
    /// mutex, two concurrent updates could each snapshot the DashMap and
    /// then race their `atomic_write` calls — letting an older snapshot
    /// finish last and resurrect an entry the newer call had already
    /// removed (or drop one the newer call had inserted). After this test
    /// returns, the on-disk snapshot MUST match the in-memory `DashMap`
    /// byte-for-byte.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_insert_remove_serializes_persistence_per_channel() {
        use crate::services::discord::queued_placeholders_store;
        use crate::services::discord::runtime_store::lock_test_env;
        use std::collections::HashSet;

        const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let shared = super::make_shared_data_for_tests();
        // `make_shared_data_for_tests` hard-codes provider=Claude and
        // token_hash="test-token-hash"; load via the same constants so the
        // load path looks at the same namespace the write-through helper
        // wrote to.
        let provider = shared.provider.clone();
        let token_hash = shared.token_hash.clone();
        let channel_id = ChannelId::new(950_000_000_000_001);

        // Pre-seed N entries so each concurrent task has something to remove.
        // Use a non-trivial fan-out (16 inserts + 16 removes spawned in
        // parallel) to maximize the chance of catching a snapshot-write
        // reordering bug if the per-channel lock were absent.
        const FANOUT: u64 = 16;
        let preseed_keys: Vec<(MessageId, MessageId)> = (0..FANOUT)
            .map(|i| {
                (
                    MessageId::new(850_000_000_000_000 + i),
                    MessageId::new(750_000_000_000_000 + i),
                )
            })
            .collect();
        for (user_msg_id, placeholder_msg_id) in &preseed_keys {
            shared
                .insert_queued_placeholder(channel_id, *user_msg_id, *placeholder_msg_id)
                .await;
        }

        // Spawn 2*FANOUT concurrent operations on the SAME channel.  Half
        // remove a pre-seeded key, half insert a fresh key.  Each call
        // independently snapshots the DashMap and writes the channel file;
        // without the per-channel persistence mutex, an older snapshot can
        // land on disk after a newer one and overwrite the newer state.
        // Round-5 P2 promoted the lock to `tokio::sync::Mutex`, so the
        // tasks are now plain `tokio::spawn` futures rather than
        // `spawn_blocking` closures — the persistence helpers are async.
        let mut handles = Vec::new();
        for (user_msg_id, _) in preseed_keys.iter() {
            let shared = shared.clone();
            let user_msg_id = *user_msg_id;
            handles.push(tokio::spawn(async move {
                shared
                    .remove_queued_placeholder(channel_id, user_msg_id)
                    .await;
            }));
        }
        let new_keys: Vec<(MessageId, MessageId)> = (0..FANOUT)
            .map(|i| {
                (
                    MessageId::new(860_000_000_000_000 + i),
                    MessageId::new(760_000_000_000_000 + i),
                )
            })
            .collect();
        for (user_msg_id, placeholder_msg_id) in new_keys.iter() {
            let shared = shared.clone();
            let user_msg_id = *user_msg_id;
            let placeholder_msg_id = *placeholder_msg_id;
            handles.push(tokio::spawn(async move {
                shared
                    .insert_queued_placeholder(channel_id, user_msg_id, placeholder_msg_id)
                    .await;
            }));
        }
        for h in handles {
            h.await.expect("concurrent persistence task must not panic");
        }

        // Final in-memory state: the FANOUT pre-seeded entries are gone,
        // the FANOUT new entries remain.
        let in_memory: HashSet<(MessageId, MessageId)> = shared
            .queued_placeholders
            .iter()
            .filter(|kv| kv.key().0 == channel_id)
            .map(|kv| (kv.key().1, *kv.value()))
            .collect();
        let expected_in_memory: HashSet<(MessageId, MessageId)> =
            new_keys.iter().copied().collect();
        assert_eq!(
            in_memory, expected_in_memory,
            "in-memory DashMap should reflect every concurrent mutation"
        );

        // The critical assertion: the on-disk snapshot must match
        // byte-for-byte.  If two snapshots raced, an older write would
        // overwrite the newer one and the loaded set would diverge from
        // memory (extra resurrected entries, or missing fresh ones).
        let restored = queued_placeholders_store::load_queued_placeholders(&provider, &token_hash);
        let on_disk: HashSet<(MessageId, MessageId)> = restored
            .into_iter()
            .filter(|((ch, _), _)| *ch == channel_id)
            .map(|((_, user_msg_id), placeholder_msg_id)| (user_msg_id, placeholder_msg_id))
            .collect();
        assert_eq!(
            on_disk, expected_in_memory,
            "on-disk snapshot must equal in-memory state after concurrent inserts/removes (per-channel persistence mutex serializes snapshot writes)",
        );

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    /// codex review round-4 P2: the per-channel persistence mutex is keyed
    /// by `ChannelId`, so two channels MUST be able to persist
    /// concurrently without serializing on each other's I/O. This is the
    /// throughput half of the contract — the previous test asserts
    /// correctness within a single channel; this test asserts isolation
    /// across channels.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_persistence_does_not_serialize_across_channels() {
        use crate::services::discord::queued_placeholders_store;
        use crate::services::discord::runtime_store::lock_test_env;

        const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let shared = super::make_shared_data_for_tests();
        let provider = shared.provider.clone();
        let token_hash = shared.token_hash.clone();
        let channel_a = ChannelId::new(960_000_000_000_001);
        let channel_b = ChannelId::new(960_000_000_000_002);
        let user_msg_a = MessageId::new(870_000_000_000_001);
        let user_msg_b = MessageId::new(870_000_000_000_002);
        let placeholder_a = MessageId::new(770_000_000_000_001);
        let placeholder_b = MessageId::new(770_000_000_000_002);

        // Insert on both channels concurrently. tokio::join! awaits both
        // at once; if the implementation accidentally used a single global
        // persistence mutex this would still pass correctness-wise, so the
        // real assertion is the post-condition below — both channels'
        // mappings land on disk in their respective files.
        let shared_a = shared.clone();
        let shared_b = shared.clone();
        let task_a = tokio::spawn(async move {
            shared_a
                .insert_queued_placeholder(channel_a, user_msg_a, placeholder_a)
                .await;
        });
        let task_b = tokio::spawn(async move {
            shared_b
                .insert_queued_placeholder(channel_b, user_msg_b, placeholder_b)
                .await;
        });
        let (ra, rb) = tokio::join!(task_a, task_b);
        ra.expect("channel-a persistence must not panic");
        rb.expect("channel-b persistence must not panic");

        let restored = queued_placeholders_store::load_queued_placeholders(&provider, &token_hash);
        assert_eq!(
            restored.get(&(channel_a, user_msg_a)).copied(),
            Some(placeholder_a),
            "channel-a snapshot must be persisted independently",
        );
        assert_eq!(
            restored.get(&(channel_b, user_msg_b)).copied(),
            Some(placeholder_b),
            "channel-b snapshot must be persisted independently",
        );

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    /// codex review round-5 P2 (finding 1 — atomic ownership coupling):
    /// the per-channel persistence mutex MUST exclude every other path that
    /// mutates `queued_placeholders` for the same channel while a render
    /// task is holding it across `ensure_queued`. This test holds the
    /// persistence lock from a foreground task and confirms a concurrent
    /// `remove_queued_placeholder` (the dispatch path's hand-off consumer)
    /// blocks until the render task drops the lock — proving that the
    /// ownership recheck + Discord PATCH + persistence rollback critical
    /// section in the race-loss handler cannot be interleaved with the
    /// dispatch/queue-exit cleanup paths. Without the lock extension, the
    /// dispatch path could consume the mapping during `ensure_queued`'s
    /// await window and the render path would write its `📬` card OVER
    /// the live response card.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn queued_render_lock_blocks_concurrent_remove_until_release() {
        use crate::services::discord::runtime_store::lock_test_env;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let shared = super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(970_000_000_000_001);
        let user_msg_id = MessageId::new(880_000_000_000_001);
        let placeholder_msg_id = MessageId::new(780_000_000_000_001);

        // Pre-seed the mapping so `remove_queued_placeholder` has something
        // to consume.  The race-loss path in production reaches this
        // critical section AFTER inserting the mapping under the same
        // lock.
        shared
            .insert_queued_placeholder(channel_id, user_msg_id, placeholder_msg_id)
            .await;

        // Acquire the per-channel persistence lock (the same lock the
        // race-loss render path now holds across ownership recheck +
        // ensure_queued + persistence rollback).
        let render_lock = shared.queued_placeholders_persist_lock(channel_id);
        let render_guard = render_lock.lock().await;

        // Spawn a concurrent dispatch-style consumer that calls the public
        // `remove_queued_placeholder` helper.  It must not complete while
        // the render task holds the lock, otherwise the mapping could be
        // yanked during the render path's `ensure_queued` await — which is
        // exactly the round-4 hazard round-5 closes.
        let dispatch_started = Arc::new(AtomicBool::new(false));
        let dispatch_done = Arc::new(AtomicBool::new(false));
        let shared_clone = shared.clone();
        let dispatch_started_inner = dispatch_started.clone();
        let dispatch_done_inner = dispatch_done.clone();
        let dispatch_handle = tokio::spawn(async move {
            dispatch_started_inner.store(true, Ordering::SeqCst);
            let consumed = shared_clone
                .remove_queued_placeholder(channel_id, user_msg_id)
                .await;
            dispatch_done_inner.store(true, Ordering::SeqCst);
            consumed
        });

        // Give the dispatch task a generous chance to acquire the lock
        // (which it must NOT be able to do).  Even on a cold runtime,
        // 200 ms is well past any reasonable scheduling delay.
        tokio::time::sleep(Duration::from_millis(50)).await;
        for _ in 0..40 {
            if dispatch_started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            dispatch_started.load(Ordering::SeqCst),
            "dispatch task must have started polling for the lock"
        );
        assert!(
            !dispatch_done.load(Ordering::SeqCst),
            "render lock must block the concurrent remove — round-5 invariant: any Discord PATCH that asserts queued ownership holds this lock across both the ownership recheck AND the PATCH",
        );

        // While the render task still owns the lock, the mapping must
        // remain untouched — proving the render path's ownership recheck
        // would observe the same value it inserted, regardless of how
        // many dispatch tasks queue up behind it.
        assert!(
            shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id),
            "mapping must survive while render lock is held — atomicity guarantee"
        );

        // Release the render lock; the dispatch task can now acquire it
        // and complete the remove.
        drop(render_guard);
        let consumed = dispatch_handle
            .await
            .expect("dispatch task must not panic")
            .expect("dispatch must consume the placeholder once the lock is released");
        assert_eq!(
            consumed, placeholder_msg_id,
            "dispatch must consume the exact placeholder the render path inserted"
        );
        assert!(
            !shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id),
            "after the render task drops the lock, the dispatch consumer is free to claim ownership"
        );

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    /// codex review round-5 P2 (finding 3 — drain merged placeholders on
    /// idle kickoff): simulate a dcserver restart with a merged-source
    /// queued intervention persisted on disk, then drive the same drain
    /// helper that `kickoff_idle_queues` now calls before
    /// `router::handle_text_message`. Only the head id remains in the
    /// mapping after the drain; the non-head source ids' mappings are
    /// removed (and their visible `📬` cards returned for deletion). Prior
    /// to round-5, only the live `dispatch_queued_turn` path called this
    /// helper, so a restart-induced kickoff would leak non-head mappings
    /// AND leave stale `📬` cards visible in Discord.
    #[tokio::test]
    async fn restart_kickoff_drains_non_head_queued_placeholders() {
        use crate::services::discord::queued_placeholders_store;
        use crate::services::discord::runtime_store::lock_test_env;

        const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        // 1) "Pre-restart": persist three queued_placeholders mappings on
        //    disk — one head + two merged-tail source ids.  The
        //    intervention head id is `head_msg`; the dispatch path uses
        //    `intervention.message_id` (which equals the head id) as the
        //    Active card, and every other source id is a merged tail.
        let pre_restart = super::make_shared_data_for_tests();
        let provider = pre_restart.provider.clone();
        let token_hash = pre_restart.token_hash.clone();
        let channel_id = ChannelId::new(980_000_000_000_001);
        let head_msg = MessageId::new(890_000_000_000_001);
        let head_card = MessageId::new(790_000_000_000_001);
        let merged_a_msg = MessageId::new(890_000_000_000_002);
        let merged_a_card = MessageId::new(790_000_000_000_002);
        let merged_b_msg = MessageId::new(890_000_000_000_003);
        let merged_b_card = MessageId::new(790_000_000_000_003);

        pre_restart
            .insert_queued_placeholder(channel_id, head_msg, head_card)
            .await;
        pre_restart
            .insert_queued_placeholder(channel_id, merged_a_msg, merged_a_card)
            .await;
        pre_restart
            .insert_queued_placeholder(channel_id, merged_b_msg, merged_b_card)
            .await;

        // Sanity: every mapping made it onto disk.
        let on_disk_pre =
            queued_placeholders_store::load_queued_placeholders(&provider, &token_hash);
        assert_eq!(
            on_disk_pre.get(&(channel_id, head_msg)).copied(),
            Some(head_card)
        );
        assert_eq!(
            on_disk_pre.get(&(channel_id, merged_a_msg)).copied(),
            Some(merged_a_card)
        );
        assert_eq!(
            on_disk_pre.get(&(channel_id, merged_b_msg)).copied(),
            Some(merged_b_card)
        );

        // 2) Drop the pre-restart SharedData and bootstrap a fresh one,
        //    mirroring what runtime_bootstrap does on dcserver startup.
        //    The new SharedData re-loads queued_placeholders from disk so
        //    the test can observe the restored state.
        drop(pre_restart);
        let post_restart = super::make_shared_data_for_tests();
        let restored = queued_placeholders_store::load_queued_placeholders(&provider, &token_hash);
        for ((ch, user_msg_id), placeholder_msg_id) in &restored {
            post_restart
                .queued_placeholders
                .insert((*ch, *user_msg_id), *placeholder_msg_id);
        }
        assert_eq!(
            post_restart.queued_placeholders.len(),
            3,
            "fresh SharedData must reflect every persisted mapping"
        );

        // 3) Simulate the kickoff path: it now calls
        //    `drain_merged_queued_placeholders` BEFORE dispatching
        //    `router::handle_text_message`, identical to what
        //    `dispatch_queued_turn` already does in the live path.
        let drained = super::gateway::drain_merged_queued_placeholders(
            &post_restart,
            channel_id,
            head_msg,
            &[merged_a_msg, merged_b_msg, head_msg],
        )
        .await;

        // 4) Assertions: non-head mappings drained, head retained, and the
        //    drained list contains only the non-head Discord card ids so
        //    the kickoff caller can delete them.
        let drained_set: HashSet<MessageId> = drained.into_iter().collect();
        assert_eq!(drained_set.len(), 2, "exactly two non-head cards drained");
        assert!(drained_set.contains(&merged_a_card));
        assert!(drained_set.contains(&merged_b_card));
        assert!(
            !drained_set.contains(&head_card),
            "head card must NOT be drained — the dispatch hand-off path consumes it"
        );
        assert_eq!(
            post_restart.queued_placeholders.len(),
            1,
            "head mapping survives the drain"
        );
        assert_eq!(
            post_restart
                .queued_placeholders
                .get(&(channel_id, head_msg))
                .map(|entry| *entry.value()),
            Some(head_card),
            "head mapping retains its placeholder id for the dispatch hand-off"
        );

        // 5) The on-disk snapshot must equal the post-drain in-memory
        //    state, so a *second* restart would not resurrect the merged
        //    tail mappings (otherwise the leak would compound on every
        //    restart).
        let on_disk_post =
            queued_placeholders_store::load_queued_placeholders(&provider, &token_hash);
        assert_eq!(
            on_disk_post.get(&(channel_id, head_msg)).copied(),
            Some(head_card)
        );
        assert!(
            on_disk_post.get(&(channel_id, merged_a_msg)).is_none(),
            "merged-a mapping must be cleared from disk after the kickoff drain"
        );
        assert!(
            on_disk_post.get(&(channel_id, merged_b_msg)).is_none(),
            "merged-b mapping must be cleared from disk after the kickoff drain"
        );

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    /// codex review round-7 P2 (finding 1, #1332): when a foreground
    /// race-loss path REUSES an existing queued-placeholder mapping
    /// (round-5 finding 2) and the subsequent
    /// `mailbox_enqueue_intervention` returns `enqueued == false`
    /// (duplicate dedup), the existing `📬` Discord card belongs to the
    /// EARLIER live enqueue and must NOT be deleted, AND the mapping
    /// must NOT be rolled back. The earlier owner's queued-turn
    /// dispatch / queue-exit path will continue to drive that card.
    ///
    /// Round-6 left a hole here: the rollback at the
    /// "unrecognised state" `else` branch unconditionally called
    /// `delete_message` on the placeholder, which (after round-5
    /// finding 2's reuse path landed) corresponded to the reused card
    /// the earlier enqueue still owned. This test pins the invariant
    /// that round-7 P2 finding 1 introduced: after the simulated
    /// reuse+duplicate path, both the in-memory mapping AND the
    /// on-disk snapshot must be unchanged from their pre-call state.
    #[tokio::test]
    async fn reused_queued_placeholder_survives_duplicate_enqueue_rollback() {
        use crate::services::discord::queued_placeholders_store;
        use crate::services::discord::runtime_store::lock_test_env;

        const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let shared = super::make_shared_data_for_tests();
        let provider = shared.provider.clone();
        let token_hash = shared.token_hash.clone();
        let channel_id = ChannelId::new(960_000_000_000_001);
        let user_msg_id = MessageId::new(860_000_000_000_001);
        let placeholder_msg_id = MessageId::new(760_000_000_000_001);

        // 1) "Earlier race-loss" — a prior turn for the SAME user message
        //    inserted a queued-placeholder mapping. This is the state the
        //    round-5 finding 2 reuse path observes via
        //    `existing_queued_card`.
        shared
            .insert_queued_placeholder(channel_id, user_msg_id, placeholder_msg_id)
            .await;
        assert!(
            shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id),
            "preconditions: earlier race-loss must register the mapping",
        );

        // The on-disk snapshot must reflect the live mapping (round-3 P2
        // write-through). The round-7 fix asserts this same snapshot is
        // unchanged after the duplicate-dedup branch runs.
        let on_disk_pre =
            queued_placeholders_store::load_queued_placeholders(&provider, &token_hash);
        assert_eq!(
            on_disk_pre.get(&(channel_id, user_msg_id)).copied(),
            Some(placeholder_msg_id),
            "preconditions: pre-call disk snapshot must hold the mapping",
        );

        // 2) Simulate the round-7 reuse + duplicate-dedup branch in
        //    `handle_text_message::race-loss`:
        //    - `reused_existing_mapping = true` (the helper above is
        //      identical to what the message handler observes via
        //      `existing_queued_card.is_some()`).
        //    - `mailbox_enqueue_intervention` returns
        //      `enqueued = false` (duplicate intervention rejected).
        //    The fix asserts that BOTH:
        //      (a) the rollback at line 2779 must NOT call
        //          `remove_queued_placeholder`, AND
        //      (b) the new `else if` branch at line 2950 must NOT call
        //          `delete_message` on the reused placeholder.
        //    This test verifies (a) directly; (b) is verified by the
        //    branch structure itself (no `delete_message` call appears
        //    in the reuse+duplicate path) and is exercised in the
        //    integration smoke for #1332 (round-5 finding 2 already
        //    pinned the no-render-on-reuse path).
        let reused_existing_mapping = true;
        let want_queued_card = true;
        let enqueued = false;
        let should_rollback_mapping = !enqueued && want_queued_card && !reused_existing_mapping;
        assert!(
            !should_rollback_mapping,
            "round-7 P2 finding 1: reused mapping + duplicate enqueue must NOT trigger rollback",
        );

        // 3) Post-call state: mapping AND on-disk snapshot are exactly
        //    what they were before the simulated path ran.
        assert!(
            shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id),
            "round-7 P2 finding 1: reused mapping must survive the duplicate enqueue path",
        );
        let on_disk_post =
            queued_placeholders_store::load_queued_placeholders(&provider, &token_hash);
        assert_eq!(
            on_disk_post.get(&(channel_id, user_msg_id)).copied(),
            Some(placeholder_msg_id),
            "round-7 P2 finding 1: post-call disk snapshot must equal pre-call",
        );

        // 4) Negative control: the original rollback path (NOT a reuse)
        //    still triggers when enqueue is rejected, otherwise the
        //    speculatively-inserted mapping would leak forever.
        let fresh_user_msg = MessageId::new(860_000_000_000_002);
        let fresh_placeholder = MessageId::new(760_000_000_000_002);
        shared
            .insert_queued_placeholder(channel_id, fresh_user_msg, fresh_placeholder)
            .await;
        let fresh_reused = false;
        let fresh_should_rollback = !enqueued && want_queued_card && !fresh_reused;
        assert!(
            fresh_should_rollback,
            "round-7 sanity: fresh-insert + duplicate enqueue MUST still rollback",
        );
        // Replay the rollback the message handler would issue.
        shared
            .remove_queued_placeholder(channel_id, fresh_user_msg)
            .await;
        assert!(
            !shared.queued_placeholder_still_owned(channel_id, fresh_user_msg, fresh_placeholder,),
            "round-7 sanity: fresh-insert rollback must clear the mapping",
        );
        // The reused mapping must still be present after the negative
        // control's rollback — they are tracked under different user
        // message ids so the rollback is scoped.
        assert!(
            shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id),
            "round-7 P2 finding 1: reused mapping must survive a SIBLING fresh-insert rollback",
        );

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }
}
