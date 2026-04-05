mod adk_session;
mod commands;
mod formatting;
mod handoff;
pub(crate) mod health;
mod inflight;
mod meeting;
mod metrics;
mod model_catalog;
mod model_picker_interaction;
mod org_schema;
pub(crate) mod org_writer;
mod prompt_builder;
mod recovery;
pub(crate) mod restart_report;
mod role_map;
mod router;
pub mod runtime_store;
pub(crate) mod settings;
mod shared_memory;
#[cfg(unix)]
mod tmux;
mod turn_bridge;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateAttachment, CreateMessage, EditMessage, MessageId, UserId};

use crate::services::agent_protocol::{DEFAULT_ALLOWED_TOOLS, StreamMessage};
use crate::services::claude;
use crate::services::codex;
use crate::services::gemini;
use crate::services::provider::{CancelToken, ProviderKind, ReadOutputResult};
use crate::services::qwen;
use crate::ui::ai_screen::{self, HistoryItem, HistoryType};

use adk_session::{
    build_adk_session_key, derive_adk_session_info, lookup_pending_dispatch_for_thread,
    parse_dispatch_id, post_adk_session_status,
};
use formatting::{
    BUILTIN_SKILLS, add_reaction_raw, extract_skill_description, format_for_discord,
    format_tool_input, normalize_empty_lines, remove_reaction_raw, send_long_message_raw,
    truncate_str,
};
use handoff::{clear_handoff, load_handoffs, update_handoff_state};
use inflight::{
    InflightTurnState, clear_inflight_state, load_inflight_states, save_inflight_state,
};
use prompt_builder::{DispatchProfile, build_system_prompt};
use recovery::restore_inflight_turns;
use restart_report::flush_restart_reports;
use router::{handle_event, handle_text_message};
use runtime_store::worktrees_root;
use settings::{
    RoleBinding, channel_upload_dir, cleanup_old_uploads, load_bot_settings, resolve_role_binding,
    save_bot_settings, validate_bot_channel_routing,
};
use shared_memory::load_shared_knowledge;
#[cfg(unix)]
use tmux::{
    cleanup_orphan_tmux_sessions, reap_dead_tmux_sessions, restore_tmux_watchers,
    tmux_output_watcher,
};
use turn_bridge::{TurnBridgeContext, spawn_turn_bridge, tmux_runtime_paths};

pub use settings::{
    load_discord_bot_launch_configs, resolve_discord_bot_provider, resolve_discord_token_by_hash,
};

/// Discord message length limit
pub(super) const DISCORD_MSG_LIMIT: usize = 2000;
const MAX_INTERVENTIONS_PER_CHANNEL: usize = 30;
const INTERVENTION_TTL: Duration = Duration::from_secs(10 * 60);
const INTERVENTION_DEDUP_WINDOW: Duration = Duration::from_secs(10);
const UPLOAD_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const UPLOAD_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const SESSION_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60); // 1 hour
const SESSION_MAX_IDLE: Duration = Duration::from_secs(24 * 60 * 60); // 1 day
const DEAD_SESSION_REAP_INTERVAL: Duration = Duration::from_secs(60); // 1 minute
const RESTART_REPORT_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const DEFERRED_RESTART_POLL_INTERVAL: Duration = Duration::from_secs(10);

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

/// Global watchdog deadline overrides, keyed by channel_id.
/// Written by POST /api/turns/{channel_id}/extend-timeout, read by the watchdog loop.
/// Values are Unix timestamp in milliseconds representing the new deadline.
static WATCHDOG_DEADLINE_OVERRIDES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<u64, i64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Extend the watchdog deadline for a channel. Returns the new deadline_ms or None if at cap.
pub fn extend_watchdog_deadline(channel_id: u64, extend_by_secs: u64) -> Option<i64> {
    let extend_ms = extend_by_secs as i64 * 1000;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut map = WATCHDOG_DEADLINE_OVERRIDES.lock().ok()?;
    let current = map.get(&channel_id).copied().unwrap_or(now_ms);
    let new_deadline = std::cmp::max(current, now_ms) + extend_ms;
    // Don't enforce max here — the watchdog will clamp against its own max
    map.insert(channel_id, new_deadline);
    Some(new_deadline)
}

/// Read and consume the deadline override for a channel (if any).
pub(super) fn take_watchdog_deadline_override(channel_id: u64) -> Option<i64> {
    WATCHDOG_DEADLINE_OVERRIDES.lock().ok()?.remove(&channel_id)
}

/// Remove the deadline override for a channel (on turn completion).
pub(super) fn clear_watchdog_deadline_override(channel_id: u64) {
    if let Ok(mut map) = WATCHDOG_DEADLINE_OVERRIDES.lock() {
        map.remove(&channel_id);
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
        println!("  [{ts}] 🔄 Deferred restart: all turns complete, restarting for v{version}...");
        let _ = fs::remove_file(&marker);
        std::process::exit(0);
    }
}

/// Per-channel session state
#[derive(Clone)]
pub(super) struct DiscordSession {
    pub(super) session_id: Option<String>,
    pub(super) current_path: Option<String>,
    pub(super) history: Vec<HistoryItem>,
    pub(super) pending_uploads: Vec<String>,
    pub(super) cleared: bool,
    /// Remote profile name for SSH execution (None = local)
    pub(super) remote_profile_name: Option<String>,
    pub(super) channel_id: Option<u64>,
    pub(super) channel_name: Option<String>,
    pub(super) category_name: Option<String>,
    /// Last time this session was actively used (for TTL cleanup)
    pub(super) last_active: tokio::time::Instant,
    /// If this session runs in a git worktree, store the info here
    pub(super) worktree: Option<WorktreeInfo>,
    /// Restart generation at which this session was created/restored.
    pub(super) born_generation: u64,
}

impl DiscordSession {
    /// Validate `current_path` and return it if it exists on disk.
    /// If the path is stale (deleted), clear `current_path` and `worktree`, log, and return `None`.
    pub(super) fn validated_path(&mut self, channel_id: impl std::fmt::Display) -> Option<String> {
        let current_path = self.current_path.as_ref()?;
        if std::path::Path::new(current_path).is_dir() {
            return Some(current_path.clone());
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] ⚠ Ignoring stale local session path for channel {}: {}",
            channel_id, current_path
        );
        self.current_path = None;
        self.worktree = None;
        None
    }
}

/// Worktree info for sessions that were auto-redirected to avoid conflicts
#[derive(Clone, Debug)]
pub(super) struct WorktreeInfo {
    /// The original repo path that was conflicted
    pub original_path: String,
    /// The worktree directory path
    pub worktree_path: String,
    /// The branch name created for this worktree
    pub branch_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InterventionMode {
    Soft,
}

#[derive(Clone, Debug)]
pub(super) struct Intervention {
    author_id: UserId,
    message_id: MessageId,
    text: String,
    mode: InterventionMode,
    created_at: Instant,
}

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
    /// channel_id (string) → last working directory path
    pub(super) last_sessions: std::collections::HashMap<String, String>,
    /// channel_id (string) → last remote profile name
    pub(super) last_remotes: std::collections::HashMap<String, String>,
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
            last_sessions: std::collections::HashMap::new(),
            last_remotes: std::collections::HashMap::new(),
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

pub(super) fn synthetic_thread_channel_name(parent_name: &str, channel_id: ChannelId) -> String {
    format!("{parent_name}-t{}", channel_id.get())
}

fn is_synthetic_thread_channel_name(channel_name: &str, channel_id: ChannelId) -> bool {
    channel_name.ends_with(&format!("-t{}", channel_id.get()))
}

fn choose_restore_channel_name(
    existing_channel_name: Option<&str>,
    live_channel_name: Option<&str>,
    thread_parent: Option<(ChannelId, Option<String>)>,
    channel_id: ChannelId,
) -> Option<String> {
    if let Some(existing_name) = existing_channel_name {
        if is_synthetic_thread_channel_name(existing_name, channel_id) {
            return Some(existing_name.to_string());
        }
    }

    if let Some((parent_id, parent_name)) = thread_parent {
        let parent_name = parent_name.unwrap_or_else(|| parent_id.get().to_string());
        return Some(synthetic_thread_channel_name(&parent_name, channel_id));
    }

    live_channel_name
        .or(existing_channel_name)
        .map(ToOwned::to_owned)
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
    pub(super) sessions: HashMap<ChannelId, DiscordSession>,
    /// Per-channel cancel tokens for in-progress AI requests
    pub(super) cancel_tokens: HashMap<ChannelId, Arc<CancelToken>>,
    /// Per-channel owner of the currently running request
    pub(super) active_request_owner: HashMap<ChannelId, UserId>,
    /// Per-channel message queue: messages arriving during an active turn are queued here
    /// and executed as subsequent turns after the current one finishes.
    pub(super) intervention_queue: HashMap<ChannelId, Vec<Intervention>>,
    /// Per-channel active meeting (one meeting per channel)
    active_meetings: HashMap<ChannelId, meeting::Meeting>,
}

/// Shared state for the Discord bot — split into independently-lockable groups
pub(super) struct SharedData {
    /// Core state (sessions + request lifecycle) — requires atomic access
    pub(super) core: Mutex<CoreState>,
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
    /// HTTP API port for self-referencing requests (from config server.port).
    pub(super) api_port: u16,
    /// Shared DB handle for direct dispatch finalization (avoids HTTP round-trip).
    pub(super) db: Option<crate::db::Db>,
    /// Shared policy engine for direct dispatch finalization.
    pub(super) engine: Option<crate::engine::PolicyEngine>,
    /// Set of registered slash command names (populated at framework setup).
    /// Used by the router to distinguish known slash commands from arbitrary
    /// `/`-prefixed user text that should fall through to the AI provider.
    pub(super) known_slash_commands: tokio::sync::OnceCell<std::collections::HashSet<String>>,
}

/// Poise user data type
pub(super) struct Data {
    pub(super) shared: Arc<SharedData>,
    pub(super) token: String,
    pub(super) provider: ProviderKind,
}

pub(super) type Error = Box<dyn std::error::Error + Send + Sync>;
pub(super) type Context<'a> = poise::Context<'a, Data, Error>;

fn prune_interventions(queue: &mut Vec<Intervention>) {
    let now = Instant::now();
    queue.retain(|i| now.duration_since(i.created_at) <= INTERVENTION_TTL);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue.drain(0..overflow);
    }
}

fn enqueue_intervention(queue: &mut Vec<Intervention>, intervention: Intervention) -> bool {
    prune_interventions(queue);

    if let Some(last) = queue.last() {
        if last.author_id == intervention.author_id
            && last.text == intervention.text
            && intervention.created_at.duration_since(last.created_at) <= INTERVENTION_DEDUP_WINDOW
        {
            return false;
        }
    }

    queue.push(intervention);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue.drain(0..overflow);
    }
    true
}

pub(super) fn has_soft_intervention(queue: &mut Vec<Intervention>) -> bool {
    prune_interventions(queue);
    queue.iter().any(|item| item.mode == InterventionMode::Soft)
}

pub(super) fn dequeue_next_soft_intervention(
    queue: &mut Vec<Intervention>,
) -> Option<Intervention> {
    prune_interventions(queue);
    let index = queue
        .iter()
        .position(|item| item.mode == InterventionMode::Soft)?;
    Some(queue.remove(index))
}

pub(super) fn requeue_intervention_front(
    queue: &mut Vec<Intervention>,
    intervention: Intervention,
) {
    prune_interventions(queue);
    queue.insert(0, intervention);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        queue.truncate(MAX_INTERVENTIONS_PER_CHANNEL);
    }
}

// ─── Pending queue persistence (write-through + SIGTERM) ─────────────────────

/// Serializable form of a queued intervention for disk persistence.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct PendingQueueItem {
    pub(super) author_id: u64,
    pub(super) message_id: u64,
    pub(super) text: String,
}

/// Write-through: save a single channel's queue to disk.
/// If the queue is empty the file is removed.
/// This is designed to be called from `tokio::spawn` after every enqueue/dequeue.
pub(super) fn save_channel_queue(
    provider: &ProviderKind,
    channel_id: ChannelId,
    queue: &[Intervention],
) {
    let Some(root) = runtime_store::discord_pending_queue_root() else {
        return;
    };
    let dir = root.join(provider.as_str());
    let path = dir.join(format!("{}.json", channel_id.get()));
    if queue.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    let _ = fs::create_dir_all(&dir);
    let items: Vec<PendingQueueItem> = queue
        .iter()
        .map(|i| PendingQueueItem {
            author_id: i.author_id.get(),
            message_id: i.message_id.get(),
            text: i.text.clone(),
        })
        .collect();
    if let Ok(json) = serde_json::to_string_pretty(&items) {
        let _ = runtime_store::atomic_write(&path, &json);
    }
}

/// Save all non-empty intervention queues to `discord_pending_queue/{provider}/`.
fn save_pending_queues(provider: &ProviderKind, queues: &HashMap<ChannelId, Vec<Intervention>>) {
    let Some(root) = runtime_store::discord_pending_queue_root() else {
        return;
    };
    let dir = root.join(provider.as_str());
    let _ = fs::create_dir_all(&dir);
    // Clean stale files first
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let _ = fs::remove_file(entry.path());
        }
    }
    for (channel_id, queue) in queues {
        if queue.is_empty() {
            continue;
        }
        let items: Vec<PendingQueueItem> = queue
            .iter()
            .map(|i| PendingQueueItem {
                author_id: i.author_id.get(),
                message_id: i.message_id.get(),
                text: i.text.clone(),
            })
            .collect();
        if let Ok(json) = serde_json::to_string_pretty(&items) {
            let path = dir.join(format!("{}.json", channel_id.get()));
            let _ = runtime_store::atomic_write(&path, &json);
        }
    }
}

/// Load persisted pending queues and delete the files.
fn load_pending_queues(provider: &ProviderKind) -> HashMap<ChannelId, Vec<Intervention>> {
    let Some(root) = runtime_store::discord_pending_queue_root() else {
        return HashMap::new();
    };
    let dir = root.join(provider.as_str());
    let Ok(entries) = fs::read_dir(&dir) else {
        return HashMap::new();
    };
    let now = Instant::now();
    let mut result: HashMap<ChannelId, Vec<Intervention>> = HashMap::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let channel_id: u64 = match path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse().ok())
        {
            Some(id) => id,
            None => continue,
        };
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(items) = serde_json::from_str::<Vec<PendingQueueItem>>(&content) else {
            let _ = fs::remove_file(&path);
            continue;
        };
        let interventions: Vec<Intervention> = items
            .into_iter()
            .map(|item| Intervention {
                author_id: UserId::new(item.author_id),
                message_id: MessageId::new(item.message_id),
                text: item.text,
                mode: InterventionMode::Soft,
                created_at: now,
            })
            .collect();
        if !interventions.is_empty() {
            result.insert(ChannelId::new(channel_id), interventions);
        }
        let _ = fs::remove_file(&path);
    }
    result
}

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

    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut total_recovered = 0usize;
    let now = Instant::now();
    let max_age = std::time::Duration::from_secs(300); // Only catch up messages within 5 minutes

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
                eprintln!(
                    "  [{ts}] ⚠ catch-up: failed to fetch messages for channel {channel_id}: {e}"
                );
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
        let existing_ids: std::collections::HashSet<u64> = {
            let data = shared.core.lock().await;
            data.intervention_queue
                .get(&channel_id)
                .map(|q| q.iter().map(|i| i.message_id.get()).collect())
                .unwrap_or_default()
        };

        let allowed_bot_ids: Vec<u64> = {
            let settings = shared.settings.read().await;
            settings.allowed_bot_ids.clone()
        };

        let mut channel_recovered = 0usize;
        let mut max_recovered_id: Option<u64> = None;
        let mut data = shared.core.lock().await;
        let queue = data.intervention_queue.entry(channel_id).or_default();

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
            // Only process messages from allowed bots or authorized users
            let is_allowed = !msg.author.bot || allowed_bot_ids.contains(&msg.author.id.get());
            if !is_allowed {
                continue;
            }

            queue.push(Intervention {
                author_id: msg.author.id,
                message_id: msg.id,
                text: text.to_string(),
                mode: InterventionMode::Soft,
                created_at: now,
            });
            channel_recovered += 1;
            // Track the newest actually-recovered message for checkpoint
            let mid = msg.id.get();
            if max_recovered_id.map(|m| mid > m).unwrap_or(true) {
                max_recovered_id = Some(mid);
            }
        }
        drop(data);

        if channel_recovered > 0 {
            total_recovered += channel_recovered;
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 🔍 CATCH-UP: recovered {} message(s) for channel {}",
                channel_recovered, channel_id
            );
        }

        // Only advance checkpoint if we actually recovered messages
        if let Some(newest) = max_recovered_id {
            shared.last_message_ids.insert(channel_id, newest);
        }
    }

    if total_recovered > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
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

        // Fetch last 20 messages (newest first — default Discord order)
        let recent = match channel_id
            .messages(http, serenity::builder::GetMessages::new().limit(20))
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!(
                    "  [{ts}] ⚠ catch-up phase2: failed to fetch recent messages for channel {channel_id}: {e}"
                );
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
        let existing_ids: std::collections::HashSet<u64> = {
            let data = shared.core.lock().await;
            data.intervention_queue
                .get(&channel_id)
                .map(|q| q.iter().map(|i| i.message_id.get()).collect())
                .unwrap_or_default()
        };

        let mut channel_recovered = 0usize;
        let mut data = shared.core.lock().await;
        let queue = data.intervention_queue.entry(channel_id).or_default();

        // Iterate in reverse (oldest first) for chronological queue order
        for msg in unanswered_slice.iter().rev() {
            if !router::should_process_turn_message(msg.kind) {
                continue;
            }
            if Some(msg.author.id.get()) == bot_user_id_phase2 {
                continue;
            }
            if existing_ids.contains(&msg.id.get()) {
                continue;
            }
            let text = msg.content.trim();
            if text.is_empty() {
                continue;
            }
            let is_allowed =
                !msg.author.bot || allowed_bot_ids_phase2.contains(&msg.author.id.get());
            if !is_allowed {
                continue;
            }
            // Skip messages older than 10 minutes (generous window for restart gap)
            let msg_age = chrono::Utc::now().signed_duration_since(*msg.id.created_at());
            if msg_age.num_seconds() > 600 {
                continue;
            }

            queue.push(Intervention {
                author_id: msg.author.id,
                message_id: msg.id,
                text: text.to_string(),
                mode: InterventionMode::Soft,
                created_at: now,
            });
            channel_recovered += 1;
        }
        drop(data);

        if channel_recovered > 0 {
            phase2_recovered += channel_recovered;
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 🔍 CATCH-UP phase2: recovered {} unanswered message(s) for channel {}",
                channel_recovered, channel_id
            );
        }
    }

    if phase2_recovered > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] 🔍 CATCH-UP phase2: total {phase2_recovered} unanswered message(s) recovered"
        );
    }
}

/// Execute durable handoff turns saved before a restart.
/// Runs after tmux watcher restore and pending queue restore, but before
/// restart report flush. Skips channels that already have pending queue messages
/// (user intent takes priority over automatic follow-up).
async fn execute_handoff_turns(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let handoffs = load_handoffs(provider);
    if handoffs.is_empty() {
        return;
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] 📎 Found {} handoff record(s) to process",
        handoffs.len()
    );

    let current_gen = runtime_store::load_generation();

    for record in handoffs {
        let channel_id = ChannelId::new(record.channel_id);
        let ts = chrono::Local::now().format("%H:%M:%S");

        // Skip if from a different generation (stale)
        if record.born_generation > current_gen {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} (future generation {})",
                record.channel_id, record.born_generation
            );
            clear_handoff(provider, record.channel_id);
            continue;
        }

        // Skip if already executed/skipped/failed
        if record.state != "created" {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} (state={})",
                record.channel_id, record.state
            );
            clear_handoff(provider, record.channel_id);
            continue;
        }

        // Skip if pending queue messages exist (user intent takes priority)
        let has_pending = {
            let data = shared.core.lock().await;
            data.intervention_queue
                .get(&channel_id)
                .map(|q| !q.is_empty())
                .unwrap_or(false)
        };
        if has_pending {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} (pending queue has messages)",
                record.channel_id
            );
            let _ = update_handoff_state(provider, record.channel_id, "skipped");
            clear_handoff(provider, record.channel_id);
            continue;
        }

        // Skip if an active turn is already running
        let has_active = {
            let data = shared.core.lock().await;
            data.cancel_tokens.contains_key(&channel_id)
        };
        if has_active {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} (active turn running)",
                record.channel_id
            );
            let _ = update_handoff_state(provider, record.channel_id, "skipped");
            clear_handoff(provider, record.channel_id);
            continue;
        }

        // Check session/path readiness
        let has_session = {
            let data = shared.core.lock().await;
            data.sessions
                .get(&channel_id)
                .and_then(|s| s.current_path.as_ref())
                .is_some()
        };
        if !has_session {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} (no active session)",
                record.channel_id
            );
            let _ = update_handoff_state(provider, record.channel_id, "skipped");
            clear_handoff(provider, record.channel_id);
            continue;
        }

        // Mark as executing
        let _ = update_handoff_state(provider, record.channel_id, "executing");
        println!(
            "  [{ts}] ▶ Executing handoff for channel {} — {}",
            record.channel_id, record.intent
        );

        // Send a placeholder message in the channel
        let handoff_prompt = format!(
            "dcserver가 재시작되었습니다. 재시작 전 작업의 후속 조치를 이어서 진행해주세요.\n\n\
             ## 재시작 전 컨텍스트\n{}\n\n\
             ## 요청 사항\n{}",
            record.context, record.intent
        );

        let placeholder = match channel_id
            .send_message(
                http,
                serenity::CreateMessage::new().content(
                    "📎 **Post-restart handoff** — 재시작 후속 작업을 자동으로 이어받습니다.",
                ),
            )
            .await
        {
            Ok(msg) => msg,
            Err(e) => {
                println!(
                    "  [{ts}] ❌ Failed to send handoff placeholder for channel {}: {}",
                    record.channel_id, e
                );
                let _ = update_handoff_state(provider, record.channel_id, "failed");
                clear_handoff(provider, record.channel_id);
                continue;
            }
        };

        // Inject as an intervention so the next turn picks it up.
        {
            let mut data = shared.core.lock().await;
            let queue = data.intervention_queue.entry(channel_id).or_default();
            queue.push(Intervention {
                author_id: serenity::UserId::new(1), // system-generated sentinel
                message_id: placeholder.id,
                text: handoff_prompt,
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
            });
        }

        let _ = update_handoff_state(provider, record.channel_id, "completed");
        clear_handoff(provider, record.channel_id);
        println!(
            "  [{ts}] ✓ Handoff queued for channel {} (injected as intervention)",
            record.channel_id
        );
    }
}

/// #164: Re-deliver orphan pending dispatches after dcserver restart.
///
/// After a restart, dispatches in `pending` status may have been Discord-notified
/// but the in-memory intervention_queue was lost. Or the notification was interrupted
/// mid-flight. This function identifies truly orphan dispatches and re-delivers them.
///
/// **Safety**:
/// - Process-global once guard via `std::sync::Once` — safe across multiple provider instances
/// - Startup boot timestamp from dcserver.pid mtime — not wall clock
/// - Newer-dispatch check uses rowid (monotonic) instead of created_at (second-granularity)
/// - Five AND conditions must ALL be met before re-delivery (see issue #164)
async fn recover_orphan_pending_dispatches(shared: &Arc<SharedData>) {
    // Process-global once guard: prevents duplicate execution when multiple
    // provider instances (Claude + Codex) call this from their own setup paths.
    static ONCE: std::sync::Once = std::sync::Once::new();
    let mut should_run = false;
    ONCE.call_once(|| should_run = true);
    if !should_run {
        return;
    }

    let db = match shared.db.as_ref() {
        Some(d) => d,
        None => return,
    };

    // Boot timestamp from dcserver.pid mtime — represents actual process start,
    // not a wall-clock offset that could mis-classify old pending dispatches.
    let boot_time: String = {
        let pid_path =
            crate::cli::agentdesk_runtime_root().map(|r| r.join("runtime").join("dcserver.pid"));
        let mtime = pid_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
        match mtime {
            Some(t) => {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.format("%Y-%m-%d %H:%M:%S").to_string()
            }
            None => {
                // No pid file — cannot determine boot time safely, skip recovery
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ⚠ #164: No dcserver.pid — skipping orphan dispatch recovery");
                return;
            }
        }
    };

    // Query orphan pending dispatches with all 5 safety conditions:
    // 1. status = 'pending'
    // 2. card is assigned to the dispatch target agent
    // 3. agent has NO working session (idle)
    // 4. created_at < boot_time (pre-restart, using pid mtime)
    // 5. no newer dispatch for the same card (using rowid for monotonic ordering,
    //    avoids same-second ambiguity with created_at)
    let orphans: Vec<(String, String, String, String, String)> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut stmt = match conn.prepare(
            "SELECT d.id, d.to_agent_id, d.kanban_card_id, d.title, d.dispatch_type
             FROM task_dispatches d
             JOIN kanban_cards kc ON kc.id = d.kanban_card_id
             WHERE d.status = 'pending'
               AND d.created_at < ?1
               AND kc.assigned_agent_id = d.to_agent_id
               AND NOT EXISTS (
                 SELECT 1 FROM sessions s
                 WHERE s.agent_id = d.to_agent_id
                   AND s.status = 'working'
               )
               AND NOT EXISTS (
                 SELECT 1 FROM task_dispatches d2
                 WHERE d2.kanban_card_id = d.kanban_card_id
                   AND d2.rowid > d.rowid
                   AND d2.status NOT IN ('cancelled', 'failed')
               )",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map([&boot_time], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };

    if orphans.is_empty() {
        return;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] 🔄 #164: Found {} orphan pending dispatch(es) to re-deliver",
        orphans.len()
    );

    let mut delivered = 0usize;
    for (dispatch_id, agent_id, card_id, title, dtype) in &orphans {
        // Clear any existing dispatch_notified marker — the 5-condition query already
        // validated this dispatch is truly orphan, so the marker (if any) is stale.
        {
            let conn = match db.lock() {
                Ok(c) => c,
                Err(_) => continue,
            };
            conn.execute(
                "DELETE FROM kv_meta WHERE key = ?1",
                [&format!("dispatch_notified:{dispatch_id}")],
            )
            .ok();
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}]   ↻ Re-delivering {dtype} dispatch {id} → {agent} (card {card})",
            id = &dispatch_id[..8],
            agent = agent_id,
            card = &card_id[..8.min(card_id.len())],
        );

        // send_dispatch_to_discord handles its own two-phase delivery guard
        // (reserving → send → notified), so no manual marker management needed here.
        match crate::server::routes::dispatches::send_dispatch_to_discord(
            db,
            agent_id,
            title,
            card_id,
            dispatch_id,
        )
        .await
        {
            Ok(()) => {
                delivered += 1;
            }
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}]   ⚠ Recovery delivery failed for {id}: {e}",
                    id = &dispatch_id[..8],
                );
            }
        }
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] ✓ #164: Re-delivered {delivered}/{} orphan dispatch(es)",
        orphans.len()
    );
}

/// Kick off turns for channels that have queued interventions but no active
/// turn running. This bridges the gap where restored pending queues or
/// handoff injections sit idle because no turn-completion event triggers
/// the dequeue chain.
async fn kickoff_idle_queues(
    ctx: &serenity::Context,
    shared: &Arc<SharedData>,
    token: &str,
    provider: &ProviderKind,
) {
    // Collect channels with queued items that are idle (no active turn)
    let channels_to_kick: Vec<(ChannelId, Intervention, bool)> = {
        let mut data = shared.core.lock().await;
        let mut result = Vec::new();
        let channel_ids: Vec<ChannelId> = data.intervention_queue.keys().cloned().collect();
        for channel_id in channel_ids {
            // Skip if active turn already running — it will dequeue when done
            if data.cancel_tokens.contains_key(&channel_id) {
                continue;
            }
            if let Some(queue) = data.intervention_queue.get_mut(&channel_id) {
                if let Some(intervention) = dequeue_next_soft_intervention(queue) {
                    let has_more = has_soft_intervention(queue);
                    // Write-through: update disk after dequeue
                    if queue.is_empty() {
                        save_channel_queue(provider, channel_id, &[]);
                        data.intervention_queue.remove(&channel_id);
                    } else {
                        save_channel_queue(provider, channel_id, queue);
                    }
                    result.push((channel_id, intervention, has_more));
                }
            }
        }
        result
    };

    if channels_to_kick.is_empty() {
        return;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] 🚀 KICKOFF: starting turns for {} idle channel(s) with queued messages",
        channels_to_kick.len()
    );

    for (channel_id, intervention, has_more) in channels_to_kick {
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
        println!(
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
            None,     // reply_context
        )
        .await
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}]   ⚠ KICKOFF: failed to start turn for channel {}: {e}",
                channel_id
            );
            // Requeue so the message is not lost
            let mut data = shared.core.lock().await;
            let queue = data.intervention_queue.entry(channel_id).or_default();
            requeue_intervention_front(queue, intervention);
        }
    }
}

/// Scan for provider-specific skills available to this bot.
pub(super) fn scan_skills(
    provider: &ProviderKind,
    project_path: Option<&str>,
) -> Vec<(String, String)> {
    let mut skills: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    match provider {
        ProviderKind::Claude => {
            for (name, desc) in BUILTIN_SKILLS {
                seen.insert(name.to_string());
                skills.push((name.to_string(), desc.to_string()));
            }

            let mut dirs_to_scan: Vec<std::path::PathBuf> = Vec::new();
            if let Some(home) = dirs::home_dir() {
                dirs_to_scan.push(home.join(".claude").join("commands"));
            }
            if let Some(proj) = project_path {
                dirs_to_scan.push(Path::new(proj).join(".claude").join("commands"));
            }

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
        ProviderKind::Codex => {
            let mut roots = Vec::new();
            if let Some(home) = dirs::home_dir() {
                roots.push(home.join(".codex").join("skills"));
            }
            if let Some(proj) = project_path {
                roots.push(Path::new(proj).join(".codex").join("skills"));
            }

            for root in roots {
                if !root.is_dir() {
                    continue;
                }
                let Ok(entries) = fs::read_dir(&root) else {
                    continue;
                };
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if let Some(skill_path) = resolve_codex_skill_file(&path) {
                        if let Some(name) = skill_path
                            .parent()
                            .and_then(|p| p.file_name())
                            .and_then(|s| s.to_str())
                        {
                            let name = name.to_string();
                            if seen.insert(name.clone()) {
                                let desc = fs::read_to_string(&skill_path)
                                    .ok()
                                    .map(|content| extract_skill_description(&content))
                                    .unwrap_or_else(|| format!("Skill: {}", name));
                                skills.push((name, desc));
                            }
                        }
                        continue;
                    }

                    if path.is_dir() {
                        let Ok(nested) = fs::read_dir(&path) else {
                            continue;
                        };
                        for child in nested.filter_map(|e| e.ok()) {
                            let child_path = child.path();
                            let Some(skill_path) = resolve_codex_skill_file(&child_path) else {
                                continue;
                            };
                            let Some(name) = skill_path
                                .parent()
                                .and_then(|p| p.file_name())
                                .and_then(|s| s.to_str())
                            else {
                                continue;
                            };
                            let name = name.to_string();
                            if seen.insert(name.clone()) {
                                let desc = fs::read_to_string(&skill_path)
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
        ProviderKind::Gemini => {
            let mut roots = Vec::new();
            if let Some(home) = dirs::home_dir() {
                roots.push(home.join(".gemini").join("skills"));
            }
            if let Some(proj) = project_path {
                roots.push(Path::new(proj).join(".gemini").join("skills"));
            }

            for root in roots {
                if !root.is_dir() {
                    continue;
                }
                let Ok(entries) = fs::read_dir(&root) else {
                    continue;
                };
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if let Some(skill_path) = resolve_codex_skill_file(&path) {
                        if let Some(name) = skill_path
                            .parent()
                            .and_then(|p| p.file_name())
                            .and_then(|s| s.to_str())
                        {
                            let name = name.to_string();
                            if seen.insert(name.clone()) {
                                let desc = fs::read_to_string(&skill_path)
                                    .ok()
                                    .map(|content| extract_skill_description(&content))
                                    .unwrap_or_else(|| format!("Skill: {}", name));
                                skills.push((name, desc));
                            }
                        }
                        continue;
                    }

                    if path.is_dir() {
                        let Ok(nested) = fs::read_dir(&path) else {
                            continue;
                        };
                        for child in nested.filter_map(|e| e.ok()) {
                            let child_path = child.path();
                            let Some(skill_path) = resolve_codex_skill_file(&child_path) else {
                                continue;
                            };
                            let Some(name) = skill_path
                                .parent()
                                .and_then(|p| p.file_name())
                                .and_then(|s| s.to_str())
                            else {
                                continue;
                            };
                            let name = name.to_string();
                            if seen.insert(name.clone()) {
                                let desc = fs::read_to_string(&skill_path)
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
        ProviderKind::Qwen => {
            let mut roots = Vec::new();
            if let Some(home) = dirs::home_dir() {
                roots.push(home.join(".qwen").join("skills"));
            }
            if let Some(proj) = project_path {
                roots.push(Path::new(proj).join(".qwen").join("skills"));
            }

            for root in roots {
                if !root.is_dir() {
                    continue;
                }
                let Ok(entries) = fs::read_dir(&root) else {
                    continue;
                };
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if let Some(skill_path) = resolve_codex_skill_file(&path) {
                        if let Some(name) = skill_path
                            .parent()
                            .and_then(|p| p.file_name())
                            .and_then(|s| s.to_str())
                        {
                            let name = name.to_string();
                            if seen.insert(name.clone()) {
                                let desc = fs::read_to_string(&skill_path)
                                    .ok()
                                    .map(|content| extract_skill_description(&content))
                                    .unwrap_or_else(|| format!("Skill: {}", name));
                                skills.push((name, desc));
                            }
                        }
                        continue;
                    }

                    if path.is_dir() {
                        let Ok(nested) = fs::read_dir(&path) else {
                            continue;
                        };
                        for child in nested.filter_map(|e| e.ok()) {
                            let child_path = child.path();
                            let Some(skill_path) = resolve_codex_skill_file(&child_path) else {
                                continue;
                            };
                            let Some(name) = skill_path
                                .parent()
                                .and_then(|p| p.file_name())
                                .and_then(|s| s.to_str())
                            else {
                                continue;
                            };
                            let name = name.to_string();
                            if seen.insert(name.clone()) {
                                let desc = fs::read_to_string(&skill_path)
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

    let dirs: Vec<std::path::PathBuf> = match provider {
        ProviderKind::Claude => {
            let mut v = Vec::new();
            if let Some(home) = dirs::home_dir() {
                v.push(home.join(".claude").join("commands"));
            }
            v
        }
        ProviderKind::Codex => {
            let mut v = Vec::new();
            if let Some(home) = dirs::home_dir() {
                v.push(home.join(".codex").join("skills"));
            }
            v
        }
        ProviderKind::Gemini => {
            let mut v = Vec::new();
            if let Some(home) = dirs::home_dir() {
                v.push(home.join(".gemini").join("skills"));
            }
            v
        }
        ProviderKind::Qwen => {
            let mut v = Vec::new();
            if let Some(home) = dirs::home_dir() {
                v.push(home.join(".qwen").join("skills"));
            }
            v
        }
        _ => vec![],
    };

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
        let proj_dir = match provider {
            ProviderKind::Claude => Path::new(path).join(".claude").join("commands"),
            ProviderKind::Codex => Path::new(path).join(".codex").join("skills"),
            ProviderKind::Gemini => Path::new(path).join(".gemini").join("skills"),
            ProviderKind::Qwen => Path::new(path).join(".qwen").join("skills"),
            _ => continue,
        };
        if proj_dir.is_dir() {
            walk_mtime(&proj_dir, &mut count, &mut max_mtime);
        }
    }

    (count, max_mtime)
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

/// Entry point: start the Discord bot
pub async fn run_bot(
    token: &str,
    provider: ProviderKind,
    global_active: Arc<std::sync::atomic::AtomicUsize>,
    global_finalizing: Arc<std::sync::atomic::AtomicUsize>,
    shutdown_remaining: Arc<std::sync::atomic::AtomicUsize>,
    health_registry: Arc<health::HealthRegistry>,
    api_port: u16,
    db: Option<crate::db::Db>,
    engine: Option<crate::engine::PolicyEngine>,
) {
    // Initialize debug logging from environment variable
    claude::init_debug_from_env();

    let mut bot_settings = load_bot_settings(token);
    bot_settings.provider = provider.clone();

    match bot_settings.owner_user_id {
        Some(owner_id) => println!("  ✓ Owner: {owner_id}"),
        None => println!("  ⚠ No owner registered — first user will be registered as owner"),
    }

    let initial_skills = scan_skills(&provider, None);
    let skill_count = initial_skills.len();
    println!(
        "  ✓ {} bot ready — Skills loaded: {}",
        provider.display_name(),
        skill_count
    );

    // Cleanup stale Discord uploads on process start
    cleanup_old_uploads(UPLOAD_MAX_AGE);

    let provider_for_shutdown = provider.clone();
    let provider_for_error = provider.clone();

    let restored_model_overrides: Vec<(ChannelId, String)> = bot_settings
        .channel_model_overrides
        .iter()
        .filter_map(|(channel_id, model)| {
            channel_id
                .parse::<u64>()
                .ok()
                .map(|id| (ChannelId::new(id), model.clone()))
        })
        .collect();

    let shared = Arc::new(SharedData {
        core: Mutex::new(CoreState {
            sessions: HashMap::new(),
            cancel_tokens: HashMap::new(),
            active_request_owner: HashMap::new(),
            intervention_queue: HashMap::new(),
            active_meetings: HashMap::new(),
        }),
        settings: tokio::sync::RwLock::new(bot_settings),
        api_timestamps: dashmap::DashMap::new(),
        skills_cache: tokio::sync::RwLock::new(initial_skills),
        tmux_watchers: dashmap::DashMap::new(),
        recovering_channels: dashmap::DashMap::new(),
        shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        finalizing_turns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        current_generation: runtime_store::load_generation(),
        restart_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        reconcile_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        global_active,
        global_finalizing,
        shutdown_remaining,
        shutdown_counted: std::sync::atomic::AtomicBool::new(false),
        intake_dedup: dashmap::DashMap::new(),
        dispatch_thread_parents: dashmap::DashMap::new(),
        bot_connected: std::sync::atomic::AtomicBool::new(false),
        last_turn_at: std::sync::Mutex::new(None),
        model_overrides: {
            let map = dashmap::DashMap::new();
            for (channel_id, model) in &restored_model_overrides {
                map.insert(*channel_id, model.clone());
            }
            map
        },
        model_session_reset_pending: {
            let set = dashmap::DashSet::new();
            for (channel_id, _) in &restored_model_overrides {
                set.insert(*channel_id);
            }
            set
        },
        model_picker_pending: dashmap::DashMap::new(),
        dispatch_role_overrides: dashmap::DashMap::new(),
        last_message_ids: dashmap::DashMap::new(),
        turn_start_times: dashmap::DashMap::new(),
        cached_serenity_ctx: tokio::sync::OnceCell::new(),
        cached_bot_token: tokio::sync::OnceCell::new(),
        api_port,
        db,
        engine,
        known_slash_commands: tokio::sync::OnceCell::new(),
    });

    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] 🔑 dcserver generation: {}",
            shared.current_generation
        );
        if !restored_model_overrides.is_empty() {
            println!(
                "  [{ts}] 🧩 restored model overrides: {} channel(s)",
                restored_model_overrides.len()
            );
        }
    }

    // Register this provider with the health check registry
    health_registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;

    let token_owned = token.to_string();
    let shared_clone = shared.clone();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                commands::cmd_start(),
                commands::cmd_pwd(),
                commands::cmd_status(),
                commands::cmd_inflight(),
                commands::cmd_clear(),
                commands::cmd_stop(),
                commands::cmd_down(),
                commands::cmd_shell(),
                commands::cmd_cc(),
                commands::cmd_metrics(),
                commands::cmd_model(),
                commands::cmd_queue(),
                commands::cmd_health(),
                commands::cmd_allowedtools(),
                commands::cmd_allowed(),
                commands::cmd_debug(),
                commands::cmd_allowall(),
                commands::cmd_adduser(),
                commands::cmd_removeuser(),
                commands::cmd_receipt(),
                commands::cmd_help(),
                commands::cmd_meeting(),
            ],
            command_check: Some(|ctx| {
                Box::pin(async move {
                    let settings_snapshot = { ctx.data().shared.settings.read().await.clone() };
                    let allowed = provider_handles_channel(
                        ctx.serenity_context(),
                        &ctx.data().provider,
                        &settings_snapshot,
                        ctx.channel_id(),
                    )
                    .await;
                    if !allowed {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!(
                            "  [{ts}] ⏭ CMD-GUARD: skipping /{} in channel {} for provider {}",
                            ctx.command().name,
                            ctx.channel_id(),
                            ctx.data().provider.as_str()
                        );
                    }
                    Ok(allowed)
                })
            }),
            event_handler: |ctx, event, _framework, data| Box::pin(handle_event(ctx, event, data)),
            ..Default::default()
        })
        .setup(move |ctx, _ready, framework| {
            let shared_for_migrate = shared_clone.clone();
            let health_registry_for_setup = health_registry.clone();
            let provider_for_setup = provider.clone();
            let token_for_ready = token_owned.clone();
            Box::pin(async move {
                // Register in each guild for instant slash command propagation
                // (register_globally can take up to 1 hour)
                let commands = &framework.options().commands;
                // Populate known slash command names for router fallback logic
                let cmd_names: std::collections::HashSet<String> = commands
                    .iter()
                    .map(|c| c.name.clone())
                    .collect();
                let _ = shared_for_migrate.known_slash_commands.set(cmd_names);
                for guild in &_ready.guilds {
                    if let Err(e) =
                        poise::builtins::register_in_guild(ctx, commands, guild.id).await
                    {
                        eprintln!(
                            "  ⚠ Failed to register commands in guild {}: {}",
                            guild.id, e
                        );
                    }
                }
                println!(
                    "  ✓ Bot connected — Registered commands in {} guild(s)",
                    _ready.guilds.len()
                );
                shared_for_migrate.bot_connected.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = shared_for_migrate.cached_serenity_ctx.set(ctx.clone());
                let _ = shared_for_migrate.cached_bot_token.set(token_for_ready.clone());
                health_registry_for_setup.register_http(provider_for_setup.as_str().to_string(), ctx.http.clone()).await;

                // Enrich role_map.json with channelId for reliable name→ID resolution
                enrich_role_map_with_channel_ids();

                let shared_for_tmux = shared_for_migrate.clone();

                // Background: poll for deferred restart marker when idle
                let shared_for_deferred = shared_for_tmux.clone();
                let provider_for_deferred = provider.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(DEFERRED_RESTART_POLL_INTERVAL).await;
                        // Detect restart_pending marker and set the in-memory flag
                        // so the router queues new messages instead of starting turns.
                        if !shared_for_deferred.restart_pending.load(Ordering::Relaxed) {
                            if let Some(root) = crate::agentdesk_runtime_root() {
                                if root.join("restart_pending").exists() {
                                    shared_for_deferred.restart_pending.store(true, Ordering::SeqCst);
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!("  [{ts}] ⏸ DRAIN: restart_pending detected, entering drain mode — new turns blocked");
                                }
                            }
                        }
                        // Use process-global counters so we wait for ALL providers
                        let g_active = shared_for_deferred.global_active.load(Ordering::Relaxed);
                        let g_finalizing = shared_for_deferred.global_finalizing.load(Ordering::Relaxed);
                        if g_active == 0 && g_finalizing == 0 && shared_for_deferred.restart_pending.load(Ordering::Relaxed) {
                            // Save pending queues before exiting so they survive restart
                            {
                                let data = shared_for_deferred.core.lock().await;
                                let queue_count: usize =
                                    data.intervention_queue.values().map(|q| q.len()).sum();
                                if queue_count > 0 {
                                    save_pending_queues(&provider_for_deferred, &data.intervention_queue);
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!("  [{ts}] 📋 DRAIN: saved {queue_count} pending queue item(s) before deferred restart");
                                }
                            }
                            check_deferred_restart(&shared_for_deferred);
                            // This provider has saved and decremented — stop polling
                            return;
                        }
                    }
                });

                // Background: hot-reload skills on file changes (30s polling)
                // Scans home-level AND all active project-level skill directories.
                let shared_for_skills = shared_for_tmux.clone();
                let provider_for_skills = provider.clone();
                tokio::spawn(async move {
                    let mut last_fingerprint: (usize, u64) = (0, 0); // (file_count, max_mtime_epoch)
                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                        // Collect unique project paths from active sessions
                        let project_paths: Vec<String> = {
                            let data = shared_for_skills.core.lock().await;
                            let mut paths: Vec<String> = data.sessions.values()
                                .filter_map(|s| s.current_path.clone())
                                .collect();
                            paths.sort();
                            paths.dedup();
                            paths
                        };
                        let fp = skill_dir_fingerprint_with_projects(&provider_for_skills, &project_paths);
                        if fp != last_fingerprint && last_fingerprint != (0, 0) {
                            // Merge home + all project skills (scan_skills deduplicates by name)
                            let mut merged = scan_skills(&provider_for_skills, None);
                            let mut seen: std::collections::HashSet<String> =
                                merged.iter().map(|(n, _)| n.clone()).collect();
                            for path in &project_paths {
                                for skill in scan_skills(&provider_for_skills, Some(path)) {
                                    if seen.insert(skill.0.clone()) {
                                        merged.push(skill);
                                    }
                                }
                            }
                            merged.sort_by(|a, b| a.0.cmp(&b.0));
                            let count = merged.len();
                            *shared_for_skills.skills_cache.write().await = merged;
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!("  [{ts}] 🔄 Skills hot-reloaded: {count} skill(s) ({} files, mtime Δ)", fp.0);
                        }
                        last_fingerprint = fp;
                    }
                });

                // Restore inflight turns FIRST, then flush restart reports.
                // Recovery skips channels that have a pending restart report,
                // so the report must still be on disk when recovery runs.
                // After recovery completes, the flush loop starts and delivers/clears reports.
                let http_for_tmux = ctx.http.clone();
                let shared_for_tmux2 = shared_for_tmux.clone();
                let http_for_restart_reports = ctx.http.clone();
                let ctx_for_kickoff = ctx.clone();
                let token_for_kickoff = token_owned.clone();
                let shared_for_restart_reports = shared_for_tmux.clone();
                let provider_for_restore = provider.clone();
                tokio::spawn(async move {
                    gc_stale_fixed_working_sessions(&shared_for_tmux2).await;
                    restore_inflight_turns(&http_for_tmux, &shared_for_tmux2, &provider_for_restore).await;

                    // Restore pending intervention queues saved during previous SIGTERM
                    let restored_queues = load_pending_queues(&provider_for_restore);
                    if !restored_queues.is_empty() {
                        let mut added = 0usize;
                        let mut skipped = 0usize;
                        let mut data = shared_for_tmux2.core.lock().await;
                        for (channel_id, items) in restored_queues {
                            let queue = data.intervention_queue.entry(channel_id).or_default();
                            let existing_ids: std::collections::HashSet<u64> =
                                queue.iter().map(|i| i.message_id.get()).collect();
                            for item in items {
                                if existing_ids.contains(&item.message_id.get()) {
                                    skipped += 1;
                                } else {
                                    queue.push(item);
                                    added += 1;
                                }
                            }
                        }
                        drop(data);
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] 📋 FLUSH: restored {added} pending queue item(s) from disk (skipped {skipped} duplicates)");
                    }

                    // Startup catch-up polling: recover messages lost during restart gap
                    catch_up_missed_messages(
                        &http_for_tmux,
                        &shared_for_tmux2,
                        &provider_for_restore,
                    ).await;

                    // #226: Collect channels that recovery already handled (spawned + ended watchers).
                    // restore_tmux_watchers must skip these to prevent duplicate watcher creation.
                    // The issue: recovery watcher starts → session ends quickly → watcher removes
                    // itself from DashMap → restore_tmux_watchers sees empty slot → creates second watcher.
                    #[cfg(unix)]
                    {
                        // Mark all channels that recovery touched as "recently handled"
                        // by inserting a recovery_handled marker in kv_meta.
                        // restore_tmux_watchers checks this and skips those channels.
                        if let Some(ref db) = shared_for_tmux2.db {
                            if let Ok(conn) = db.lock() {
                                let recovery_channels: Vec<u64> = shared_for_tmux2
                                    .recovering_channels
                                    .iter()
                                    .map(|entry| entry.key().get())
                                    .collect();
                                for ch in &recovery_channels {
                                    conn.execute(
                                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                                        rusqlite::params![
                                            format!("recovery_handled_channel:{ch}"),
                                            chrono::Utc::now().timestamp().to_string(),
                                        ],
                                    )
                                    .ok();
                                }
                            }
                        }

                        restore_tmux_watchers(&http_for_tmux, &shared_for_tmux2).await;
                        cleanup_orphan_tmux_sessions(&shared_for_tmux2).await;

                        // Clean up recovery markers
                        if let Some(ref db) = shared_for_tmux2.db {
                            if let Ok(conn) = db.lock() {
                                conn.execute(
                                    "DELETE FROM kv_meta WHERE key LIKE 'recovery_handled_channel:%'",
                                    [],
                                )
                                .ok();
                            }
                        }
                    }

                    // Execute durable handoffs (post-restart follow-up work)
                    execute_handoff_turns(
                        &http_for_restart_reports,
                        &shared_for_restart_reports,
                        &provider_for_restore,
                    )
                    .await;

                    // #164: Re-deliver orphan pending dispatches from before restart
                    recover_orphan_pending_dispatches(&shared_for_restart_reports).await;

                    // Kick off turns for channels that have queued messages but no
                    // active turn. Without this, restored pending queues and handoff
                    // injections sit idle until the next user message arrives.
                    kickoff_idle_queues(
                        &ctx_for_kickoff,
                        &shared_for_restart_reports,
                        &token_for_kickoff,
                        &provider_for_restore,
                    )
                    .await;

                    // #122: Reconcile phase complete — open intake
                    shared_for_restart_reports
                        .reconcile_done
                        .store(true, std::sync::atomic::Ordering::Release);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ✓ Reconcile complete — intake open");

                    // Kick off again to drain messages queued during reconcile window
                    kickoff_idle_queues(
                        &ctx_for_kickoff,
                        &shared_for_restart_reports,
                        &token_for_kickoff,
                        &provider_for_restore,
                    )
                    .await;

                    // NOW flush restart reports (recovery is done, safe to delete them)
                    flush_restart_reports(
                        &http_for_restart_reports,
                        &shared_for_restart_reports,
                        &provider_for_restore,
                    )
                    .await;
                    // Continue flushing in a loop for any reports created later
                    loop {
                        tokio::time::sleep(RESTART_REPORT_FLUSH_INTERVAL).await;
                        flush_restart_reports(
                            &http_for_restart_reports,
                            &shared_for_restart_reports,
                            &provider_for_restore,
                        )
                        .await;
                    }
                });

                // Background: periodic cleanup for stale Discord upload files
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(UPLOAD_CLEANUP_INTERVAL).await;
                        cleanup_old_uploads(UPLOAD_MAX_AGE);
                    }
                });

                // Background: periodic reaper for dead tmux sessions that
                // still show as working in the DB (catches watcher gaps)
                #[cfg(unix)]
                {
                    let shared_for_reaper = shared_clone.clone();
                    tokio::spawn(async move {
                        // Initial delay: let startup recovery finish first
                        tokio::time::sleep(tokio::time::Duration::from_secs(90)).await;
                        loop {
                            reap_dead_tmux_sessions(&shared_for_reaper).await;
                            tokio::time::sleep(DEAD_SESSION_REAP_INTERVAL).await;
                        }
                    });
                }

                // Background: periodic GC for stale thread sessions in DB
                // (idle/disconnected thread sessions older than 1 hour)
                {
                    let api_port = shared_clone.api_port;
                    let shared_for_session_gc = shared_clone.clone();
                    tokio::spawn(async move {
                        // Run every 10 minutes, initial delay 2 minutes
                        tokio::time::sleep(tokio::time::Duration::from_secs(120)).await;
                        loop {
                            gc_stale_fixed_working_sessions(&shared_for_session_gc).await;
                            gc_stale_thread_sessions_via_api(api_port).await;
                            tokio::time::sleep(tokio::time::Duration::from_secs(600)).await;
                        }
                    });
                }

                Ok(Data {
                    shared: shared_clone,
                    token: token_owned,
                    provider,
                })
            })
        })
        .build();

    let intents = serenity::GatewayIntents::GUILDS
        | serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::DIRECT_MESSAGES
        | serenity::GatewayIntents::MESSAGE_CONTENT;

    let mut client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .await
        .expect("Failed to create Discord client");

    // Graceful shutdown: on SIGTERM, cancel all tmux watchers before dying
    let shared_for_signal = shared.clone();
    let token_for_signal = token.to_string();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                sigterm.recv().await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] 🛑 SIGTERM received — graceful shutdown");

                // Set global shutdown flag
                shared_for_signal
                    .shutting_down
                    .store(true, std::sync::atomic::Ordering::SeqCst);

                // Block dequeue and put router into drain mode so no new
                // queue/checkpoint mutations occur during shutdown.
                shared_for_signal
                    .restart_pending
                    .store(true, std::sync::atomic::Ordering::SeqCst);

                // Cancel all active tmux watchers (quiet exit, no "session ended" messages)
                for entry in shared_for_signal.tmux_watchers.iter() {
                    entry
                        .value()
                        .cancel
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }

                // Grace period for watchers to see cancel flag and exit cleanly.
                // Active turns may also finish during this window.
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

                // ── Critical state persistence (MUST run before any I/O) ──
                // Save pending queues and last_message_ids FIRST, before any
                // network calls that might block/timeout and prevent saving.

                // Persist pending intervention queues so they survive restart
                {
                    let data = shared_for_signal.core.lock().await;
                    let queue_count: usize =
                        data.intervention_queue.values().map(|q| q.len()).sum();
                    if queue_count > 0 {
                        save_pending_queues(&provider_for_shutdown, &data.intervention_queue);
                        let ts3 = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts3}] 📋 saved {queue_count} pending queue item(s) to disk");
                    }
                }

                // Persist last_message_ids for catch-up polling after restart
                {
                    let ids: std::collections::HashMap<u64, u64> = shared_for_signal
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    if !ids.is_empty() {
                        runtime_store::save_all_last_message_ids(
                            provider_for_shutdown.as_str(),
                            &ids,
                        );
                    }
                }

                // ── Inflight state, restart reports & placeholder updates ──
                let inflight_states = inflight::load_inflight_states(&provider_for_shutdown);

                // Save restart reports FIRST (disk-only, guaranteed to complete)
                // before any HTTP calls that might hang/timeout.
                for state in &inflight_states {
                    let existing = restart_report::load_restart_report(
                        &provider_for_shutdown,
                        state.channel_id,
                    );
                    if existing.as_ref().map(|r| r.status.as_str()) == Some("pending") {
                        continue;
                    }
                    let mut report = restart_report::RestartCompletionReport::new(
                        provider_for_shutdown.clone(),
                        state.channel_id,
                        "sigterm",
                        "dcserver가 SIGTERM으로 종료되었습니다. 재시작 후 작업을 이어받습니다.",
                    );
                    report.current_msg_id = Some(state.current_msg_id);
                    report.channel_name = state.channel_name.clone();
                    report.user_msg_id = Some(state.user_msg_id);
                    if let Err(e) = restart_report::save_restart_report(&report) {
                        eprintln!(
                            "  ⚠ failed to save restart report for channel {}: {e}",
                            state.channel_id
                        );
                    }
                }
                if !inflight_states.is_empty() {
                    let ts2 = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts2}] 📝 saved {} restart report(s) for inflight channels",
                        inflight_states.len()
                    );
                }

                // Best-effort: update placeholder messages with restart notice.
                // Each edit gets a 3-second timeout to avoid blocking shutdown.
                if !inflight_states.is_empty() {
                    let http = serenity::Http::new(&token_for_signal);
                    for state in &inflight_states {
                        let channel = ChannelId::new(state.channel_id);
                        let msg_id = MessageId::new(state.current_msg_id);
                        let restart_notice = if state.full_response.trim().is_empty() {
                            "⚠️ dcserver 재시작으로 중단됨 — 곧 복원됩니다".to_string()
                        } else {
                            let partial = formatting::format_for_discord_with_provider(
                                state.full_response.trim(),
                                &provider_for_shutdown,
                            );
                            format!("{partial}\n\n⚠️ dcserver 재시작으로 중단됨 — 곧 복원됩니다")
                        };
                        let edit_fut = channel.edit_message(
                            &http,
                            msg_id,
                            EditMessage::new().content(&restart_notice),
                        );
                        match tokio::time::timeout(tokio::time::Duration::from_secs(3), edit_fut)
                            .await
                        {
                            Ok(Ok(_)) => {
                                let ts_ok = chrono::Local::now().format("%H:%M:%S");
                                println!(
                                    "  [{ts_ok}] ✓ Updated placeholder msg {} in channel {}",
                                    state.current_msg_id, state.channel_id
                                );
                            }
                            Ok(Err(e)) => {
                                eprintln!(
                                    "  ⚠ Failed to update placeholder msg {} in channel {}: {e}",
                                    state.current_msg_id, state.channel_id
                                );
                            }
                            Err(_) => {
                                eprintln!(
                                    "  ⚠ Timeout updating placeholder msg {} in channel {}",
                                    state.current_msg_id, state.channel_id
                                );
                            }
                        }
                    }
                }

                // ── Final state snapshot (belt-and-suspenders) ──
                // During the HTTP placeholder edits above, active turns may have
                // finished and mutated queues/last_message_ids. Re-save to capture
                // any changes that occurred after the initial save.
                {
                    let data = shared_for_signal.core.lock().await;
                    let queue_count: usize =
                        data.intervention_queue.values().map(|q| q.len()).sum();
                    if queue_count > 0 {
                        save_pending_queues(&provider_for_shutdown, &data.intervention_queue);
                        let ts4 = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts4}] 📋 final save: {queue_count} pending queue item(s)");
                    }
                }
                {
                    let ids: std::collections::HashMap<u64, u64> = shared_for_signal
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    if !ids.is_empty() {
                        runtime_store::save_all_last_message_ids(
                            provider_for_shutdown.as_str(),
                            &ids,
                        );
                    }
                }

                // Wait for all providers to finish saving before exiting.
                // CAS guard: skip if this provider already decremented via deferred restart path.
                if shared_for_signal
                    .shutdown_counted
                    .compare_exchange(
                        false,
                        true,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    if shared_for_signal
                        .shutdown_remaining
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
                        == 1
                    {
                        std::process::exit(0);
                    }
                }
            }
        }
    });

    if let Err(e) = client.start().await {
        eprintln!("  ✗ {} bot error: {e}", provider_for_error.display_name());
    }
}

/// Check if a user is authorized (owner or allowed user)
/// Returns true if authorized, false if rejected.
/// On first use, registers the user as owner.
pub(super) async fn check_auth(
    user_id: UserId,
    user_name: &str,
    shared: &Arc<SharedData>,
    token: &str,
) -> bool {
    let mut settings = shared.settings.write().await;
    match settings.owner_user_id {
        None => {
            // Imprint: register first user as owner
            settings.owner_user_id = Some(user_id.get());
            save_bot_settings(token, &settings);
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ★ Owner registered: {user_name} (id:{})",
                user_id.get()
            );
            true
        }
        Some(_) => {
            let uid = user_id.get();
            if user_is_authorized(&settings, uid) {
                true
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ✗ Rejected: {user_name} (id:{})", uid);
                false
            }
        }
    }
}

fn user_is_authorized(settings: &DiscordBotSettings, user_id: u64) -> bool {
    settings.allow_all_users
        || settings.owner_user_id == Some(user_id)
        || settings.allowed_user_ids.contains(&user_id)
}

/// Check if a user is the owner (not just allowed)
pub(super) async fn check_owner(user_id: UserId, shared: &Arc<SharedData>) -> bool {
    let settings = shared.settings.read().await;
    settings.owner_user_id == Some(user_id.get())
}

/// Check for pending DM replies and consume them. The answer text is stored
/// in the consumed row's context (as `_answer`), and a notification is sent
/// to the source agent's Discord channel so its session can process the reply.
pub(super) async fn try_handle_pending_dm_reply(
    db: &crate::db::Db,
    msg: &serenity::Message,
) -> bool {
    if msg.author.bot || msg.guild_id.is_some() {
        return false;
    }
    let answer = msg.content.trim();
    if answer.is_empty() {
        return false;
    }
    let user_id_str = msg.author.id.get().to_string();
    let username = msg.author.name.clone();
    let db = db.clone();
    let answer_owned = answer.to_string();
    let result = tokio::task::spawn_blocking(move || {
        consume_pending_dm_reply(&db, &user_id_str, &answer_owned)
    })
    .await;
    match result {
        Ok(Some(info)) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ✉️ DM reply consumed: user={} agent={} id={}",
                msg.author.id.get(),
                info.source_agent,
                info.id
            );

            // Notify the source agent's Discord channel (inline, not fire-and-forget)
            if let Err(e) = notify_source_agent(
                &info.db,
                &info.source_agent,
                info.id,
                info.channel_id.as_deref(),
                &username,
                &info.answer,
            )
            .await
            {
                eprintln!("  [dm-reply] notify source agent failed: {e}");
                // Record failure in context so readConsumed can detect it
                let db3 = info.db.clone();
                let reply_id = info.id;
                let err_msg = format!("{e}");
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(conn) = db3.separate_conn() {
                        let _ = conn.execute(
                            "UPDATE pending_dm_replies SET context = \
                             json_set(context, '$._notify_failed', json('true'), '$._notify_error', ?1) \
                             WHERE id = ?2",
                            rusqlite::params![err_msg, reply_id],
                        );
                    }
                })
                .await;
            }

            true
        }
        Ok(None) => false,
        Err(e) => {
            eprintln!("  [dm-reply] consume task error: {e}");
            false
        }
    }
}

/// Send a notification to the source agent's Discord channel about the DM reply.
/// Prefers the stored `channel_id` from the pending row (alt/thread channels);
/// falls back to `agents.discord_channel_id` only if none was stored.
async fn notify_source_agent(
    db: &crate::db::Db,
    source_agent: &str,
    reply_id: i64,
    stored_channel_id: Option<&str>,
    username: &str,
    answer: &str,
) -> Result<(), String> {
    let token =
        crate::credential::read_bot_token("announce").ok_or("no announce bot token configured")?;

    // Prefer the stored channel_id from the pending row (supports alt/thread channels)
    let channel_id: u64 = if let Some(ch) = stored_channel_id {
        resolve_channel_to_u64(ch)?
    } else {
        // Fall back to the agent's primary discord_channel_id
        let db = db.clone();
        let agent_name = source_agent.to_string();
        let ch_opt: Option<String> = tokio::task::spawn_blocking(move || {
            let conn = db.separate_conn().map_err(|e| format!("{e}"))?;
            crate::db::agents::resolve_agent_primary_channel_on_conn(&conn, &agent_name)
                .map_err(|e| format!("{e}"))
        })
        .await
        .map_err(|e| format!("join: {e}"))??;
        let raw = ch_opt.ok_or("agent has no discord_channel_id")?;
        resolve_channel_to_u64(&raw)?
    };

    let message = format!("DM_REPLY:{reply_id} from {username}: {answer}");
    send_message_to_channel(&token, channel_id, &message)
        .await
        .map_err(|e| format!("{e}"))?;
    Ok(())
}

/// Parse a channel identifier — numeric ID or name alias (e.g. "윤호네비서") → u64.
fn resolve_channel_to_u64(raw: &str) -> Result<u64, String> {
    raw.parse::<u64>().or_else(|_| {
        crate::server::routes::dispatches::resolve_channel_alias_pub(raw)
            .ok_or_else(|| format!("cannot resolve channel '{raw}'"))
    })
}

/// Retry DM reply notifications that previously failed (`_notify_failed` in context).
/// Called from the 5-min tick loop.
pub async fn retry_failed_dm_notifications(db: &crate::db::Db) {
    let db2 = db.clone();
    let entries: Vec<(i64, String, String, Option<String>)> =
        match tokio::task::spawn_blocking(move || {
            let conn = db2.separate_conn().map_err(|e| format!("{e}"))?;
            let mut stmt = conn
                .prepare(
                    "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
                     WHERE status = 'consumed' AND json_extract(context, '$._notify_failed') IS NOT NULL \
                     LIMIT 10",
                )
                .map_err(|e| format!("{e}"))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .map_err(|e| format!("{e}"))?
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();
            Ok::<_, String>(rows)
        })
        .await
        {
            Ok(Ok(v)) => v,
            _ => return,
        };

    if entries.is_empty() {
        return;
    }

    for (id, source_agent, context_str, channel_id) in entries {
        let ctx: serde_json::Value =
            serde_json::from_str(&context_str).unwrap_or(serde_json::json!({}));
        let answer = ctx
            .get("_answer")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if answer.is_empty() {
            continue;
        }

        match notify_source_agent(
            db,
            &source_agent,
            id,
            channel_id.as_deref(),
            "(retry)",
            &answer,
        )
        .await
        {
            Ok(()) => {
                // Clear _notify_failed on success
                let db3 = db.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(conn) = db3.separate_conn() {
                        let _ = conn.execute(
                            "UPDATE pending_dm_replies SET context = \
                             json_remove(context, '$._notify_failed', '$._notify_error') \
                             WHERE id = ?1",
                            rusqlite::params![id],
                        );
                    }
                })
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ✉️ DM reply retry OK: id={id} agent={source_agent}");
            }
            Err(e) => {
                eprintln!("  [dm-reply] retry still failing id={id}: {e}");
            }
        }
    }
}

struct ConsumedDmReply {
    id: i64,
    source_agent: String,
    answer: String,
    channel_id: Option<String>,
    db: crate::db::Db,
}

fn consume_pending_dm_reply(
    db: &crate::db::Db,
    user_id: &str,
    answer: &str,
) -> Option<ConsumedDmReply> {
    let conn = db.separate_conn().ok()?;
    // FIFO: consume oldest non-expired pending entry
    let row: Result<(i64, String, String, Option<String>), _> = conn.query_row(
        "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
         WHERE user_id = ?1 AND status = 'pending' \
         AND (expires_at IS NULL OR expires_at > datetime('now')) \
         ORDER BY created_at ASC LIMIT 1",
        rusqlite::params![user_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );
    let (id, source_agent, context_str, channel_id) = row.ok()?;

    // Merge the answer into the context JSON
    let mut context: serde_json::Value =
        serde_json::from_str(&context_str).unwrap_or(serde_json::json!({}));
    context["_answer"] = serde_json::Value::String(answer.to_string());
    let updated_context = serde_json::to_string(&context).unwrap_or_default();

    // CAS: only mark consumed if still pending (guards against race)
    let updated = conn.execute(
        "UPDATE pending_dm_replies SET status = 'consumed', consumed_at = datetime('now'), \
         context = ?1 WHERE id = ?2 AND status = 'pending'",
        rusqlite::params![updated_context, id],
    );
    match updated {
        Ok(0) => return None, // already consumed by another path
        Err(_) => return None,
        _ => {}
    }

    Some(ConsumedDmReply {
        id,
        source_agent,
        answer: answer.to_string(),
        channel_id,
        db: db.clone(),
    })
}

/// Rate limit helper — ensures minimum 1s gap between API calls per channel
pub(super) async fn rate_limit_wait(shared: &Arc<SharedData>, channel_id: ChannelId) {
    let min_gap = tokio::time::Duration::from_millis(1000);
    let sleep_until = {
        let now = tokio::time::Instant::now();
        let default_ts = now - tokio::time::Duration::from_secs(10);
        let last_ts = shared
            .api_timestamps
            .get(&channel_id)
            .map(|r| *r.value())
            .unwrap_or(default_ts);
        let earliest_next = last_ts + min_gap;
        let target = if earliest_next > now {
            earliest_next
        } else {
            now
        };
        shared.api_timestamps.insert(channel_id, target);
        target
    };
    tokio::time::sleep_until(sleep_until).await;
}

/// Add a reaction to a message
async fn add_reaction(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
) {
    let reaction = serenity::ReactionType::Unicode(emoji.to_string());
    if let Err(e) = channel_id
        .create_reaction(&ctx.http, message_id, reaction)
        .await
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        eprintln!(
            "  [{ts}] ⚠ Failed to add reaction '{emoji}' to msg {message_id} in channel {channel_id}: {e}"
        );
    }
}

// ─── Event handler ───────────────────────────────────────────────────────────

/// Periodic GC: delete stale idle/disconnected thread sessions from DB via cleanup API.
async fn gc_stale_thread_sessions_via_api(api_port: u16) {
    let url = crate::config::local_api_url(api_port, "/api/dispatched-sessions/gc-threads");
    match reqwest::Client::new().delete(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                let gc = body.get("gc_threads").and_then(|v| v.as_u64()).unwrap_or(0);
                if gc > 0 {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] 🧹 GC: removed {gc} stale thread session(s) from DB");
                }
            }
        }
        Ok(resp) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] ⚠ Thread session GC failed: HTTP {}",
                resp.status()
            );
        }
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!("  [{ts}] ⚠ Thread session GC error: {e}");
        }
    }
}

/// Periodic GC: disconnect stale fixed-channel working sessions from the DB so
/// restart recovery cannot restore dead provider session IDs.
async fn gc_stale_fixed_working_sessions(shared: &Arc<SharedData>) {
    let Some(db) = &shared.db else {
        return;
    };

    let cleared = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!("  [{ts}] ⚠ Fixed-session GC lock error: {e}");
                return;
            }
        };
        crate::server::routes::dispatched_sessions::gc_stale_fixed_working_sessions_db(&conn)
    };

    if cleared > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] 🧹 GC: disconnected {cleared} stale fixed-channel working session(s)");
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

    let expired: Vec<(ChannelId, Option<String>)> = {
        let data = shared.core.lock().await;
        let now = tokio::time::Instant::now();
        data.sessions
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_active) > SESSION_MAX_IDLE)
            .map(|(ch, s)| (*ch, s.session_id.clone()))
            .collect()
    };
    if expired.is_empty() {
        return;
    }
    // Collect session_keys for audit before removing from memory
    let expired_keys: Vec<(ChannelId, String)> = {
        let hostname = crate::services::platform::hostname_short();
        let provider = shared.settings.read().await.provider.clone();
        let data = shared.core.lock().await;
        expired
            .iter()
            .filter_map(|(ch, _)| {
                data.sessions.get(ch).and_then(|s| {
                    s.channel_name.as_ref().map(|name| {
                        let tmux_name = provider.build_tmux_session_name(name);
                        (*ch, format!("{}:{}", hostname, tmux_name))
                    })
                })
            })
            .collect()
    };
    {
        let mut data = shared.core.lock().await;
        for (ch, _) in &expired {
            // Clean up worktree if session had one
            if let Some(session) = data.sessions.get(ch) {
                if let Some(ref wt) = session.worktree {
                    cleanup_git_worktree(wt);
                }
            }
            data.sessions.remove(ch);
            if data.cancel_tokens.remove(ch).is_some() {
                shared
                    .global_active
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            data.active_request_owner.remove(ch);
            data.intervention_queue.remove(ch);
        }
    }
    for (ch, _) in &expired {
        shared.api_timestamps.remove(ch);
        shared.tmux_watchers.remove(ch);
    }
    // Record termination audit for cleaned-up sessions
    for (_, session_key) in &expired_keys {
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
    println!("  [cleanup] Removed {} idle session(s)", expired.len());
}

// ─── Slash commands (extracted to commands/ module) ──────────────────────────

// Command functions removed — see commands/ submodule.
// Remaining in mod.rs: detect_worktree_conflict, create_git_worktree, cleanup_git_worktree,
// send_file_to_channel, send_message_to_channel, send_message_to_user, auto_restore_session,
// bootstrap_thread_session, resolve_channel_category, and other non-command functions.

// ─── Text message → Claude AI ───────────────────────────────────────────────

/// Handle regular text messages — send to the active provider.
/// Check if a path is a git repo and if another channel already uses it.
/// Returns the conflicting channel's name if found.
pub(super) fn detect_worktree_conflict(
    sessions: &HashMap<ChannelId, DiscordSession>,
    path: &str,
    my_channel: ChannelId,
) -> Option<String> {
    let norm = path.trim_end_matches('/');
    for (cid, session) in sessions {
        if *cid == my_channel {
            continue;
        }
        let other_path = if let Some(ref wt) = session.worktree {
            &wt.original_path
        } else {
            match &session.current_path {
                Some(p) => p.as_str(),
                None => continue,
            }
        };
        if other_path.trim_end_matches('/') == norm {
            return session
                .channel_name
                .clone()
                .or_else(|| Some(cid.get().to_string()));
        }
    }
    None
}

/// Create a git worktree for the given repo path.
/// Returns (worktree_path, branch_name) on success.
pub(super) fn create_git_worktree(
    repo_path: &str,
    channel_name: &str,
    provider: &str,
) -> Result<(String, String), String> {
    let git_check = std::process::Command::new("git")
        .args(["-C", repo_path, "rev-parse", "--is-inside-work-tree"])
        .output()
        .map_err(|e| format!("git check failed: {}", e))?;
    if !git_check.status.success() {
        return Err(format!("{} is not a git repository", repo_path));
    }

    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let safe_name = channel_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let branch = format!("wt/{}-{}-{}", provider, safe_name, ts);

    let wt_base = worktrees_root().ok_or("Cannot determine worktree root")?;
    std::fs::create_dir_all(&wt_base)
        .map_err(|e| format!("Failed to create worktree base dir: {}", e))?;
    let wt_dir = wt_base.join(format!("{}-{}-{}", provider, safe_name, ts));
    let wt_path = wt_dir.display().to_string();

    let output = std::process::Command::new("git")
        .args(["-C", repo_path, "worktree", "add", &wt_path, "-b", &branch])
        .output()
        .map_err(|e| format!("git worktree add failed: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree add failed: {}", stderr));
    }

    let ts_log = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts_log}] 🌿 Created worktree: {} (branch: {})",
        wt_path, branch
    );
    Ok((wt_path, branch))
}

/// Clean up a git worktree after session ends.
fn cleanup_git_worktree(wt_info: &WorktreeInfo) {
    let ts = chrono::Local::now().format("%H:%M:%S");

    let status = std::process::Command::new("git")
        .args(["-C", &wt_info.worktree_path, "status", "--porcelain"])
        .output();
    let has_changes = match &status {
        Ok(out) => !out.stdout.is_empty(),
        Err(_) => false,
    };

    // Check if branch has new commits
    let diff = std::process::Command::new("git")
        .args([
            "-C",
            &wt_info.original_path,
            "log",
            "--oneline",
            &format!("HEAD..{}", wt_info.branch_name),
        ])
        .output();
    let has_commits = match &diff {
        Ok(out) => !out.stdout.is_empty(),
        Err(_) => false,
    };

    if has_changes || has_commits {
        println!(
            "  [{ts}] 🌿 Worktree {} has changes/commits — keeping for manual merge",
            wt_info.worktree_path
        );
        println!(
            "  [{ts}] 🌿 Branch: {} | Original: {}",
            wt_info.branch_name, wt_info.original_path
        );
    } else {
        let _ = std::process::Command::new("git")
            .args([
                "-C",
                &wt_info.original_path,
                "worktree",
                "remove",
                &wt_info.worktree_path,
            ])
            .output();
        let _ = std::process::Command::new("git")
            .args([
                "-C",
                &wt_info.original_path,
                "branch",
                "-d",
                &wt_info.branch_name,
            ])
            .output();
        println!(
            "  [{ts}] 🌿 Cleaned up worktree: {} (no changes)",
            wt_info.worktree_path
        );
    }
}

// ─── File upload handling ────────────────────────────────────────────────────

// ─── Sendfile (CLI) ──────────────────────────────────────────────────────────

/// Send a file to a Discord channel (called from CLI --discord-sendfile)
pub async fn send_file_to_channel(
    token: &str,
    channel_id: u64,
    file_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new(file_path);
    if !path.exists() {
        return Err(format!("File not found: {}", file_path).into());
    }

    let http = serenity::Http::new(token);

    let channel = ChannelId::new(channel_id);
    let attachment = CreateAttachment::path(path).await?;

    channel
        .send_message(
            &http,
            CreateMessage::new()
                .content(format!(
                    "📎 {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ))
                .add_file(attachment),
        )
        .await?;

    Ok(())
}

/// Send a text message to a Discord channel (called from CLI --discord-sendmessage)
pub async fn send_message_to_channel(
    token: &str,
    channel_id: u64,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = serenity::Http::new(token);
    let channel = ChannelId::new(channel_id);

    channel
        .send_message(&http, CreateMessage::new().content(message))
        .await?;

    Ok(())
}

/// Send a text message to a Discord user DM (called from CLI --discord-senddm)
pub async fn send_message_to_user(
    token: &str,
    user_id: u64,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = serenity::Http::new(token);
    let dm_channel = UserId::new(user_id).create_dm_channel(&http).await?;

    dm_channel
        .id
        .send_message(&http, CreateMessage::new().content(message))
        .await?;

    Ok(())
}

// ─── Session persistence ─────────────────────────────────────────────────────

/// Auto-restore session from bot_settings.json if not in memory
pub(super) async fn auto_restore_session(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    serenity_ctx: &serenity::prelude::Context,
) {
    // Resolve channel/category before taking the lock for mutation
    let (live_ch_name, cat_name) = resolve_channel_category(serenity_ctx, channel_id).await;
    let existing_channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let restore_ch_name = choose_restore_channel_name(
        existing_channel_name.as_deref(),
        live_ch_name.as_deref(),
        resolve_thread_parent(&serenity_ctx.http, channel_id).await,
        channel_id,
    );

    // Read settings first to get last_sessions/last_remotes info
    // DB cwd takes priority over yaml last_sessions (preserves worktree paths)
    let (last_path, is_remote, saved_remote, provider) = {
        let settings = shared.settings.read().await;
        let channel_key = channel_id.get().to_string();
        let yaml_path = settings.last_sessions.get(&channel_key).cloned();
        let is_remote = settings.last_remotes.contains_key(&channel_key);
        let saved_remote = settings.last_remotes.get(&channel_key).cloned();
        let provider = settings.provider.clone();

        // Use the effective tmux channel name here so restart recovery keeps
        // looking up the same session key for thread sessions that intentionally
        // use a synthetic "{parent}-t{thread_id}" channel name.
        let db_cwd: Option<String> = restore_ch_name.as_ref().and_then(|ch| {
            let tmux_name = provider.build_tmux_session_name(ch);
            let hostname = crate::services::platform::hostname_short();
            let session_key = format!("{}:{}", hostname, tmux_name);
            shared.db.as_ref().and_then(|db| {
                db.lock().ok().and_then(|conn| {
                    conn.query_row(
                        "SELECT cwd FROM sessions WHERE session_key = ?1",
                        [&session_key],
                        |row| row.get::<_, String>(0),
                    )
                    .ok()
                    .filter(|p| !p.is_empty() && std::path::Path::new(p).is_dir())
                })
            })
        });
        let last_path = db_cwd.or(yaml_path);

        (last_path, is_remote, saved_remote, provider)
    };

    let mut data = shared.core.lock().await;
    if let Some(session) = data.sessions.get_mut(&channel_id) {
        session.channel_id = Some(channel_id.get());
        session.last_active = tokio::time::Instant::now();
        session.channel_name = restore_ch_name.clone();
        session.category_name = cat_name.clone();
        if session.remote_profile_name.is_none() {
            session.remote_profile_name = saved_remote.clone();
        }
        if session.current_path.is_some() || last_path.is_none() {
            return;
        }
    }

    if let Some(last_path) = last_path {
        if is_remote || Path::new(&last_path).is_dir() {
            // Session ID is restored from DB (sessions.claude_session_id column)
            // which is already loaded into DiscordSession.session_id at startup.
            let session = data
                .sessions
                .entry(channel_id)
                .or_insert_with(|| DiscordSession {
                    session_id: None,
                    current_path: None,
                    history: Vec::new(),
                    pending_uploads: Vec::new(),
                    cleared: false,
                    channel_id: Some(channel_id.get()),
                    channel_name: restore_ch_name.clone(),
                    category_name: cat_name.clone(),
                    remote_profile_name: saved_remote.clone(),

                    last_active: tokio::time::Instant::now(),
                    worktree: None,

                    born_generation: runtime_store::load_generation(),
                });
            session.channel_id = Some(channel_id.get());
            session.last_active = tokio::time::Instant::now();
            session.channel_name = restore_ch_name.clone();
            session.category_name = cat_name.clone();
            if session.remote_profile_name.is_none() {
                session.remote_profile_name = saved_remote.clone();
            }
            session.current_path = Some(last_path.clone());
            drop(data);

            // Rescan skills with project path
            let new_skills = scan_skills(&provider, Some(&last_path));
            *shared.skills_cache.write().await = new_skills;
            let ts = chrono::Local::now().format("%H:%M:%S");
            let remote_info = saved_remote
                .as_ref()
                .map(|n| format!(" (remote: {})", n))
                .unwrap_or_default();
            println!("  [{ts}] ↻ Auto-restored session: {last_path}{remote_info}");
        }
    }
}

/// Create a lightweight session for a thread, bootstrapped from the parent channel's path.
/// The session's `channel_name` uses `{parent_channel}-t{thread_id}` so the derived
/// tmux session name stays short and unique instead of using the full thread title.
async fn bootstrap_thread_session(
    shared: &Arc<SharedData>,
    thread_channel_id: ChannelId,
    parent_path: &str,
    serenity_ctx: &serenity::prelude::Context,
) {
    let (_thread_title, cat_name) = resolve_channel_category(serenity_ctx, thread_channel_id).await;
    // Build a short, stable channel_name: "{parent_channel}-t{thread_id}"
    let parent_info = resolve_thread_parent(&serenity_ctx.http, thread_channel_id).await;
    let ch_name = if let Some((_parent_id, parent_name)) = parent_info {
        let parent = parent_name.unwrap_or_else(|| format!("{}", _parent_id));
        Some(synthetic_thread_channel_name(&parent, thread_channel_id))
    } else {
        // Not a thread (shouldn't happen here) — fall back to resolved name
        _thread_title
    };
    let mut data = shared.core.lock().await;
    if data.sessions.contains_key(&thread_channel_id) {
        return;
    }

    // Session ID comes from DB (sessions.claude_session_id), not from file.
    let session = data
        .sessions
        .entry(thread_channel_id)
        .or_insert_with(|| DiscordSession {
            session_id: None,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            channel_id: Some(thread_channel_id.get()),
            channel_name: ch_name,
            category_name: cat_name,
            remote_profile_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: runtime_store::load_generation(),
        });
    session.current_path = Some(parent_path.to_string());
    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ↻ Bootstrapped thread session from parent path: {parent_path}");
}

/// Resolve the channel name and parent category name for a Discord channel.
pub(super) async fn resolve_channel_category(
    ctx: &serenity::prelude::Context,
    channel_id: serenity::model::id::ChannelId,
) -> (Option<String>, Option<String>) {
    let Ok(channel) = channel_id.to_channel(&ctx.http).await else {
        return (None, None);
    };
    let serenity::model::channel::Channel::Guild(gc) = channel else {
        return (None, None);
    };
    let ch_name = Some(gc.name.clone());
    let cat_name = if let Some(parent_id) = gc.parent_id {
        let cached_cat_name = ctx.cache.guild(gc.guild_id).and_then(|guild| {
            guild
                .channels
                .get(&parent_id)
                .map(|parent_ch| parent_ch.name.clone())
        });

        if let Some(cat_name) = cached_cat_name {
            Some(cat_name)
        } else if let Ok(parent_ch) = parent_id.to_channel(&ctx.http).await {
            match parent_ch {
                serenity::model::channel::Channel::Guild(cat) => Some(cat.name.clone()),
                _ => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ⚠ Category channel {parent_id} is not a Guild channel for #{}",
                        gc.name
                    );
                    None
                }
            }
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ Failed to resolve category {parent_id} for #{}",
                gc.name
            );
            None
        }
    } else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] ⚠ No parent_id for #{}", gc.name);
        None
    };
    (ch_name, cat_name)
}

pub(super) async fn provider_handles_channel(
    ctx: &serenity::prelude::Context,
    provider: &ProviderKind,
    settings: &DiscordBotSettings,
    channel_id: serenity::model::id::ChannelId,
) -> bool {
    let is_dm = matches!(
        channel_id.to_channel(&ctx.http).await,
        Ok(serenity::model::channel::Channel::Private(_))
    );
    let (channel_name, _) = resolve_channel_category(ctx, channel_id).await;
    let (effective_channel_id, effective_channel_name) = if let Some((parent_id, parent_name)) =
        resolve_thread_parent(&ctx.http, channel_id).await
    {
        (parent_id, parent_name.or(channel_name))
    } else {
        (channel_id, channel_name)
    };
    validate_bot_channel_routing(
        settings,
        provider,
        effective_channel_id,
        effective_channel_name.as_deref(),
        is_dm,
    )
    .is_ok()
}

/// If `channel_id` is a Discord thread, return the parent channel ID and name.
/// For non-thread channels, returns `None`.
pub(super) async fn resolve_thread_parent(
    http: &Arc<serenity::Http>,
    channel_id: serenity::model::id::ChannelId,
) -> Option<(serenity::model::id::ChannelId, Option<String>)> {
    let channel = channel_id.to_channel(http).await.ok()?;
    let serenity::model::channel::Channel::Guild(gc) = channel else {
        return None;
    };
    use serenity::model::channel::ChannelType;
    match gc.kind {
        ChannelType::PublicThread | ChannelType::PrivateThread => {
            let parent_id = gc.parent_id?;
            let parent_name = if let Ok(parent_ch) = parent_id.to_channel(http).await {
                match parent_ch {
                    serenity::model::channel::Channel::Guild(pg) => Some(pg.name.clone()),
                    _ => None,
                }
            } else {
                None
            };
            Some((parent_id, parent_name))
        }
        _ => None,
    }
}

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
    use super::ChannelId;
    use super::{
        DiscordBotSettings, choose_restore_channel_name, is_synthetic_thread_channel_name,
        synthetic_thread_channel_name, user_is_authorized,
    };

    #[test]
    fn synthetic_thread_channel_name_round_trips() {
        let channel_id = ChannelId::new(12345);
        let synthetic = synthetic_thread_channel_name("agentdesk-codex", channel_id);

        assert_eq!(synthetic, "agentdesk-codex-t12345");
        assert!(is_synthetic_thread_channel_name(&synthetic, channel_id));
        assert!(!is_synthetic_thread_channel_name(
            "agentdesk-codex",
            channel_id
        ));
    }

    #[test]
    fn choose_restore_channel_name_prefers_existing_synthetic_thread_name() {
        let channel_id = ChannelId::new(12345);
        let chosen = choose_restore_channel_name(
            Some("agentdesk-codex-t12345"),
            Some("새 스레드 제목"),
            Some((ChannelId::new(777), Some("agentdesk-codex".to_string()))),
            channel_id,
        );

        assert_eq!(chosen.as_deref(), Some("agentdesk-codex-t12345"));
    }

    #[test]
    fn choose_restore_channel_name_builds_synthetic_name_for_threads() {
        let channel_id = ChannelId::new(12345);
        let chosen = choose_restore_channel_name(
            None,
            Some("새 스레드 제목"),
            Some((ChannelId::new(777), Some("agentdesk-codex".to_string()))),
            channel_id,
        );

        assert_eq!(chosen.as_deref(), Some("agentdesk-codex-t12345"));
    }

    #[test]
    fn choose_restore_channel_name_keeps_existing_name_when_live_metadata_missing() {
        let channel_id = ChannelId::new(12345);
        let chosen = choose_restore_channel_name(Some("agentdesk-codex"), None, None, channel_id);

        assert_eq!(chosen.as_deref(), Some("agentdesk-codex"));
    }

    #[test]
    fn user_is_authorized_allows_owner_and_explicit_users() {
        let mut settings = DiscordBotSettings::default();
        settings.owner_user_id = Some(42);
        settings.allowed_user_ids = vec![7];

        assert!(user_is_authorized(&settings, 42));
        assert!(user_is_authorized(&settings, 7));
        assert!(!user_is_authorized(&settings, 99));
    }

    #[test]
    fn user_is_authorized_allows_everyone_when_flag_enabled() {
        let mut settings = DiscordBotSettings::default();
        settings.owner_user_id = Some(42);
        settings.allow_all_users = true;

        assert!(user_is_authorized(&settings, 42));
        assert!(user_is_authorized(&settings, 99));
    }
}
