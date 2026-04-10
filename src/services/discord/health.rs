use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serde::Serialize;
use serenity::ChannelId;

use super::{SharedData, mailbox_clear_channel};
use crate::db::Db;
use crate::services::provider::ProviderKind;

/// Per-provider snapshot for the health response.
struct ProviderEntry {
    name: String,
    shared: Arc<SharedData>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

impl HealthStatus {
    fn rank(self) -> u8 {
        match self {
            Self::Healthy => 0,
            Self::Degraded => 1,
            Self::Unhealthy => 2,
        }
    }

    pub fn worsen(self, other: Self) -> Self {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }

    pub fn is_http_ready(self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded)
    }
}

#[derive(Debug, Serialize)]
struct ProviderHealthSnapshot {
    name: String,
    connected: bool,
    active_turns: usize,
    queue_depth: usize,
    sessions: usize,
    restart_pending: bool,
    last_turn_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DiscordHealthSnapshot {
    status: HealthStatus,
    version: &'static str,
    uptime_secs: u64,
    global_active: usize,
    global_finalizing: usize,
    deferred_hooks: usize,
    queue_depth: usize,
    watcher_count: usize,
    recovery_duration: f64,
    degraded_reasons: Vec<String>,
    providers: Vec<ProviderHealthSnapshot>,
}

impl DiscordHealthSnapshot {
    pub fn status(&self) -> HealthStatus {
        self.status
    }
}

/// Registry that providers register with so the unified axum API can query all of them.
/// Also holds Discord HTTP clients for agent-to-agent message routing.
pub struct HealthRegistry {
    providers: tokio::sync::Mutex<Vec<ProviderEntry>>,
    started_at: Instant,
    /// Discord HTTP clients keyed by provider name (for sending messages via correct bot)
    discord_http: tokio::sync::Mutex<Vec<(String, Arc<serenity::Http>)>>,
    /// Dedicated HTTP client for the announce bot (agent-to-agent routing).
    /// This bot's messages are accepted by all agents' allowed_bot_ids.
    announce_http: tokio::sync::Mutex<Option<Arc<serenity::Http>>>,
    /// Dedicated HTTP client for the notify bot (info-only notifications).
    /// Agents do NOT process notify bot messages — use for non-actionable alerts.
    notify_http: tokio::sync::Mutex<Option<Arc<serenity::Http>>>,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self {
            providers: tokio::sync::Mutex::new(Vec::new()),
            started_at: Instant::now(),
            discord_http: tokio::sync::Mutex::new(Vec::new()),
            announce_http: tokio::sync::Mutex::new(None),
            notify_http: tokio::sync::Mutex::new(None),
        }
    }

    pub(super) async fn register(&self, name: String, shared: Arc<SharedData>) {
        self.providers
            .lock()
            .await
            .push(ProviderEntry { name, shared });
    }

    pub(super) async fn register_http(&self, provider: String, http: Arc<serenity::Http>) {
        self.discord_http.lock().await.push((provider, http));
    }

    /// Load announce + notify bot tokens from credential/ files.
    /// Call once at startup before the axum server begins accepting requests.
    pub async fn init_bot_tokens(&self) {
        if let Some(root) = super::runtime_store::agentdesk_root() {
            for (bot_name, field) in [
                ("announce", &self.announce_http),
                ("notify", &self.notify_http),
            ] {
                let path = root
                    .join("credential")
                    .join(format!("{bot_name}_bot_token"));
                if let Ok(token) = std::fs::read_to_string(&path) {
                    let token = token.trim().to_string();
                    if !token.is_empty() {
                        let http = Arc::new(serenity::Http::new(&format!("Bot {token}")));
                        *field.lock().await = Some(http);
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        let emoji = if bot_name == "announce" {
                            "📢"
                        } else {
                            "🔔"
                        };
                        println!("  [{ts}] {emoji} {bot_name} bot loaded for /api/send routing");
                    }
                }
            }
        }
    }

    /// Start a meeting directly from the HTTP layer (dashboard direct-start).
    /// Finds the provider's registered Http client and SharedData, then calls meeting::start_meeting().
    /// Returns Ok(meeting_id) or an error string.
    pub async fn start_meeting_for_channel(
        &self,
        channel_id: ChannelId,
        agenda: &str,
        primary_provider: ProviderKind,
        reviewer_provider: ProviderKind,
    ) -> Result<Option<String>, String> {
        let (http, shared) = {
            let providers = self.providers.lock().await;
            let discord_http = self.discord_http.lock().await;

            let entry = providers
                .iter()
                .find(|e| e.name.eq_ignore_ascii_case(primary_provider.as_str()));
            let Some(entry) = entry else {
                return Err(format!(
                    "provider '{}' is not registered",
                    primary_provider.as_str()
                ));
            };
            let shared = entry.shared.clone();

            let http = discord_http
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(primary_provider.as_str()))
                .map(|(_, h)| h.clone());
            let Some(http) = http else {
                return Err(format!(
                    "no Discord HTTP client registered for provider '{}'",
                    primary_provider.as_str()
                ));
            };
            (http, shared)
        };

        super::meeting::start_meeting(
            &*http,
            channel_id,
            agenda,
            primary_provider,
            reviewer_provider,
            &shared,
        )
        .await
        .map_err(|e| e.to_string())
    }
}

/// Best-effort runtime-side equivalent of `/clear` for an existing Discord channel session.
/// Used by auto-queue slot recycling so pooled unified-thread slots start the next group fresh
/// without killing the shared thread itself.
pub async fn clear_provider_channel_runtime(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    session_key: Option<&str>,
) -> bool {
    let Some(provider) = ProviderKind::from_str(provider_name) else {
        return false;
    };

    let shared = {
        let providers = registry.providers.lock().await;
        providers
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(provider.as_str()))
            .map(|entry| entry.shared.clone())
    };
    let Some(shared) = shared else {
        return false;
    };

    let tmux_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.as_ref())
            .map(|channel_name| provider.build_tmux_session_name(channel_name))
            .or_else(|| {
                session_key
                    .and_then(|key| key.split_once(':'))
                    .map(|(_, tmux_name)| tmux_name.to_string())
            })
    };

    let cleared = mailbox_clear_channel(&shared, &provider, channel_id).await;
    if let Some(token) = cleared.removed_token {
        super::turn_bridge::cancel_active_token(&token, true, "auto-queue slot clear");
        shared.global_active.fetch_sub(1, Ordering::Relaxed);
    }

    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            super::settings::cleanup_channel_uploads(channel_id);
            session.clear_provider_session();
            session.history.clear();
            session.pending_uploads.clear();
            session.cleared = true;
        }
    }

    #[cfg(unix)]
    if provider == ProviderKind::Claude {
        if let Some(name) = tmux_name {
            let _ = tokio::task::spawn_blocking(move || {
                crate::services::platform::tmux::send_keys(&name, &["/clear", "Enter"])
            })
            .await;
        }
    }

    true
}

/// Build the health check snapshot for the API response.
pub async fn build_health_snapshot(registry: &HealthRegistry) -> DiscordHealthSnapshot {
    let uptime_secs = registry.started_at.elapsed().as_secs();
    let version = env!("CARGO_PKG_VERSION");

    let providers = registry.providers.lock().await;
    let mut provider_entries = Vec::new();
    let mut degraded_reasons = Vec::new();
    let mut status = HealthStatus::Healthy;
    let mut deferred_hooks = 0usize;
    let mut queue_depth = 0usize;
    let mut watcher_count = 0usize;
    let mut recovery_duration = 0.0f64;

    if providers.is_empty() {
        degraded_reasons.push("no_providers_registered".to_string());
        status = HealthStatus::Unhealthy;
    }

    for entry in providers.iter() {
        let session_count = entry
            .shared
            .core
            .try_lock()
            .map(|data| data.sessions.len())
            .unwrap_or(0);
        let mailbox_snapshots = entry.shared.mailboxes.snapshot_all().await;
        let active_turns = mailbox_snapshots
            .values()
            .filter(|snapshot| snapshot.cancel_token.is_some())
            .count();
        let provider_queue_depth: usize = mailbox_snapshots
            .values()
            .map(|snapshot| snapshot.intervention_queue.len())
            .sum();

        let restart_pending = entry
            .shared
            .restart_pending
            .load(std::sync::atomic::Ordering::Relaxed);
        let connected = entry
            .shared
            .bot_connected
            .load(std::sync::atomic::Ordering::Relaxed);
        let reconcile_done = entry
            .shared
            .reconcile_done
            .load(std::sync::atomic::Ordering::Relaxed);
        let provider_deferred_hooks = entry
            .shared
            .deferred_hook_backlog
            .load(std::sync::atomic::Ordering::Relaxed);
        let provider_watchers = entry.shared.tmux_watchers.len();
        let recovering_channels = mailbox_snapshots
            .values()
            .filter(|snapshot| snapshot.recovery_started_at.is_some())
            .count();
        let provider_recovery_duration = recovery_duration_secs(&entry.shared);
        let last_turn_at = entry
            .shared
            .last_turn_at
            .lock()
            .ok()
            .and_then(|g| g.clone());

        deferred_hooks += provider_deferred_hooks;
        queue_depth += provider_queue_depth;
        watcher_count += provider_watchers;
        recovery_duration = recovery_duration.max(provider_recovery_duration);

        if !connected {
            status = status.worsen(HealthStatus::Unhealthy);
            degraded_reasons.push(format!("provider:{}:disconnected", entry.name));
        }
        if restart_pending {
            status = status.worsen(HealthStatus::Unhealthy);
            degraded_reasons.push(format!("provider:{}:restart_pending", entry.name));
        }
        if !reconcile_done {
            status = status.worsen(HealthStatus::Degraded);
            degraded_reasons.push(format!("provider:{}:reconcile_in_progress", entry.name));
        }
        if provider_deferred_hooks > 0 {
            status = status.worsen(HealthStatus::Degraded);
            degraded_reasons.push(format!(
                "provider:{}:deferred_hooks_backlog:{}",
                entry.name, provider_deferred_hooks
            ));
        }
        if provider_queue_depth > 0 {
            status = status.worsen(HealthStatus::Degraded);
            degraded_reasons.push(format!(
                "provider:{}:pending_queue_depth:{}",
                entry.name, provider_queue_depth
            ));
        }
        if recovering_channels > 0 {
            status = status.worsen(HealthStatus::Degraded);
            degraded_reasons.push(format!(
                "provider:{}:recovering_channels:{}",
                entry.name, recovering_channels
            ));
        }

        provider_entries.push(ProviderHealthSnapshot {
            name: entry.name.clone(),
            connected,
            active_turns,
            queue_depth: provider_queue_depth,
            sessions: session_count,
            restart_pending,
            last_turn_at,
        });
    }

    let global_active = if let Some(p) = providers.first() {
        p.shared
            .global_active
            .load(std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };
    let global_finalizing = if let Some(p) = providers.first() {
        p.shared
            .global_finalizing
            .load(std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };

    DiscordHealthSnapshot {
        status,
        version,
        uptime_secs,
        global_active: global_active as usize,
        global_finalizing: global_finalizing as usize,
        deferred_hooks,
        queue_depth,
        watcher_count,
        recovery_duration,
        degraded_reasons,
        providers: provider_entries,
    }
}

fn recovery_duration_secs(shared: &SharedData) -> f64 {
    let recorded_ms = shared
        .recovery_duration_ms
        .load(std::sync::atomic::Ordering::Relaxed);
    let duration_ms = if recorded_ms > 0 {
        recorded_ms
    } else {
        let elapsed_ms = shared.recovery_started_at.elapsed().as_millis();
        elapsed_ms.min(u64::MAX as u128) as u64
    };
    duration_ms as f64 / 1000.0
}

#[cfg(test)]
pub(crate) struct TestHealthHarness {
    registry: Arc<HealthRegistry>,
    shared: Arc<SharedData>,
}

#[cfg(test)]
impl TestHealthHarness {
    pub(crate) async fn new() -> Self {
        let registry = Arc::new(HealthRegistry::new());
        let global_active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let global_finalizing = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let shutdown_remaining = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let shared = Arc::new(SharedData {
            core: tokio::sync::Mutex::new(super::CoreState {
                sessions: std::collections::HashMap::new(),
                active_meetings: std::collections::HashMap::new(),
            }),
            mailboxes: super::ChannelMailboxRegistry::default(),
            settings: tokio::sync::RwLock::new(super::DiscordBotSettings::default()),
            api_timestamps: dashmap::DashMap::new(),
            skills_cache: tokio::sync::RwLock::new(Vec::new()),
            tmux_watchers: dashmap::DashMap::new(),
            recovering_channels: dashmap::DashMap::new(),
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            finalizing_turns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            current_generation: 0,
            restart_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reconcile_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            deferred_hook_backlog: std::sync::atomic::AtomicUsize::new(0),
            recovery_started_at: Instant::now(),
            recovery_duration_ms: std::sync::atomic::AtomicU64::new(0),
            global_active,
            global_finalizing,
            shutdown_remaining,
            shutdown_counted: std::sync::atomic::AtomicBool::new(false),
            intake_dedup: dashmap::DashMap::new(),
            dispatch_thread_parents: dashmap::DashMap::new(),
            bot_connected: std::sync::atomic::AtomicBool::new(true),
            last_turn_at: std::sync::Mutex::new(None),
            model_overrides: dashmap::DashMap::new(),
            model_session_reset_pending: dashmap::DashSet::new(),
            model_picker_pending: dashmap::DashMap::new(),
            dispatch_role_overrides: dashmap::DashMap::new(),
            last_message_ids: dashmap::DashMap::new(),
            turn_start_times: dashmap::DashMap::new(),
            cached_serenity_ctx: tokio::sync::OnceCell::new(),
            cached_bot_token: tokio::sync::OnceCell::new(),
            token_hash: super::settings::discord_token_hash("test-token"),
            api_port: 8791,
            db: None,
            engine: None,
            known_slash_commands: tokio::sync::OnceCell::new(),
        });
        super::mark_reconcile_complete(&shared);
        registry
            .register("claude".to_string(), shared.clone())
            .await;
        Self { registry, shared }
    }

    pub(crate) fn registry(&self) -> Arc<HealthRegistry> {
        self.registry.clone()
    }

    pub(crate) fn set_deferred_hooks(&self, count: usize) {
        self.shared
            .deferred_hook_backlog
            .store(count, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn set_recovery_duration_ms(&self, duration_ms: u64) {
        self.shared
            .recovery_duration_ms
            .store(duration_ms, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) async fn set_queue_depth(&self, depth: usize) {
        super::mailbox_replace_queue(
            &self.shared,
            &ProviderKind::Claude,
            ChannelId::new(1),
            Vec::new(),
        )
        .await;
        if depth == 0 {
            return;
        }
        let queue = (0..depth)
            .map(|idx| super::Intervention {
                author_id: serenity::UserId::new(idx as u64 + 1),
                message_id: serenity::MessageId::new(idx as u64 + 1),
                text: format!("queued-{idx}"),
                mode: super::InterventionMode::Soft,
                created_at: Instant::now(),
            })
            .collect::<Vec<_>>();
        super::mailbox_replace_queue(
            &self.shared,
            &ProviderKind::Claude,
            ChannelId::new(1),
            queue,
        )
        .await;
    }
}

/// Resolve the bot HTTP client by name.
/// Supported: "announce", "notify", or a provider name like "claude"/"codex".
pub async fn resolve_bot_http(
    registry: &HealthRegistry,
    bot: &str,
) -> Result<Arc<serenity::Http>, (&'static str, String)> {
    match bot {
        "notify" => {
            let guard = registry.notify_http.lock().await;
            match guard.as_ref() {
                Some(http) => Ok(http.clone()),
                None => Err((
                    "503 Service Unavailable",
                    r#"{"ok":false,"error":"notify bot not configured (missing credential/notify_bot_token)"}"#.to_string(),
                )),
            }
        }
        "announce" => {
            let guard = registry.announce_http.lock().await;
            match guard.as_ref() {
                Some(http) => Ok(http.clone()),
                None => Err((
                    "503 Service Unavailable",
                    r#"{"ok":false,"error":"announce bot not configured (missing credential/announce_bot_token)"}"#.to_string(),
                )),
            }
        }
        provider => {
            // Look up provider bot (e.g. "claude", "codex")
            let clients = registry.discord_http.lock().await;
            for (name, http) in clients.iter() {
                if name == provider {
                    return Ok(http.clone());
                }
            }
            Err((
                "400 Bad Request",
                format!(r#"{{"ok":false,"error":"unknown bot: {provider}"}}"#),
            ))
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SendTargetResolutionError {
    BadRequest(&'static str),
    NotFound(String),
    Internal(String),
}

fn parse_channel_target_value(target: &str) -> Option<u64> {
    let trimmed = target.trim();
    trimmed
        .parse::<u64>()
        .ok()
        .or_else(|| crate::server::routes::dispatches::resolve_channel_alias_pub(trimmed))
}

fn resolve_send_target_channel_id(db: &Db, target: &str) -> Result<u64, SendTargetResolutionError> {
    if let Some(agent_id_raw) = target.strip_prefix("agent:") {
        let agent_id = agent_id_raw.trim();
        if agent_id.is_empty() {
            return Err(SendTargetResolutionError::BadRequest(
                "invalid target format (use channel:<id>, channel:<name>, or agent:<roleId>)",
            ));
        }

        let conn = db.lock().map_err(|e| {
            SendTargetResolutionError::Internal(format!("db lock failed during agent lookup: {e}"))
        })?;
        let bindings = crate::db::agents::load_agent_channel_bindings(&conn, agent_id)
            .map_err(|e| {
                SendTargetResolutionError::Internal(format!(
                    "agent lookup failed for {agent_id}: {e}"
                ))
            })?
            .ok_or_else(|| {
                SendTargetResolutionError::NotFound(format!("unknown agent target: {agent_id}"))
            })?;
        let channel_target = bindings.primary_channel().ok_or_else(|| {
            SendTargetResolutionError::NotFound(format!(
                "agent target has no primary channel: {agent_id}"
            ))
        })?;

        return parse_channel_target_value(&channel_target).ok_or_else(|| {
            SendTargetResolutionError::Internal(format!(
                "agent target resolved to invalid channel: {channel_target}"
            ))
        });
    }

    let channel_target = target.strip_prefix("channel:").unwrap_or(target);
    parse_channel_target_value(channel_target).ok_or(SendTargetResolutionError::BadRequest(
        "invalid target format (use channel:<id>, channel:<name>, or agent:<roleId>)",
    ))
}

/// Handle POST /api/send — agent-to-agent native routing.
/// Accepts JSON: {"target":"channel:<id>|channel:<name>|agent:<roleId>", "content":"...", "source":"role-id", "bot":"announce|notify"}
pub async fn handle_send<'a>(registry: &HealthRegistry, db: &Db, body: &str) -> (&'a str, String) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
        );
    };

    let target = json.get("target").and_then(|v| v.as_str()).unwrap_or("");
    let content = json.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let source = json
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let bot = json
        .get("bot")
        .and_then(|v| v.as_str())
        .unwrap_or("announce");

    if content.is_empty() {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"content is required"}"#.to_string(),
        );
    }

    let channel_id_raw = match resolve_send_target_channel_id(db, target) {
        Ok(id) => id,
        Err(SendTargetResolutionError::BadRequest(message)) => {
            return (
                "400 Bad Request",
                serde_json::json!({"ok": false, "error": message}).to_string(),
            );
        }
        Err(SendTargetResolutionError::NotFound(message)) => {
            return (
                "404 Not Found",
                serde_json::json!({"ok": false, "error": message}).to_string(),
            );
        }
        Err(SendTargetResolutionError::Internal(message)) => {
            return (
                "500 Internal Server Error",
                serde_json::json!({"ok": false, "error": message}).to_string(),
            );
        }
    };

    let channel_id = ChannelId::new(channel_id_raw);

    // Validate source is a known agent role_id or internal system source
    if !is_allowed_send_source(source) {
        return (
            "403 Forbidden",
            format!(
                r#"{{"ok":false,"error":"unknown source role: {}"}}"#,
                source
            ),
        );
    }

    // Verify target channel exists in role-map (authorization check).
    // If the target is a thread, resolve its parent channel and check that instead.
    // Pass channel name so byChannelName-style configs can match.
    if super::settings::resolve_role_binding(channel_id, None).is_none() {
        let mut authorized = false;
        // Try resolving as a thread: fetch channel info and check parent_id
        if let Ok(http) = resolve_bot_http(registry, bot).await {
            if let Ok(channel) = channel_id.to_channel(&*http).await {
                if let Some(guild_channel) = channel.guild() {
                    if let Some(parent_id) = guild_channel.parent_id {
                        // Resolve parent channel name for byChannelName configs
                        let parent_name = if let Ok(parent_ch) = parent_id.to_channel(&*http).await
                        {
                            parent_ch.guild().map(|pg| pg.name.clone())
                        } else {
                            None
                        };
                        if super::settings::resolve_role_binding(parent_id, parent_name.as_deref())
                            .is_some()
                        {
                            authorized = true;
                        }
                    }
                }
            }
        }
        if !authorized {
            return (
                "403 Forbidden",
                r#"{"ok":false,"error":"channel not in role-map"}"#.to_string(),
            );
        }
    }

    // Select bot: "announce" (default, agents respond) or "notify" (info-only, agents ignore)
    let http = match resolve_bot_http(registry, bot).await {
        Ok(h) => h,
        Err(resp) => return resp,
    };

    match channel_id.say(&*http, content).await {
        Ok(_) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let emoji = if bot == "notify" { "🔔" } else { "📨" };
            println!("  [{ts}] {emoji} ROUTE: [{source}] → channel {channel_id} (bot={bot})");
            let mut response = serde_json::json!({
                "ok": true,
                "target": format!("channel:{channel_id}"),
                "source": source,
                "bot": bot,
            });
            if target != format!("channel:{channel_id}") {
                response["requested_target"] = serde_json::Value::String(target.to_string());
            }
            ("200 OK", response.to_string())
        }
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!("  [{ts}] ⚠ ROUTE: failed to send to channel {channel_id}: {e}");
            (
                "500 Internal Server Error",
                format!(r#"{{"ok":false,"error":"Discord send failed: {}"}}"#, e),
            )
        }
    }
}

fn is_allowed_send_source(source: &str) -> bool {
    const INTERNAL_SOURCES: &[&str] = &[
        "kanban-rules",
        "triage-rules",
        "review-automation",
        "auto-queue",
        "pipeline",
        "system",
        "timeouts",
        "merge-automation",
        "dashboard",
    ];

    INTERNAL_SOURCES.contains(&source) || super::settings::is_known_agent(source)
}

pub async fn fetch_channel_name(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    provider: &ProviderKind,
) -> Option<String> {
    let http = resolve_bot_http(registry, provider.as_str()).await.ok()?;
    let channel = channel_id.to_channel(&*http).await.ok()?;
    channel.guild().map(|guild_channel| guild_channel.name)
}

pub async fn start_direct_meeting(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: ProviderKind,
    primary_provider: ProviderKind,
    reviewer_provider: ProviderKind,
    agenda: String,
    fixed_participants: Vec<String>,
) -> Result<(), String> {
    let http = resolve_bot_http(registry, owner_provider.as_str())
        .await
        .map_err(|(_, body)| body)?;

    let shared = {
        let providers = registry.providers.lock().await;
        providers
            .iter()
            .find(|entry| entry.name == owner_provider.as_str())
            .map(|entry| entry.shared.clone())
            .ok_or_else(|| {
                format!(
                    r#"{{"ok":false,"error":"provider runtime not registered: {}"}}"#,
                    owner_provider.as_str()
                )
            })?
    };

    super::meeting::spawn_direct_start(
        http,
        channel_id,
        agenda,
        primary_provider,
        reviewer_provider,
        fixed_participants,
        shared,
    )
    .await
}

/// Handle POST /api/senddm — send a DM to a Discord user.
/// Accepts JSON: {"user_id":"...", "content":"...", "bot":"announce|notify"}
/// When using announce bot, user replies trigger a Claude session.
pub async fn handle_senddm(registry: &HealthRegistry, body: &str) -> (&'static str, String) {
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => {
            return (
                "400 Bad Request",
                r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
            );
        }
    };

    let user_id_raw: u64 = parsed["user_id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| parsed["user_id"].as_u64())
        .unwrap_or(0);
    if user_id_raw == 0 {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"user_id required (string or number)"}"#.to_string(),
        );
    }

    let content = match parsed["content"].as_str() {
        Some(c) if !c.is_empty() => c,
        _ => {
            return (
                "400 Bad Request",
                r#"{"ok":false,"error":"content required"}"#.to_string(),
            );
        }
    };

    let bot = parsed["bot"].as_str().unwrap_or("announce");
    let http = match resolve_bot_http(registry, bot).await {
        Ok(h) => h,
        Err(resp) => return resp,
    };

    use poise::serenity_prelude::{CreateMessage, UserId};
    let user_id = UserId::new(user_id_raw);
    match user_id.create_dm_channel(&*http).await {
        Ok(dm_channel) => {
            match dm_channel
                .id
                .send_message(&*http, CreateMessage::new().content(content))
                .await
            {
                Ok(_) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] 📨 DM: → user {user_id_raw}");
                    (
                        "200 OK",
                        format!(r#"{{"ok":true,"user_id":"{}"}}"#, user_id_raw),
                    )
                }
                Err(e) => (
                    "500 Internal Server Error",
                    format!(r#"{{"ok":false,"error":"DM send failed: {}"}}"#, e),
                ),
            }
        }
        Err(e) => (
            "500 Internal Server Error",
            format!(
                r#"{{"ok":false,"error":"DM channel creation failed: {}"}}"#,
                e
            ),
        ),
    }
}

/// Handle POST /api/session/start — start a session via API.
/// Accepts JSON: {"channel_id":"<id>", "path":"/some/path", "provider":"claude"}
/// Creates a DiscordSession in the provider's SharedData and responds.
pub async fn handle_session_start<'a>(registry: &HealthRegistry, body: &str) -> (&'a str, String) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
        );
    };

    let channel_id_str = json
        .get("channel_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let path = json.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let provider_hint = json.get("provider").and_then(|v| v.as_str()).unwrap_or("");

    let Some(channel_id_raw) = channel_id_str.parse::<u64>().ok() else {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"channel_id must be a numeric string"}"#.to_string(),
        );
    };

    // Resolve path — expand ~ and . to absolute
    let effective_path = if path == "." || path.is_empty() {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string())
    } else if path.starts_with('~') {
        dirs::home_dir()
            .map(|h| path.replacen('~', &h.to_string_lossy(), 1))
            .unwrap_or_else(|| path.to_string())
    } else {
        path.to_string()
    };

    let channel_id = ChannelId::new(channel_id_raw);

    // Find the matching provider
    let providers = registry.providers.lock().await;

    // Try to match by provider hint, or by channel name suffix
    let target_provider = if !provider_hint.is_empty() {
        providers.iter().find(|p| p.name == provider_hint)
    } else {
        // Try to detect from channel_id via role binding
        let binding = super::settings::resolve_role_binding(channel_id, None);
        let bound_provider = binding.as_ref().and_then(|b| b.provider.as_ref());
        match bound_provider {
            Some(p) => providers.iter().find(|e| &e.name == p.as_str()),
            None => providers.first(),
        }
    };

    let Some(provider_entry) = target_provider else {
        return (
            "404 Not Found",
            r#"{"ok":false,"error":"no matching provider found"}"#.to_string(),
        );
    };

    // Create session
    {
        let mut data = provider_entry.shared.core.lock().await;
        let session = data
            .sessions
            .entry(channel_id)
            .or_insert_with(|| super::DiscordSession {
                session_id: None,
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                channel_name: None,
                category_name: None,
                remote_profile_name: None,
                channel_id: Some(channel_id_raw),
                last_active: tokio::time::Instant::now(),
                worktree: None,

                born_generation: super::runtime_store::load_generation(),
            });
        session.current_path = Some(effective_path.clone());
        session.last_active = tokio::time::Instant::now();
    }

    let response = format!(
        r#"{{"ok":true,"channel_id":"{}","path":"{}","provider":"{}"}}"#,
        channel_id_raw, effective_path, provider_entry.name
    );
    ("200 OK", response)
}

/// Self-watchdog: runs on a dedicated OS thread (not tokio) to detect
/// runtime hangs.  Periodically opens a raw TCP connection to the server
/// port and expects a response within a few seconds.  If the check fails
/// `max_failures` times in a row the process is force-killed so launchd
/// (or systemd) can restart it.
pub fn spawn_watchdog(port: u16) {
    const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    const TCP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    const MAX_FAILURES: u32 = 3;
    // Grace period: skip checks for the first 30s after startup so the
    // runtime has time to initialise Discord bots and register providers.
    const STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

    std::thread::Builder::new()
        .name("health-watchdog".into())
        .spawn(move || {
            std::thread::sleep(STARTUP_GRACE);

            let mut consecutive_failures: u32 = 0;

            loop {
                std::thread::sleep(CHECK_INTERVAL);

                let ok = (|| -> bool {
                    use std::io::{Read, Write};
                    let loopback = crate::config::loopback();
                    let addr = format!("{loopback}:{port}");
                    let mut stream =
                        match std::net::TcpStream::connect_timeout(
                            &addr.parse().unwrap(),
                            TCP_TIMEOUT,
                        ) {
                            Ok(s) => s,
                            Err(_) => return false,
                        };
                    let _ = stream.set_read_timeout(Some(TCP_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(TCP_TIMEOUT));
                    let req = format!("GET /api/health HTTP/1.1\r\nHost: {loopback}\r\nConnection: close\r\n\r\n");
                    if stream.write_all(req.as_bytes()).is_err() {
                        return false;
                    }
                    let mut buf = [0u8; 512];
                    match stream.read(&mut buf) {
                        Ok(n) if n > 0 => {
                            // Any HTTP response means the process is alive and serving.
                            // Only TCP failure (Err/_) indicates a true hang/deadlock.
                            // A 503 (degraded/unhealthy state) still means the runtime is
                            // responsive — killing it would create an infinite crash loop
                            // when a provider is temporarily disconnected.
                            true
                        }
                        _ => false,
                    }
                })();

                if ok {
                    if consecutive_failures > 0 {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        eprintln!(
                            "  [{ts}] 🩺 watchdog: health recovered after {consecutive_failures} failure(s)"
                        );
                    }
                    consecutive_failures = 0;
                } else {
                    consecutive_failures += 1;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!(
                        "  [{ts}] 🩺 watchdog: health check failed ({consecutive_failures}/{MAX_FAILURES})"
                    );
                    if consecutive_failures >= MAX_FAILURES {
                        eprintln!(
                            "  [{ts}] 🩺 watchdog: runtime unresponsive — capturing diagnostics before exit"
                        );
                        // Capture process dump for post-mortem analysis (platform-aware)
                        // Write to runtime root's logs/ dir so dumps survive /tmp cleanup
                        let pid = std::process::id();
                        let dump_dir = crate::agentdesk_runtime_root()
                            .map(|r| r.join("logs"))
                            .unwrap_or_else(|| std::env::temp_dir());
                        let _ = std::fs::create_dir_all(&dump_dir);
                        let dump_path = format!(
                            "{}/adk-hang-{}-{}.txt",
                            dump_dir.display(),
                            pid,
                            chrono::Local::now().format("%Y%m%d-%H%M%S")
                        );
                        match crate::services::platform::capture_process_dump(pid, &dump_path) {
                            Ok(()) => eprintln!(
                                "  [{ts}] 🩺 watchdog: dump saved to {dump_path} — forcing exit"
                            ),
                            Err(e) => eprintln!(
                                "  [{ts}] 🩺 watchdog: dump capture failed ({e}) — forcing exit without diagnostics"
                            ),
                        }
                        std::process::exit(1);
                    }
                }
            }
        })
        .expect("Failed to spawn watchdog thread");
}

/// Parse a /api/send JSON body and extract (target, content, source).
/// Returns Err with an error message on invalid input.
/// Factored out of handle_send for testability.
#[cfg_attr(not(test), allow(dead_code))]
fn parse_send_body(body: &str) -> Result<(String, String, String), &'static str> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|_| "invalid JSON")?;
    let content = json
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if content.is_empty() {
        return Err("content is required");
    }
    let target = json
        .get("target")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let source = json
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    Ok((target, content, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    #[test]
    fn test_parse_send_request_valid_json() {
        let body = r#"{"target":"channel:123","content":"hello","source":"agent-a"}"#;
        let result = parse_send_body(body);
        assert!(result.is_ok(), "Valid JSON should parse successfully");
        let (target, content, source) = result.unwrap();
        assert_eq!(target, "channel:123");
        assert_eq!(content, "hello");
        assert_eq!(source, "agent-a");
    }

    #[test]
    fn test_parse_send_request_missing_content() {
        let body = r#"{"target":"channel:123"}"#;
        let result = parse_send_body(body);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "content is required");
    }

    #[test]
    fn test_parse_send_request_empty_content() {
        let body = r#"{"target":"channel:123","content":""}"#;
        let result = parse_send_body(body);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "content is required");
    }

    #[test]
    fn test_parse_send_request_invalid_json() {
        let body = "not json at all";
        let result = parse_send_body(body);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "invalid JSON");
    }

    #[test]
    fn test_parse_send_request_missing_target_defaults_empty() {
        let body = r#"{"content":"hello world"}"#;
        let result = parse_send_body(body);
        assert!(result.is_ok());
        let (target, content, source) = result.unwrap();
        assert_eq!(target, "");
        assert_eq!(content, "hello world");
        assert_eq!(source, "unknown");
    }

    #[test]
    fn test_parse_send_request_missing_source_defaults_unknown() {
        let body = r#"{"target":"channel:999","content":"msg"}"#;
        let result = parse_send_body(body);
        assert!(result.is_ok());
        let (_, _, source) = result.unwrap();
        assert_eq!(source, "unknown");
    }

    #[test]
    fn test_resolve_send_target_channel_id_supports_channel_target() {
        let db = test_db();
        let resolved = resolve_send_target_channel_id(&db, "channel:123").unwrap();
        assert_eq!(resolved, 123);
    }

    #[test]
    fn test_resolve_send_target_channel_id_uses_agent_primary_channel_for_claude() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
                 VALUES ('agent-claude', 'Claude Agent', 'claude', '111', '222')",
                [],
            )
            .unwrap();
        }

        let resolved = resolve_send_target_channel_id(&db, "agent:agent-claude").unwrap();
        assert_eq!(resolved, 111);
    }

    #[test]
    fn test_resolve_send_target_channel_id_uses_agent_primary_channel_for_codex() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
                 VALUES ('agent-codex', 'Codex Agent', 'codex', '111', '222')",
                [],
            )
            .unwrap();
        }

        let resolved = resolve_send_target_channel_id(&db, "agent:agent-codex").unwrap();
        assert_eq!(resolved, 222);
    }

    #[test]
    fn test_resolve_send_target_channel_id_rejects_unknown_agent_target() {
        let db = test_db();
        let err = resolve_send_target_channel_id(&db, "agent:missing").unwrap_err();
        assert_eq!(
            err,
            SendTargetResolutionError::NotFound("unknown agent target: missing".to_string())
        );
    }

    #[tokio::test]
    async fn health_snapshot_reports_observability_metrics_and_degraded_queue_state() {
        let harness = TestHealthHarness::new().await;
        harness.set_deferred_hooks(2);
        harness.set_recovery_duration_ms(4_250);
        harness.set_queue_depth(3).await;

        let snapshot = build_health_snapshot(&harness.registry()).await;
        let json = serde_json::to_value(&snapshot).unwrap();

        assert_eq!(snapshot.status(), HealthStatus::Degraded);
        assert_eq!(json["deferred_hooks"], 2);
        assert_eq!(json["queue_depth"], 3);
        assert_eq!(json["watcher_count"], 0);
        assert_eq!(json["recovery_duration"], 4.25);
        assert!(
            json["degraded_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason == "provider:claude:deferred_hooks_backlog:2")
        );
        assert!(
            json["degraded_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason == "provider:claude:pending_queue_depth:3")
        );
    }

    #[test]
    fn dashboard_is_allowed_send_source() {
        assert!(is_allowed_send_source("dashboard"));
    }

    #[test]
    fn unknown_send_source_is_rejected() {
        assert!(!is_allowed_send_source("totally-unknown-source"));
    }
}
