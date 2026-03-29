mod adk_session;
mod commands;
mod formatting;
mod handoff;
pub(crate) mod health;
mod inflight;
mod meeting;
mod metrics;
mod org_schema;
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
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateAttachment, CreateMessage, EditMessage, MessageId, UserId};

use crate::services::claude::{
    self, CancelToken, DEFAULT_ALLOWED_TOOLS, ReadOutputResult, StreamMessage,
};
use crate::services::codex;
use crate::services::gemini;
use crate::services::provider::ProviderKind;
use crate::ui::ai_screen::{self, HistoryItem, HistoryType, SessionData};

use adk_session::{
    build_adk_session_key, derive_adk_session_info, parse_dispatch_id, post_adk_session_status,
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
    RoleBinding, channel_supports_provider, channel_upload_dir, cleanup_old_uploads,
    load_bot_settings, resolve_role_binding, save_bot_settings,
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

/// Maximum watchdog extension: 3 hours from the original turn start.
const WATCHDOG_MAX_EXTENSION_MS: i64 = 3 * 3600 * 1000;

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
    pub(super) provider: ProviderKind,
    pub(super) allowed_tools: Vec<String>,
    /// channel_id (string) → last working directory path
    pub(super) last_sessions: std::collections::HashMap<String, String>,
    /// channel_id (string) → last remote profile name
    pub(super) last_remotes: std::collections::HashMap<String, String>,
    /// Discord user ID of the registered owner (imprinting auth)
    pub(super) owner_user_id: Option<u64>,
    /// Additional authorized user IDs (added by owner via /adduser)
    pub(super) allowed_user_ids: Vec<u64>,
    /// Bot IDs whose messages are NOT ignored (e.g. announce bot for CEO directives)
    pub(super) allowed_bot_ids: Vec<u64>,
}

impl Default for DiscordBotSettings {
    fn default() -> Self {
        Self {
            provider: ProviderKind::Claude,
            allowed_tools: DEFAULT_ALLOWED_TOOLS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            last_sessions: std::collections::HashMap::new(),
            last_remotes: std::collections::HashMap::new(),
            owner_user_id: None,
            allowed_user_ids: Vec::new(),
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
    /// Takes priority over role-map model. Cleared via `/model default`.
    pub(super) model_overrides: dashmap::DashMap<ChannelId, String>,
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

    for (dispatch_id, agent_id, card_id, title, dtype) in &orphans {
        // Remove the dispatch_notified guard so send_dispatch_to_discord can proceed.
        // This is safe because the Once guard ensures only one caller reaches here,
        // and the 5-condition query already validated this dispatch is truly orphan.
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

        crate::server::routes::dispatches::send_dispatch_to_discord(
            db,
            agent_id,
            title,
            card_id,
            dispatch_id,
        )
        .await;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] ✓ #164: Re-delivered {} orphan dispatch(es)",
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
        model_overrides: dashmap::DashMap::new(),
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
                commands::cmd_adduser(),
                commands::cmd_removeuser(),
                commands::cmd_help(),
                commands::cmd_meeting(),
            ],
            event_handler: |ctx, event, _framework, data| Box::pin(handle_event(ctx, event, data)),
            ..Default::default()
        })
        .setup(move |ctx, _ready, framework| {
            let ctx_clone = ctx.clone();
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

                // Background: resolve category names for all known channels
                let shared_for_tmux = shared_for_migrate.clone();
                tokio::spawn(async move {
                    migrate_session_categories(&ctx_clone, &shared_for_migrate).await;
                });

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

                    #[cfg(unix)]
                    {
                        restore_tmux_watchers(&http_for_tmux, &shared_for_tmux2).await;
                        cleanup_orphan_tmux_sessions(&shared_for_tmux2).await;
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
                    tokio::spawn(async move {
                        // Run every 10 minutes, initial delay 2 minutes
                        tokio::time::sleep(tokio::time::Duration::from_secs(120)).await;
                        loop {
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
                            let partial =
                                formatting::format_for_discord(state.full_response.trim());
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
        Some(owner_id) => {
            let uid = user_id.get();
            if uid == owner_id || settings.allowed_user_ids.contains(&uid) {
                true
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ✗ Rejected: {user_name} (id:{})", uid);
                false
            }
        }
    }
}

/// Check if a user is the owner (not just allowed)
pub(super) async fn check_owner(user_id: UserId, shared: &Arc<SharedData>) -> bool {
    let settings = shared.settings.read().await;
    settings.owner_user_id == Some(user_id.get())
}

fn family_profile_probe_script_path() -> Option<std::path::PathBuf> {
    // Try org.yaml skills_root first, fallback to $AGENTDESK_ROOT_DIR/skills/
    let skills_root = org_schema::load_skills_root()
        .map(std::path::PathBuf::from)
        .or_else(|| runtime_store::agentdesk_root().map(|r| r.join("skills")));
    skills_root.map(|root| {
        root.join("family-profile-probe")
            .join("scripts")
            .join("select_profile_probe.py")
    })
}

fn family_profile_probe_state_paths() -> Vec<std::path::PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    vec![
        home.join(".local")
            .join("state")
            .join("family-profile-probe")
            .join("profile_probe_state.json"),
        home.join(".openclaw")
            .join("workspace")
            .join("state")
            .join("profile_probe_state.json"),
    ]
}

fn profile_probe_target_user_id(target: &str) -> Option<u64> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return None;
    }

    for prefix in ["user:", "dm:"] {
        if let Some(raw) = trimmed.strip_prefix(prefix) {
            return raw.trim().parse::<u64>().ok();
        }
    }

    trimmed.parse::<u64>().ok()
}

fn profile_probe_target_key_for_user(user_id: u64) -> Option<&'static str> {
    match user_id {
        343742347365974026 => Some("obujang"),
        429955158974136340 => Some("yohoejang"),
        _ => None,
    }
}

fn profile_probe_target_for_key(target_key: &str) -> Option<String> {
    match target_key {
        "obujang" => Some("user:343742347365974026".to_string()),
        "yohoejang" => Some("user:429955158974136340".to_string()),
        _ => None,
    }
}

fn pending_family_profile_probe_from_state_value(
    json: &serde_json::Value,
    user_id: u64,
) -> Option<(String, String)> {
    if let Some(pending) = json.get("pending").and_then(|v| v.as_object()) {
        for (target, entry) in pending {
            if profile_probe_target_user_id(target) != Some(user_id) {
                continue;
            }
            let Some(topic_key) = entry.get("topicKey").and_then(|v| v.as_str()) else {
                continue;
            };
            return Some((topic_key.to_string(), target.to_string()));
        }
    }

    let target_key = profile_probe_target_key_for_user(user_id)?;
    let topic_key = json
        .get("perTarget")
        .and_then(|v| v.as_object())
        .and_then(|per| per.get(target_key))
        .and_then(|v| v.as_object())
        .and_then(|bucket| bucket.get("pendingProbe"))
        .and_then(|v| v.as_object())
        .and_then(|pending| pending.get("topicKey"))
        .and_then(|v| v.as_str())?;
    let target = profile_probe_target_for_key(target_key)?;
    Some((topic_key.to_string(), target))
}

fn pending_family_profile_probe_for_user(user_id: u64) -> Option<(String, String)> {
    for path in family_profile_probe_state_paths() {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        if let Some(pending) = pending_family_profile_probe_from_state_value(&json, user_id) {
            return Some(pending);
        }
    }

    None
}

fn clear_family_profile_probe_pending_from_state_value(
    json: &mut serde_json::Value,
    target: &str,
) -> bool {
    let Some(root) = json.as_object_mut() else {
        return false;
    };

    let mut changed = false;
    let target_user_id = profile_probe_target_user_id(target);

    if let Some(pending) = root.get_mut("pending").and_then(|v| v.as_object_mut()) {
        let remove_targets: Vec<String> = pending
            .keys()
            .filter(|entry_target| {
                if entry_target.as_str() == target {
                    return true;
                }
                target_user_id
                    .map(|uid| profile_probe_target_user_id(entry_target) == Some(uid))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        for entry_target in remove_targets {
            if pending.remove(&entry_target).is_some() {
                changed = true;
            }
        }
    }

    if let Some(target_key) = target_user_id.and_then(profile_probe_target_key_for_user) {
        if let Some(bucket) = root
            .get_mut("perTarget")
            .and_then(|v| v.as_object_mut())
            .and_then(|per| per.get_mut(target_key))
            .and_then(|v| v.as_object_mut())
        {
            if bucket.remove("pendingProbe").is_some() {
                changed = true;
            }
        }
    }

    changed
}

fn clear_family_profile_probe_pending(target: &str) -> Result<bool, String> {
    let mut changed_any = false;
    for path in family_profile_probe_state_paths() {
        if !path.exists() {
            continue;
        }

        let content = fs::read_to_string(&path).map_err(|err| err.to_string())?;
        let mut json = serde_json::from_str::<serde_json::Value>(&content)
            .map_err(|err| format!("clear_pending_parse_failed:{err}: {}", path.display()))?;

        if clear_family_profile_probe_pending_from_state_value(&mut json, target) {
            let pretty = serde_json::to_string_pretty(&json)
                .map_err(|err| format!("clear_pending_serialize_failed:{err}"))?;
            fs::write(&path, pretty + "\n").map_err(|err| err.to_string())?;
            changed_any = true;
        }
    }

    Ok(changed_any)
}

fn record_family_profile_probe_answer(
    topic_key: &str,
    target: &str,
    answer: &str,
) -> Result<bool, String> {
    let Some(script_path) = family_profile_probe_script_path() else {
        return Err("family_profile_probe_script_missing".to_string());
    };
    if !script_path.exists() {
        return Err(format!(
            "family_profile_probe_script_not_found:{}",
            script_path.display()
        ));
    }

    let output = Command::new("/usr/bin/python3")
        .arg(script_path)
        .arg("--record-answer")
        .arg("--topic-key")
        .arg(topic_key)
        .arg("--target")
        .arg(target)
        .arg("--answer")
        .arg(answer)
        .output()
        .map_err(|err| err.to_string())?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        return Err(if stderr.is_empty() { stdout } else { stderr });
    }

    let payload = serde_json::from_str::<serde_json::Value>(&stdout)
        .map_err(|err| format!("record_answer_parse_failed:{err}: {stdout}"))?;
    Ok(payload.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
}

fn record_and_clear_family_profile_probe_answer(
    topic_key: &str,
    target: &str,
    answer: &str,
) -> Result<bool, String> {
    let recorded = record_family_profile_probe_answer(topic_key, target, answer)?;
    if recorded {
        if let Err(err) = clear_family_profile_probe_pending(target) {
            eprintln!(
                "  [profile-probe] recorded answer but failed to clear pending target={} topic={} error={}",
                target, topic_key, err
            );
        }
    }
    Ok(recorded)
}

async fn try_handle_family_profile_probe_reply(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> Result<bool, Error> {
    if *provider != ProviderKind::Claude || msg.author.bot || msg.guild_id.is_some() {
        return Ok(false);
    }

    let answer = msg.content.trim();
    if answer.is_empty() {
        return Ok(false);
    }

    let Some((topic_key, target)) = pending_family_profile_probe_for_user(msg.author.id.get())
    else {
        return Ok(false);
    };

    let topic_key_owned = topic_key.clone();
    let target_owned = target.clone();
    let answer_owned = answer.to_string();
    let recorded = tokio::task::spawn_blocking(move || {
        record_and_clear_family_profile_probe_answer(&topic_key_owned, &target_owned, &answer_owned)
    })
    .await
    .map_err(|err| format!("profile_probe_join_failed:{err}"))?;

    let response = match recorded {
        Ok(true) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ✓ Recorded family profile probe answer: user={} topic={}",
                msg.author.id.get(),
                topic_key
            );
            "답변 고마워요. 프로필에 반영해둘게요."
        }
        Ok(false) => {
            eprintln!(
                "  [profile-probe] record_answer returned false for user={} topic={}",
                msg.author.id.get(),
                topic_key
            );
            "답변은 받았는데 저장 대상에 바로 반영하지 못했어요. 제가 다시 확인할게요."
        }
        Err(err) => {
            eprintln!(
                "  [profile-probe] failed to record answer for user={} topic={} error={}",
                msg.author.id.get(),
                topic_key,
                err
            );
            "답변은 받았는데 저장 중 오류가 있었어요. 다시 확인해서 반영할게요."
        }
    };

    rate_limit_wait(shared, msg.channel_id).await;
    let _ = msg.channel_id.say(&ctx.http, response).await;
    Ok(true)
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

    let expired: Vec<ChannelId> = {
        let data = shared.core.lock().await;
        let now = tokio::time::Instant::now();
        data.sessions
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_active) > SESSION_MAX_IDLE)
            .map(|(ch, _)| *ch)
            .collect()
    };
    if expired.is_empty() {
        return;
    }
    {
        let mut data = shared.core.lock().await;
        for ch in &expired {
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
    for ch in &expired {
        shared.api_timestamps.remove(ch);
        shared.tmux_watchers.remove(ch);
    }
    println!("  [cleanup] Removed {} idle session(s)", expired.len());
}

// ─── Slash commands (extracted to commands/ module) ──────────────────────────

// Command functions removed — see commands/ submodule.
// Remaining in mod.rs: detect_worktree_conflict, create_git_worktree, cleanup_git_worktree,
// send_file_to_channel, send_message_to_channel, send_message_to_user, auto_restore_session,
// bootstrap_thread_session, load_existing_session, cleanup_session_files, resolve_channel_category,
// and other non-command functions.

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
    {
        let data = shared.core.lock().await;
        if data.sessions.contains_key(&channel_id) {
            return;
        }
    }

    // Resolve channel/category before taking the lock for mutation
    let (ch_name, cat_name) = resolve_channel_category(serenity_ctx, channel_id).await;

    // Read settings first to get last_sessions/last_remotes info
    // DB cwd takes priority over yaml last_sessions (preserves worktree paths)
    let (last_path, is_remote, saved_remote, provider) = {
        let settings = shared.settings.read().await;
        let channel_key = channel_id.get().to_string();
        let yaml_path = settings.last_sessions.get(&channel_key).cloned();
        let is_remote = settings.last_remotes.contains_key(&channel_key);
        let saved_remote = settings.last_remotes.get(&channel_key).cloned();
        let provider = settings.provider.clone();

        // Try DB cwd first — preserves worktree paths from previous session
        let ch_name = {
            let data = shared.core.lock().await;
            data.sessions
                .get(&channel_id)
                .and_then(|s| s.channel_name.clone())
        };
        let db_cwd: Option<String> = ch_name.as_ref().and_then(|ch| {
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
    if data.sessions.contains_key(&channel_id) {
        return; // Double-check after re-acquiring lock
    }

    if let Some(last_path) = last_path {
        if is_remote || Path::new(&last_path).is_dir() {
            let existing = load_existing_session(&last_path, Some(channel_id.get()));
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
                    channel_name: ch_name,
                    category_name: cat_name,
                    remote_profile_name: saved_remote.clone(),

                    last_active: tokio::time::Instant::now(),
                    worktree: None,

                    born_generation: runtime_store::load_generation(),
                });
            session.channel_id = Some(channel_id.get());
            session.last_active = tokio::time::Instant::now();
            session.current_path = Some(last_path.clone());
            let current_gen = runtime_store::load_generation();
            if let Some((session_data, _)) = existing {
                // Restore session_id and history regardless of generation.
                // If the session_id is stale (Claude CLI died), auto-retry
                // will detect "No conversation found" and create a fresh session
                // with Discord history context.
                session.session_id = Some(session_data.session_id.clone());
                session.history = session_data.history.clone();
                if session_data.born_generation < current_gen && current_gen > 0 {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ↻ Adopting old-gen session for {last_path} (gen {} → {current_gen})",
                        session_data.born_generation
                    );
                }
            }
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
    let parent_info = resolve_thread_parent(serenity_ctx, thread_channel_id).await;
    let ch_name = if let Some((_parent_id, parent_name)) = parent_info {
        let parent = parent_name.unwrap_or_else(|| format!("{}", _parent_id));
        Some(format!("{}-t{}", parent, thread_channel_id.get()))
    } else {
        // Not a thread (shouldn't happen here) — fall back to resolved name
        _thread_title
    };
    let existing = load_existing_session(parent_path, Some(thread_channel_id.get()));

    let mut data = shared.core.lock().await;
    if data.sessions.contains_key(&thread_channel_id) {
        return;
    }

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
    if let Some((session_data, _)) = existing {
        session.session_id = Some(session_data.session_id.clone());
        session.history = session_data.history.clone();
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ↻ Bootstrapped thread session from parent path: {parent_path}");
}

/// Load existing session from ai_sessions directory.
/// Prefers sessions with a non-empty session_id. Among those, picks the most recently modified.
pub(super) fn load_existing_session(
    current_path: &str,
    channel_id: Option<u64>,
) -> Option<(SessionData, std::time::SystemTime)> {
    let sessions_dir = ai_screen::ai_sessions_dir()?;

    if !sessions_dir.exists() {
        return None;
    }

    let mut best_with_id: Option<(SessionData, std::time::SystemTime)> = None;
    let mut best_without_id: Option<(SessionData, std::time::SystemTime)> = None;

    if let Ok(entries) = fs::read_dir(&sessions_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(session_data) = serde_json::from_str::<SessionData>(&content) {
                        if session_data.current_path == current_path {
                            // Strict channel-aware restore when channel_id is provided.
                            if let Some(cid) = channel_id {
                                if session_data.discord_channel_id != Some(cid) {
                                    continue;
                                }
                            }

                            if let Ok(metadata) = path.metadata() {
                                if let Ok(modified) = metadata.modified() {
                                    let has_id = !session_data.session_id.is_empty();
                                    let target = if has_id {
                                        &mut best_with_id
                                    } else {
                                        &mut best_without_id
                                    };
                                    match target {
                                        None => *target = Some((session_data, modified)),
                                        Some((_, latest_time)) if modified > *latest_time => {
                                            *target = Some((session_data, modified));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Prefer sessions with a valid session_id
    best_with_id.or(best_without_id)
}

/// Clean up stale session files for a given path, keeping only the one matching current_session_id.
pub(super) fn cleanup_session_files(current_path: &str, current_session_id: Option<&str>) {
    let Some(sessions_dir) = ai_screen::ai_sessions_dir() else {
        return;
    };
    if !sessions_dir.exists() {
        return;
    }

    let Ok(entries) = fs::read_dir(&sessions_dir) else {
        return;
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.extension().map(|e| e == "json").unwrap_or(false) {
            continue;
        }
        // Don't delete the current session file
        if let Some(sid) = current_session_id {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if stem == sid {
                    continue;
                }
            }
        }
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(old) = serde_json::from_str::<SessionData>(&content) {
                if old.current_path == current_path {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
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

/// If `channel_id` is a Discord thread, return the parent channel ID and name.
/// For non-thread channels, returns `None`.
async fn resolve_thread_parent(
    ctx: &serenity::prelude::Context,
    channel_id: serenity::model::id::ChannelId,
) -> Option<(serenity::model::id::ChannelId, Option<String>)> {
    let channel = channel_id.to_channel(&ctx.http).await.ok()?;
    let serenity::model::channel::Channel::Guild(gc) = channel else {
        return None;
    };
    use serenity::model::channel::ChannelType;
    match gc.kind {
        ChannelType::PublicThread | ChannelType::PrivateThread => {
            let parent_id = gc.parent_id?;
            let parent_name = if let Ok(parent_ch) = parent_id.to_channel(&ctx.http).await {
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

/// On startup, resolve category names for all known channels and update session files.
async fn migrate_session_categories(ctx: &serenity::prelude::Context, shared: &Arc<SharedData>) {
    let sessions_dir = match ai_screen::ai_sessions_dir() {
        Some(d) if d.exists() => d,
        _ => return,
    };

    // Collect channel IDs from bot_settings.last_sessions
    let channel_keys: Vec<(String, String)> = {
        let settings = shared.settings.read().await;
        settings
            .last_sessions
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };

    let mut updated = 0usize;
    for (channel_key, session_path) in &channel_keys {
        let Ok(cid) = channel_key.parse::<u64>() else {
            continue;
        };
        let channel_id = serenity::model::id::ChannelId::new(cid);
        let (ch_name, cat_name) = resolve_channel_category(ctx, channel_id).await;
        if ch_name.is_none() && cat_name.is_none() {
            continue;
        }

        // Find the session file for this channel's path
        let existing = load_existing_session(session_path, Some(cid));
        if let Some((session_data, _)) = existing {
            let file_path = sessions_dir.join(format!("{}.json", session_data.session_id));
            if file_path.exists() {
                // Read, update category fields, write back
                if let Ok(content) = fs::read_to_string(&file_path) {
                    if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(obj) = val.as_object_mut() {
                            obj.insert(
                                "discord_channel_id".to_string(),
                                serde_json::Value::Number(serde_json::Number::from(cid)),
                            );
                            if let Some(ref name) = ch_name {
                                obj.insert(
                                    "discord_channel_name".to_string(),
                                    serde_json::Value::String(name.clone()),
                                );
                            }
                            if let Some(ref cat) = cat_name {
                                obj.insert(
                                    "discord_category_name".to_string(),
                                    serde_json::Value::String(cat.clone()),
                                );
                            }
                            if let Ok(json) = serde_json::to_string_pretty(&val) {
                                let _ = fs::write(&file_path, json);
                                updated += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    if updated > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] ✓ Updated {updated} session(s) with channel/category info");
    }
}

/// Save session to file in the ai_sessions directory
fn save_session_to_file(session: &DiscordSession, current_path: &str) {
    let Some(ref session_id) = session.session_id else {
        return;
    };

    if session.history.is_empty() {
        return;
    }

    let Some(sessions_dir) = ai_screen::ai_sessions_dir() else {
        return;
    };

    if fs::create_dir_all(&sessions_dir).is_err() {
        return;
    }

    let saveable_history: Vec<HistoryItem> = session
        .history
        .iter()
        .filter(|item| !matches!(item.item_type, HistoryType::System))
        .cloned()
        .collect();

    if saveable_history.is_empty() {
        return;
    }

    let file_path = sessions_dir.join(format!("{}.json", session_id));

    if let Some(parent) = file_path.parent() {
        if parent != sessions_dir {
            return;
        }
    }

    // Preserve existing category/channel names from the file when in-memory values are None
    let (effective_channel_name, effective_category_name) =
        if session.channel_name.is_none() || session.category_name.is_none() {
            if let Ok(content) = fs::read_to_string(&file_path) {
                if let Ok(existing) = serde_json::from_str::<SessionData>(&content) {
                    (
                        session
                            .channel_name
                            .clone()
                            .or(existing.discord_channel_name),
                        session
                            .category_name
                            .clone()
                            .or(existing.discord_category_name),
                    )
                } else {
                    (session.channel_name.clone(), session.category_name.clone())
                }
            } else {
                (session.channel_name.clone(), session.category_name.clone())
            }
        } else {
            (session.channel_name.clone(), session.category_name.clone())
        };

    // Clean up old session files for the same Discord channel (different session_id)
    if let Ok(entries) = fs::read_dir(&sessions_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                let fname = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if fname == session_id {
                    continue;
                } // keep current
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(old) = serde_json::from_str::<SessionData>(&content) {
                        let same_channel = match (session.channel_id, old.discord_channel_id) {
                            (Some(cid), Some(old_cid)) => cid == old_cid,
                            _ => old.discord_channel_name == effective_channel_name,
                        };
                        if same_channel {
                            let _ = fs::remove_file(&path);
                        }
                    }
                }
            }
        }
    }

    let session_data = SessionData {
        session_id: session_id.clone(),
        history: saveable_history,
        current_path: current_path.to_string(),
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        discord_channel_id: session.channel_id,
        discord_channel_name: effective_channel_name,
        discord_category_name: effective_category_name,
        remote_profile_name: session.remote_profile_name.clone(),
        born_generation: session.born_generation,
    };

    if let Ok(json) = serde_json::to_string_pretty(&session_data) {
        let _ = fs::write(file_path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clear_family_profile_probe_pending_from_state_value,
        pending_family_profile_probe_from_state_value,
    };

    #[test]
    fn legacy_pending_probe_is_detected() {
        let json = serde_json::json!({
            "pending": {
                "user:343742347365974026": {
                    "topicKey": "nar.vaccination_history"
                }
            }
        });

        let pending = pending_family_profile_probe_from_state_value(&json, 343742347365974026);
        assert_eq!(
            pending,
            Some((
                "nar.vaccination_history".to_string(),
                "user:343742347365974026".to_string()
            ))
        );
    }

    #[test]
    fn per_target_pending_probe_is_detected() {
        let json = serde_json::json!({
            "perTarget": {
                "yohoejang": {
                    "pendingProbe": {
                        "topicKey": "yunho.newborn_screening",
                        "question": "질문",
                        "sentAt": "2026-03-26T18:45:42.764539+09:00"
                    }
                }
            }
        });

        let pending = pending_family_profile_probe_from_state_value(&json, 429955158974136340);
        assert_eq!(
            pending,
            Some((
                "yunho.newborn_screening".to_string(),
                "user:429955158974136340".to_string()
            ))
        );
    }

    #[test]
    fn clear_pending_probe_removes_legacy_and_per_target_entries() {
        let mut json = serde_json::json!({
            "pending": {
                "user:343742347365974026": {
                    "topicKey": "nar.vaccination_history"
                }
            },
            "perTarget": {
                "obujang": {
                    "pendingProbe": {
                        "topicKey": "nar.vaccination_history",
                        "question": "질문",
                        "sentAt": "2026-03-26T19:00:41.734890+09:00"
                    }
                }
            }
        });

        let changed = clear_family_profile_probe_pending_from_state_value(
            &mut json,
            "user:343742347365974026",
        );

        assert!(changed);
        assert!(
            json.get("pending")
                .and_then(|v| v.as_object())
                .map(|pending| pending.is_empty())
                .unwrap_or(true)
        );
        assert!(
            json.get("perTarget")
                .and_then(|v| v.get("obujang"))
                .and_then(|v| v.as_object())
                .and_then(|bucket| bucket.get("pendingProbe"))
                .is_none()
        );
    }
}
