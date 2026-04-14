use super::*;

/// Per-channel session state
#[derive(Clone)]
pub(super) struct DiscordSession {
    pub(super) session_id: Option<String>,
    pub(super) memento_context_loaded: bool,
    pub(super) memento_reflected: bool,
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
    #[allow(dead_code)]
    pub(super) born_generation: u64,
}

pub(super) fn allows_nonlocal_session_path(remote_profile_name: Option<&str>) -> bool {
    remote_profile_name.is_some_and(|name| !name.trim().is_empty())
}

pub(super) fn session_path_is_usable(
    current_path: &str,
    remote_profile_name: Option<&str>,
) -> bool {
    allows_nonlocal_session_path(remote_profile_name) || std::path::Path::new(current_path).is_dir()
}

impl DiscordSession {
    pub(super) fn clear_provider_session(&mut self) {
        self.session_id = None;
        self.memento_context_loaded = false;
        self.memento_reflected = false;
    }

    pub(super) fn restore_provider_session(&mut self, session_id: Option<String>) {
        self.session_id = session_id;
        self.memento_context_loaded = self.session_id.is_some();
        self.memento_reflected = false;
    }

    pub(super) fn note_memento_context_loaded(&mut self) {
        self.memento_context_loaded = true;
        self.memento_reflected = false;
    }

    /// Validate `current_path` and return it if it exists on disk.
    /// If the path is stale (deleted), clear `current_path` and `worktree`, log, and return `None`.
    pub(super) fn validated_path(&mut self, channel_id: impl std::fmt::Display) -> Option<String> {
        let current_path = self.current_path.as_ref()?;
        if session_path_is_usable(current_path, self.remote_profile_name.as_deref()) {
            return Some(current_path.clone());
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
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
pub(super) enum InterventionMode {
    Soft,
}

#[derive(Clone, Debug)]
pub(super) struct Intervention {
    pub(super) author_id: UserId,
    pub(super) message_id: MessageId,
    pub(super) text: String,
    pub(super) mode: InterventionMode,
    pub(super) created_at: Instant,
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

pub(super) fn is_synthetic_thread_channel_name(channel_name: &str, channel_id: ChannelId) -> bool {
    channel_name.ends_with(&format!("-t{}", channel_id.get()))
}

pub(super) fn choose_restore_channel_name(
    existing_channel_name: Option<&str>,
    live_channel_name: Option<&str>,
    thread_parent: Option<(ChannelId, Option<String>)>,
    channel_id: ChannelId,
) -> Option<String> {
    if let Some(existing_name) = existing_channel_name
        && is_synthetic_thread_channel_name(existing_name, channel_id)
    {
        return Some(existing_name.to_string());
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
    pub(super) active_meetings: HashMap<ChannelId, meeting::Meeting>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session() -> DiscordSession {
        DiscordSession {
            session_id: Some("session-1".to_string()),
            memento_context_loaded: false,
            memento_reflected: true,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(1),
            channel_name: Some("adk-cdx".to_string()),
            category_name: Some("AgentDesk".to_string()),
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 1,
            assistant_turns: 0,
        }
    }

    #[test]
    fn clear_provider_session_resets_all_provider_state() {
        let mut session = sample_session();
        session.memento_context_loaded = true;

        session.clear_provider_session();

        assert_eq!(session.session_id, None);
        assert!(!session.memento_context_loaded);
        assert!(!session.memento_reflected);
    }

    #[test]
    fn restore_provider_session_marks_memento_context_for_resumed_sessions() {
        let mut session = sample_session();

        session.restore_provider_session(Some("restored-1".to_string()));

        assert_eq!(session.session_id.as_deref(), Some("restored-1"));
        assert!(session.memento_context_loaded);
        assert!(!session.memento_reflected);
    }

    #[test]
    fn note_memento_context_loaded_preserves_session_id_and_clears_reflect_flag() {
        let mut session = sample_session();

        session.note_memento_context_loaded();

        assert_eq!(session.session_id.as_deref(), Some("session-1"));
        assert!(session.memento_context_loaded);
        assert!(!session.memento_reflected);
    }
}
