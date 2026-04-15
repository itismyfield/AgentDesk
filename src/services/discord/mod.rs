mod adk_session;
pub(crate) mod agentdesk_config;
mod commands;
pub(crate) mod config_audit;
mod discord_io;
pub(crate) mod dm_reply_store;
mod formatting;
mod gateway;
mod handoff;
pub(crate) mod health;
mod inflight;
pub(crate) mod internal_api;
pub(crate) mod meeting_orchestrator;
mod metrics;
mod model_catalog;
mod model_picker_interaction;
mod org_schema;
pub(crate) mod org_writer;
mod prompt_builder;
mod queue_io;
mod recovery_engine;
pub(crate) mod restart_report;
mod role_map;
mod router;
mod runtime_bootstrap;
pub mod runtime_store;
mod session_runtime;
pub(crate) mod settings;
pub(crate) mod shared_memory;
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

pub(crate) use meeting_orchestrator as meeting;
pub(in crate::services::discord) use recovery_engine as recovery;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, EditMessage, MessageId, UserId};

use crate::services::agent_protocol::{DEFAULT_ALLOWED_TOOLS, StreamMessage};
use crate::services::claude;
use crate::services::codex;
use crate::services::gemini;
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
use inflight::{
    InflightTurnState, clear_inflight_state, load_inflight_states, save_inflight_state,
};
use prompt_builder::build_system_prompt;
use recovery_engine::restore_inflight_turns;
use restart_report::flush_restart_reports;
use router::handle_event;
use settings::{
    RoleBinding, channel_upload_dir, cleanup_old_uploads, load_bot_settings,
    load_last_remote_profile, load_last_session_path, resolve_role_binding, save_bot_settings,
    validate_bot_channel_routing_with_provider_channel,
};
#[cfg(unix)]
use tmux::{restore_tmux_watchers, tmux_output_watcher};
#[cfg(unix)]
use tmux_reaper::{cleanup_orphan_tmux_sessions, reap_dead_tmux_sessions};
use turn_bridge::{TurnBridgeContext, spawn_turn_bridge, tmux_runtime_paths};

pub(crate) use prompt_builder::DispatchProfile;
pub(crate) use runtime_bootstrap::RunBotContext;
pub(crate) use runtime_bootstrap::run_bot;

use crate::services::turn_orchestrator::{
    CancelActiveTurnResult, CancelQueuedMessageResult, ChannelMailboxSnapshot, ClearChannelResult,
    FinishTurnResult, QueueExitEvent, QueueExitKind, QueuePersistenceContext,
    RecoveryKickoffResult, RequeueInterventionResult, TakeNextSoftResult, load_pending_queues,
    warn_legacy_pending_queue_files,
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

/// Discord message length limit
pub(super) const DISCORD_MSG_LIMIT: usize = 2000;
const UPLOAD_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const UPLOAD_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const SESSION_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60); // 1 hour
const SESSION_MAX_IDLE: Duration = Duration::from_secs(60 * 60); // 1 hour
const SESSION_MAX_ASSISTANT_TURNS: usize = 100;
const SESSION_RECOVERY_CONTEXT_MESSAGES: usize = 10;
const DEAD_SESSION_REAP_INTERVAL: Duration = Duration::from_secs(60); // 1 minute
const RESTART_REPORT_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const DEFERRED_RESTART_POLL_INTERVAL: Duration = Duration::from_secs(10);

pub(super) fn session_retry_context_key(channel_id: ChannelId) -> String {
    format!("session_retry_context:{}", channel_id.get())
}

pub(super) fn should_process_allowed_bot_turn_text(_text: &str) -> bool {
    // All announce bot messages trigger turns — dispatches, agent-to-agent
    // communication, review instructions, and deadlock alerts all need
    // processing.  "검토 전용" / "작업 착수 금지" are instructions for the
    // agent's behavior during the turn, not reasons to skip the turn.
    true
}

pub(in crate::services::discord) fn is_allowed_turn_sender(
    allowed_bot_ids: &[u64],
    author_id: u64,
    author_is_bot: bool,
    text: &str,
) -> bool {
    if allowed_bot_ids.contains(&author_id) {
        return should_process_allowed_bot_turn_text(text);
    }
    !author_is_bot
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

pub(in crate::services::discord) fn recovery_known_message_ids(
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
    if let Some(active_id) = snapshot.active_user_message_id {
        ids.insert(active_id.get());
    }
    ids
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

/// Extend the watchdog deadline for a channel. Returns the new deadline_ms or None if at cap.
pub async fn extend_watchdog_deadline(channel_id: u64, extend_by_secs: u64) -> Option<i64> {
    ChannelMailboxRegistry::global_handle(ChannelId::new(channel_id))?
        .extend_timeout(extend_by_secs)
        .await
}

/// Read and consume the deadline override for a channel (if any).
pub(super) async fn take_watchdog_deadline_override(channel_id: u64) -> Option<i64> {
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
    bootstrap_thread_session, cleanup_git_worktree, create_git_worktree, detect_worktree_conflict,
    provider_handles_channel, resolve_channel_category, resolve_runtime_channel_binding_status,
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
    /// channel_id (string) → persisted model override
    pub(super) channel_model_overrides: std::collections::HashMap<String, String>,
    /// Discord user ID of the registered owner (imprinting auth)
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
            channel_model_overrides: std::collections::HashMap::new(),
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

/// Shared state for the Discord bot — split into independently-lockable groups
pub(super) struct SharedData {
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
    /// Per-channel tmux output watchers for terminal→Discord relay
    pub(super) tmux_watchers: dashmap::DashMap<ChannelId, TmuxWatcherHandle>,
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
    /// Set to true after Discord gateway ready event fires.
    pub(super) bot_connected: std::sync::atomic::AtomicBool,
    /// ISO 8601 timestamp of the last completed turn (for health reporting).
    pub(super) last_turn_at: std::sync::Mutex<Option<String>>,
    /// Per-channel model override, independent of session lifecycle.
    /// Takes priority over role-map model. Cleared via the `/model` picker default option.
    pub(super) model_overrides: dashmap::DashMap<ChannelId, String>,
    /// Channels that must start a fresh provider session on the next turn
    /// because the effective model override changed.
    pub(super) model_session_reset_pending: dashmap::DashSet<ChannelId>,
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
    /// Per-channel turn start time — used for metrics duration calculation.
    pub(super) turn_start_times: dashmap::DashMap<ChannelId, std::time::Instant>,
    /// Cached serenity context for deferred queue drain (set once during ready event).
    pub(super) cached_serenity_ctx: tokio::sync::OnceCell<serenity::Context>,
    /// Cached bot token for deferred queue drain.
    pub(super) cached_bot_token: tokio::sync::OnceCell<String>,
    /// SHA-256 hash of the bot token — used to namespace the pending-queue directory
    /// so that multiple bots sharing the same runtime root cannot steal each other's queues.
    pub(super) token_hash: String,
    /// HTTP API port for self-referencing requests (from config server.port).
    pub(super) api_port: u16,
    /// Shared DB handle for direct dispatch finalization (avoids HTTP round-trip).
    pub(super) db: Option<crate::db::Db>,
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
    fn mailbox(&self, channel_id: ChannelId) -> ChannelMailboxHandle {
        self.mailboxes.handle(channel_id)
    }

    fn health_registry(&self) -> Option<Arc<health::HealthRegistry>> {
        self.health_registry.upgrade()
    }
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
    shared.mailbox(channel_id).cancel_active_turn().await
}

async fn mailbox_has_active_turn(shared: &SharedData, channel_id: ChannelId) -> bool {
    shared.mailbox(channel_id).has_active_turn().await
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

async fn mailbox_clear_recovery_marker(shared: &SharedData, channel_id: ChannelId) {
    shared.mailbox(channel_id).clear_recovery_marker().await;
}

async fn mailbox_enqueue_intervention(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    intervention: Intervention,
) -> bool {
    let result = shared
        .mailbox(channel_id)
        .enqueue(
            intervention,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    result.enqueued
}

fn queue_exit_feedback_emoji(kind: QueueExitKind) -> char {
    match kind {
        QueueExitKind::Cancelled => '🚫',
        QueueExitKind::Expired => '⌛',
        QueueExitKind::Superseded => '⏏',
    }
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

    let Some(ctx) = shared.cached_serenity_ctx.get() else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⚠ QUEUE-FEEDBACK: skipped {} queue exit reaction(s) in channel {} (cached ctx missing)",
            queue_exit_events.len(),
            channel_id
        );
        return;
    };

    for event in queue_exit_events {
        formatting::remove_reaction_raw(&ctx.http, channel_id, event.intervention.message_id, '📬')
            .await;
        formatting::add_reaction_raw(
            &ctx.http,
            channel_id,
            event.intervention.message_id,
            queue_exit_feedback_emoji(event.kind),
        )
        .await;
    }
}

async fn enqueue_internal_followup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reply_message_id: MessageId,
    text: impl Into<String>,
    reason: &'static str,
) -> bool {
    let enqueued = mailbox_enqueue_intervention(
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

    if enqueued {
        schedule_deferred_idle_queue_kickoff(shared.clone(), provider.clone(), channel_id, reason);
    }

    enqueued
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

async fn mailbox_take_next_soft_intervention(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<(Intervention, bool)> {
    let result: TakeNextSoftResult = shared
        .mailbox(channel_id)
        .take_next_soft(queue_persistence_context(shared, provider, channel_id))
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    result
        .intervention
        .map(|intervention| (intervention, result.has_more))
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

async fn mailbox_restart_drain_all(shared: &SharedData, provider: &ProviderKind) -> usize {
    shared
        .mailboxes
        .restart_drain_all(
            provider,
            &shared.token_hash,
            &shared.dispatch_role_overrides,
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

/// Startup catch-up polling: fetch messages that arrived during the restart gap.
/// Uses saved last_message_ids to query Discord REST API for missed messages,
/// filters out bot messages and duplicates, and inserts into intervention queue.
async fn catch_up_missed_messages(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
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
        let Ok(last_id) = last_id_str.trim().parse::<u64>() else {
            continue;
        };

        let channel_id = ChannelId::new(channel_id_raw);
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
        let messages = match channel_id
            .messages(
                http,
                serenity::builder::GetMessages::new()
                    .after(after_msg)
                    .limit(10),
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

        let mut channel_recovered = 0usize;
        let mut max_recovered_id: Option<u64> = None;

        for msg in &messages {
            // Skip system messages (thread creation, slash commands, etc.)
            if !router::should_process_turn_message(msg.kind) {
                continue;
            }
            // Skip own messages
            if Some(msg.author.id.get()) == bot_user_id {
                continue;
            }
            // Skip if already in queue
            if existing_ids.contains(&msg.id.get()) {
                continue;
            }
            // Skip messages older than max_age (use message snowflake timestamp)
            let msg_ts = msg.id.created_at();
            let msg_age = chrono::Utc::now().signed_duration_since(*msg_ts);
            if msg_age.num_seconds() > max_age.as_secs() as i64 {
                continue;
            }
            let text = msg.content.trim();
            if text.is_empty() {
                continue;
            }
            if !is_allowed_turn_sender(&allowed_bot_ids, msg.author.id.get(), msg.author.bot, text)
            {
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
            channel_recovered += 1;
            // Track the newest actually-recovered message for checkpoint
            let mid = msg.id.get();
            if max_recovered_id.map(|m| mid > m).unwrap_or(true) {
                max_recovered_id = Some(mid);
            }
        }

        if channel_recovered > 0 {
            total_recovered += channel_recovered;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🔍 CATCH-UP: recovered {} message(s) for channel {}",
                channel_recovered,
                channel_id
            );
        }

        // Only advance checkpoint if we actually recovered messages
        if let Some(newest) = max_recovered_id {
            shared.last_message_ids.insert(channel_id, newest);
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
                msg.author.id.get(),
                msg.author.bot,
                text,
            ) {
                continue;
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
            if snapshot.cancel_token.is_some() || snapshot.intervention_queue.is_empty() {
                None
            } else {
                Some(channel_id)
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

        if mailbox_has_active_turn(shared, channel_id).await {
            continue;
        }

        let Some((intervention, has_more)) =
            mailbox_take_next_soft_intervention(shared, provider, channel_id).await
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

        if let Err(e) = router::handle_text_message(
            ctx,
            channel_id,
            intervention.message_id,
            intervention.author_id,
            &owner_name,
            &intervention.text,
            shared,
            token,
            true,     // reply_to_user_message
            has_more, // defer_watcher_resume
            false,    // wait_for_completion — don't block, let channels run concurrently
            intervention.merge_consecutive,
            intervention.reply_context.clone(),
            intervention.has_reply_boundary,
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
        ProviderKind::Codex | ProviderKind::Gemini | ProviderKind::Qwen => {
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
        ProviderKind::Claude | ProviderKind::Codex | ProviderKind::Gemini | ProviderKind::Qwen
    )
}

fn provider_home_skill_dir(provider: &ProviderKind, home: &Path) -> Option<std::path::PathBuf> {
    match provider {
        ProviderKind::Claude => Some(home.join(".claude").join("commands")),
        ProviderKind::Codex => Some(home.join(".codex").join("skills")),
        ProviderKind::Gemini => Some(home.join(".gemini").join("skills")),
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
            .filter(|(_, s)| now.duration_since(s.last_active) > SESSION_MAX_IDLE)
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
                    cleanup_git_worktree(wt);
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
        shared.tmux_watchers.remove(&expired_session.channel_id);
    }
    // Record termination audit for cleaned-up sessions
    for expired_session in &expired {
        if let Some(session_key) = expired_session.session_key.as_deref() {
            crate::services::termination_audit::record_termination(
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

#[cfg(test)]
mod tests {
    use super::runtime_bootstrap::discord_gateway_intents;
    use super::{ChannelId, MessageId, UserId};
    use super::{
        DiscordBotSettings, Intervention, InterventionMode, is_allowed_turn_sender,
        recovery_known_message_ids, should_phase2_recover_message,
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
    fn allowed_bot_all_messages_accepted() {
        let allowed_bot_ids = vec![123];
        let review_only = "⚠️ 검토 전용 — 작업 착수 금지";
        let dispatch = "DISPATCH: abc123\n작업 시작";
        let agent_msg = "completion_guard 수정해줘";

        // All announce bot messages trigger turns
        assert!(should_process_allowed_bot_turn_text(review_only));
        assert!(should_process_allowed_bot_turn_text(dispatch));
        assert!(should_process_allowed_bot_turn_text(agent_msg));
        assert!(is_allowed_turn_sender(
            &allowed_bot_ids,
            123,
            false,
            review_only
        ));
        assert!(is_allowed_turn_sender(
            &allowed_bot_ids,
            123,
            false,
            dispatch
        ));
        assert!(is_allowed_turn_sender(
            &allowed_bot_ids,
            123,
            false,
            agent_msg
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
            .find("spawn_startup_thread_map_validation(db, token_for_kickoff.clone());")
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
}
