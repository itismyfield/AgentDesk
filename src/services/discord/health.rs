use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serde::Serialize;
use serenity::ChannelId;

use super::{
    SharedData, clear_inflight_state, mailbox_cancel_active_turn, mailbox_clear_channel,
    mailbox_clear_recovery_marker, mailbox_finish_turn,
};
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
    /// Cached Discord user id for the announce bot.
    announce_user_id: tokio::sync::Mutex<Option<u64>>,
    /// Dedicated HTTP client for the notify bot (info-only notifications).
    /// Agents do NOT process notify bot messages — use for non-actionable alerts.
    notify_http: tokio::sync::Mutex<Option<Arc<serenity::Http>>>,
    /// Cached Discord user id for the notify bot.
    notify_user_id: tokio::sync::Mutex<Option<u64>>,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self {
            providers: tokio::sync::Mutex::new(Vec::new()),
            started_at: Instant::now(),
            discord_http: tokio::sync::Mutex::new(Vec::new()),
            announce_http: tokio::sync::Mutex::new(None),
            announce_user_id: tokio::sync::Mutex::new(None),
            notify_http: tokio::sync::Mutex::new(None),
            notify_user_id: tokio::sync::Mutex::new(None),
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

    /// Load announce + notify bot tokens from the canonical runtime credential path.
    /// Call once at startup before the axum server begins accepting requests.
    pub async fn init_bot_tokens(&self) {
        if super::runtime_store::agentdesk_root().is_some() {
            for (bot_name, field) in [
                ("announce", &self.announce_http),
                ("notify", &self.notify_http),
            ] {
                if let Some(token) = crate::credential::read_bot_token(bot_name) {
                    let http = Arc::new(serenity::Http::new(&format!("Bot {token}")));
                    *field.lock().await = Some(http);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    let emoji = if bot_name == "announce" {
                        "📢"
                    } else {
                        "🔔"
                    };
                    tracing::info!("  [{ts}] {emoji} {bot_name} bot loaded for /api/send routing");
                }
            }
        }
    }

    pub async fn utility_bot_user_id(&self, bot_name: &str) -> Option<u64> {
        match bot_name {
            "announce" => {
                if let Some(id) = *self.announce_user_id.lock().await {
                    return Some(id);
                }
                let http = { self.announce_http.lock().await.clone()? };
                let user = http.get_current_user().await.ok()?;
                let id = user.id.get();
                *self.announce_user_id.lock().await = Some(id);
                Some(id)
            }
            "notify" => {
                if let Some(id) = *self.notify_user_id.lock().await {
                    return Some(id);
                }
                let http = { self.notify_http.lock().await.clone()? };
                let user = http.get_current_user().await.ok()?;
                let id = user.id.get();
                *self.notify_user_id.lock().await = Some(id);
                Some(id)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeTurnStopResult {
    pub lifecycle_path: &'static str,
    pub queue_depth: usize,
    pub termination_recorded: bool,
}

fn decrement_counter(counter: &AtomicUsize) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        current.checked_sub(1)
    });
}

async fn shared_for_provider(
    registry: &HealthRegistry,
    provider: &ProviderKind,
) -> Option<Arc<SharedData>> {
    let providers = registry.providers.lock().await;
    providers
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(provider.as_str()))
        .map(|entry| entry.shared.clone())
}

async fn wait_for_turn_end(
    shared: &SharedData,
    channel_id: ChannelId,
    timeout: std::time::Duration,
) -> bool {
    let start = tokio::time::Instant::now();
    while shared.mailbox(channel_id).has_active_turn().await {
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    true
}

fn runtime_stop_wait_timeout() -> std::time::Duration {
    #[cfg(test)]
    {
        std::time::Duration::from_millis(150)
    }
    #[cfg(not(test))]
    {
        std::time::Duration::from_secs(3)
    }
}

pub async fn stop_provider_channel_runtime(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    reason: &str,
) -> Option<RuntimeTurnStopResult> {
    let provider = ProviderKind::from_str(provider_name)?;
    let shared = shared_for_provider(registry, &provider).await?;
    let result = mailbox_cancel_active_turn(&shared, channel_id).await;

    if let Some(token) = result.token.as_ref() {
        if !result.already_stopping {
            super::turn_bridge::cancel_active_token(token, true, reason);
        }
        if wait_for_turn_end(&shared, channel_id, runtime_stop_wait_timeout()).await {
            let snapshot = shared.mailbox(channel_id).snapshot().await;
            return Some(RuntimeTurnStopResult {
                lifecycle_path: "canonical",
                queue_depth: snapshot.intervention_queue.len(),
                termination_recorded: result.token.is_some(),
            });
        }
    }

    let finish = mailbox_finish_turn(&shared, &provider, channel_id).await;
    if let Some(token) = finish.removed_token.as_ref() {
        super::turn_bridge::cancel_active_token(token, true, reason);
    }
    apply_runtime_hard_stop_cleanup(
        &shared,
        &provider,
        channel_id,
        &finish,
        "runtime_stop_fallback",
    )
    .await;
    let queue_depth = shared
        .mailbox(channel_id)
        .snapshot()
        .await
        .intervention_queue
        .len();
    mailbox_clear_recovery_marker(&shared, channel_id).await;
    clear_inflight_state(&provider, channel_id.get());

    Some(RuntimeTurnStopResult {
        lifecycle_path: "runtime-fallback",
        queue_depth,
        termination_recorded: finish.removed_token.is_some(),
    })
}

pub async fn active_request_owner_for_channel(
    registry: &HealthRegistry,
    channel_id: u64,
) -> Option<u64> {
    let channel_id = ChannelId::new(channel_id);
    let providers: Vec<_> = registry
        .providers
        .lock()
        .await
        .iter()
        .map(|entry| entry.shared.clone())
        .collect();
    for shared in providers {
        let snapshots = shared.mailboxes.snapshot_all().await;
        if let Some(owner) = snapshots
            .get(&channel_id)
            .and_then(|snapshot| snapshot.active_request_owner)
        {
            return Some(owner.get());
        }
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HardStopRuntimeResult {
    pub cleanup_path: &'static str,
    pub had_active_turn: bool,
    pub has_pending_queue: bool,
    pub runtime_session_cleared: bool,
}

impl Default for HardStopRuntimeResult {
    fn default() -> Self {
        Self {
            cleanup_path: "runtime_unavailable_fallback",
            had_active_turn: false,
            has_pending_queue: false,
            runtime_session_cleared: false,
        }
    }
}

struct RuntimeChannelMatch {
    provider: ProviderKind,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
}

async fn find_runtime_channel_match(
    registry: &HealthRegistry,
    provider_name: Option<&str>,
    channel_id: Option<ChannelId>,
    tmux_name: Option<&str>,
) -> Option<RuntimeChannelMatch> {
    let preferred_provider = provider_name.and_then(ProviderKind::from_str);
    let providers: Vec<_> = registry
        .providers
        .lock()
        .await
        .iter()
        .filter_map(|entry| {
            let provider = ProviderKind::from_str(&entry.name)?;
            if preferred_provider
                .as_ref()
                .is_some_and(|preferred| preferred != &provider)
            {
                return None;
            }
            Some((provider, entry.shared.clone()))
        })
        .collect();

    for (provider, shared) in providers {
        if let Some(channel_id) = channel_id {
            let has_session = {
                let data = shared.core.lock().await;
                data.sessions.contains_key(&channel_id)
            };
            if has_session || super::ChannelMailboxRegistry::global_handle(channel_id).is_some() {
                return Some(RuntimeChannelMatch {
                    provider,
                    shared,
                    channel_id,
                });
            }
            continue;
        }

        let Some(tmux_name) = tmux_name else {
            continue;
        };
        let matched_channel_id = {
            let data = shared.core.lock().await;
            data.sessions
                .iter()
                .find_map(|(candidate_channel_id, session)| {
                    session.channel_name.as_ref().and_then(|channel_name| {
                        let expected_tmux_name = provider.build_tmux_session_name(channel_name);
                        (expected_tmux_name == tmux_name).then_some(*candidate_channel_id)
                    })
                })
        };
        if let Some(channel_id) = matched_channel_id {
            return Some(RuntimeChannelMatch {
                provider,
                shared,
                channel_id,
            });
        }
    }

    None
}

async fn apply_runtime_hard_stop_cleanup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    finish: &super::FinishTurnResult,
    stop_source: &'static str,
) -> bool {
    if let Some(token) = finish.removed_token.as_ref() {
        token.cancelled.store(true, Ordering::Relaxed);
        shared.global_active.fetch_sub(1, Ordering::Relaxed);
    }

    super::clear_watchdog_deadline_override(channel_id.get()).await;
    shared
        .dispatch_thread_parents
        .retain(|_, thread| *thread != channel_id);
    shared.recovering_channels.remove(&channel_id);
    shared.turn_start_times.remove(&channel_id);

    if !finish.has_pending {
        shared.dispatch_role_overrides.remove(&channel_id);
    }

    if let Some((_, watcher)) = shared.tmux_watchers.remove(&channel_id) {
        watcher.cancel.store(true, Ordering::Relaxed);
    }

    let runtime_session_cleared = {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
            true
        } else {
            false
        }
    };

    if finish.mailbox_online && finish.has_pending {
        super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            stop_source,
        );
    }

    runtime_session_cleared
}

pub async fn hard_stop_runtime_turn(
    registry: Option<&HealthRegistry>,
    provider_name: Option<&str>,
    channel_id: Option<u64>,
    tmux_name: Option<&str>,
    stop_source: &'static str,
) -> HardStopRuntimeResult {
    let channel_id = channel_id.map(ChannelId::new);

    if let Some(registry) = registry
        && let Some(runtime) =
            find_runtime_channel_match(registry, provider_name, channel_id, tmux_name).await
    {
        let finish = if let Some(handle) =
            super::ChannelMailboxRegistry::global_handle(runtime.channel_id)
        {
            handle
                .finish_turn(super::queue_persistence_context(
                    &runtime.shared,
                    &runtime.provider,
                    runtime.channel_id,
                ))
                .await
        } else {
            super::FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: false,
                queue_exit_events: Vec::new(),
            }
        };
        let runtime_session_cleared = apply_runtime_hard_stop_cleanup(
            &runtime.shared,
            &runtime.provider,
            runtime.channel_id,
            &finish,
            stop_source,
        )
        .await;
        return HardStopRuntimeResult {
            cleanup_path: if finish.mailbox_online {
                "mailbox_canonical"
            } else {
                "mailbox_fallback"
            },
            had_active_turn: finish.removed_token.is_some(),
            has_pending_queue: finish.has_pending,
            runtime_session_cleared,
        };
    }

    if let Some(channel_id) = channel_id
        && let Some(handle) = super::ChannelMailboxRegistry::global_handle(channel_id)
    {
        let finish = handle.hard_stop().await;
        super::clear_watchdog_deadline_override(channel_id.get()).await;
        return HardStopRuntimeResult {
            cleanup_path: if finish.mailbox_online {
                "mailbox_canonical"
            } else {
                "mailbox_fallback"
            },
            had_active_turn: finish.removed_token.is_some(),
            has_pending_queue: finish.has_pending,
            runtime_session_cleared: false,
        };
    }

    HardStopRuntimeResult::default()
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
        decrement_counter(shared.global_active.as_ref());
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
    provider: ProviderKind,
    registry: Arc<HealthRegistry>,
    shared: Arc<SharedData>,
}

#[cfg(test)]
impl TestHealthHarness {
    pub(crate) async fn new() -> Self {
        Self::new_with_provider(ProviderKind::Claude).await
    }

    pub(crate) async fn new_with_provider(provider: ProviderKind) -> Self {
        let registry = Arc::new(HealthRegistry::new());
        let global_active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let global_finalizing = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let shutdown_remaining = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let mut settings = super::DiscordBotSettings::default();
        settings.provider = provider.clone();
        let shared = Arc::new(SharedData {
            core: tokio::sync::Mutex::new(super::CoreState {
                sessions: std::collections::HashMap::new(),
                active_meetings: std::collections::HashMap::new(),
            }),
            mailboxes: super::ChannelMailboxRegistry::default(),
            settings: tokio::sync::RwLock::new(settings),
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
            health_registry: Arc::downgrade(&registry),
            known_slash_commands: tokio::sync::OnceCell::new(),
        });
        super::mark_reconcile_complete(&shared);
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        Self {
            provider,
            registry,
            shared,
        }
    }

    pub(crate) fn registry(&self) -> Arc<HealthRegistry> {
        self.registry.clone()
    }

    fn shared(&self) -> Arc<SharedData> {
        self.shared.clone()
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
        self.set_queue_depth_for_channel(1, ProviderKind::Claude, depth)
            .await;
    }

    pub(crate) async fn set_queue_depth_for_channel(
        &self,
        channel_id: u64,
        provider: ProviderKind,
        depth: usize,
    ) {
        super::mailbox_replace_queue(
            &self.shared,
            &provider,
            ChannelId::new(channel_id),
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
                source_message_ids: vec![serenity::MessageId::new(idx as u64 + 1)],
                text: format!("queued-{idx}"),
                mode: super::InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            })
            .collect::<Vec<_>>();
        super::mailbox_replace_queue(&self.shared, &provider, ChannelId::new(channel_id), queue)
            .await;
    }

    pub(crate) async fn queue_depth_for_channel(&self, channel_id: u64) -> usize {
        self.shared
            .mailbox(ChannelId::new(channel_id))
            .snapshot()
            .await
            .intervention_queue
            .len()
    }

    pub(crate) async fn seed_channel_session(
        &self,
        channel_id: u64,
        channel_name: &str,
        session_id: Option<&str>,
    ) {
        let mut data = self.shared.core.lock().await;
        data.sessions.insert(
            ChannelId::new(channel_id),
            super::DiscordSession {
                session_id: session_id.map(str::to_string),
                memento_context_loaded: session_id.is_some(),
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(channel_id),
                channel_name: Some(channel_name.to_string()),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: 0,
                assistant_turns: 0,
            },
        );
    }

    pub(crate) async fn start_active_turn(
        &self,
        channel_id: u64,
        user_id: u64,
        message_id: u64,
        tmux_name: Option<&str>,
    ) -> Arc<crate::services::provider::CancelToken> {
        let token = Arc::new(crate::services::provider::CancelToken::new());
        if let Some(tmux_name) = tmux_name {
            *token.tmux_session.lock().unwrap() = Some(tmux_name.to_string());
        }
        let started = self
            .shared
            .mailbox(ChannelId::new(channel_id))
            .try_start_turn(
                token.clone(),
                serenity::UserId::new(user_id),
                serenity::MessageId::new(message_id),
            )
            .await;
        assert!(started, "test active turn should start");
        self.shared.global_active.fetch_add(1, Ordering::Relaxed);
        token
    }

    pub(crate) async fn seed_active_turn(
        &self,
        channel_id: u64,
        request_owner: u64,
        user_message_id: u64,
    ) {
        let started = self
            .shared
            .mailbox(ChannelId::new(channel_id))
            .try_start_turn(
                Arc::new(crate::services::provider::CancelToken::new()),
                serenity::UserId::new(request_owner),
                serenity::MessageId::new(user_message_id),
            )
            .await;
        assert!(started, "test harness expected an idle mailbox");
        self.shared.global_active.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) async fn seed_queue(&self, channel_id: u64, queue_items: &[(u64, &str)]) {
        let queue = queue_items
            .iter()
            .map(|(message_id, text)| super::Intervention {
                author_id: serenity::UserId::new(1),
                message_id: serenity::MessageId::new(*message_id),
                source_message_ids: vec![serenity::MessageId::new(*message_id)],
                text: (*text).to_string(),
                mode: super::InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            })
            .collect::<Vec<_>>();
        super::mailbox_replace_queue(
            &self.shared,
            &self.provider,
            ChannelId::new(channel_id),
            queue,
        )
        .await;
    }

    pub(crate) async fn mailbox_state(&self, channel_id: u64) -> (bool, usize, Option<String>) {
        let snapshot = super::mailbox_snapshot(&self.shared, ChannelId::new(channel_id)).await;
        let session_id = {
            let data = self.shared.core.lock().await;
            data.sessions
                .get(&ChannelId::new(channel_id))
                .and_then(|session| session.session_id.clone())
        };
        (
            snapshot.cancel_token.is_some(),
            snapshot.intervention_queue.len(),
            session_id,
        )
    }

    pub(crate) fn has_dispatch_role_override(&self, channel_id: u64) -> bool {
        self.shared
            .dispatch_role_overrides
            .contains_key(&ChannelId::new(channel_id))
    }

    pub(crate) fn insert_dispatch_role_override(&self, channel_id: u64, override_channel_id: u64) {
        self.shared.dispatch_role_overrides.insert(
            ChannelId::new(channel_id),
            ChannelId::new(override_channel_id),
        );
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

pub async fn fetch_channel_name(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    provider: &ProviderKind,
) -> Option<String> {
    let http = resolve_bot_http(registry, provider.as_str()).await.ok()?;
    let channel = channel_id.to_channel(&*http).await.ok()?;
    channel.guild().map(|guild_channel| guild_channel.name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirectMeetingRuntimeCandidate {
    index: usize,
    explicit_channel_match: bool,
    live_channel_match: bool,
}

fn select_direct_meeting_runtime_candidate(
    provider_name: &str,
    channel_id: ChannelId,
    candidates: &[DirectMeetingRuntimeCandidate],
) -> Result<Option<usize>, String> {
    let explicit_matches = candidates
        .iter()
        .filter(|candidate| candidate.explicit_channel_match)
        .map(|candidate| candidate.index)
        .collect::<Vec<_>>();
    if explicit_matches.len() > 1 {
        return Err(serde_json::json!({
            "ok": false,
            "error": format!(
                "multiple runtimes explicitly allow channel {} for provider {}",
                channel_id.get(),
                provider_name
            ),
        })
        .to_string());
    }
    if let Some(index) = explicit_matches.first().copied() {
        return Ok(Some(index));
    }

    let live_matches = candidates
        .iter()
        .filter(|candidate| candidate.live_channel_match)
        .map(|candidate| candidate.index)
        .collect::<Vec<_>>();
    if live_matches.len() > 1 {
        return Err(serde_json::json!({
            "ok": false,
            "error": format!(
                "multiple runtimes can handle channel {} for provider {}",
                channel_id.get(),
                provider_name
            ),
        })
        .to_string());
    }
    Ok(live_matches.first().copied())
}

async fn resolve_direct_meeting_runtime(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: &ProviderKind,
) -> Result<(Arc<serenity::Http>, Arc<SharedData>), String> {
    let provider_name = owner_provider.as_str();
    let shared_candidates = {
        let providers = registry.providers.lock().await;
        providers
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.eq_ignore_ascii_case(provider_name))
            .map(|(index, entry)| (index, entry.shared.clone()))
            .collect::<Vec<_>>()
    };

    if shared_candidates.is_empty() {
        return Err(serde_json::json!({
            "ok": false,
            "error": format!("provider runtime not registered: {}", provider_name),
        })
        .to_string());
    }

    let mut candidate_matches = Vec::with_capacity(shared_candidates.len());
    for (index, shared) in &shared_candidates {
        let settings = shared.settings.read().await.clone();
        let explicit_channel_match = settings.allowed_channel_ids.contains(&channel_id.get());
        let live_channel_match = match shared.cached_serenity_ctx.get() {
            Some(ctx) => {
                super::provider_handles_channel(ctx, owner_provider, &settings, channel_id).await
            }
            None => false,
        };
        candidate_matches.push(DirectMeetingRuntimeCandidate {
            index: *index,
            explicit_channel_match,
            live_channel_match,
        });
    }

    if let Some(selected_index) =
        select_direct_meeting_runtime_candidate(provider_name, channel_id, &candidate_matches)?
    {
        let (_, shared) = shared_candidates
            .iter()
            .find(|(index, _)| *index == selected_index)
            .cloned()
            .ok_or_else(|| {
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "selected runtime index vanished for provider {} on channel {}",
                        provider_name,
                        channel_id.get()
                    ),
                })
                .to_string()
            })?;
        let http = shared
            .cached_serenity_ctx
            .get()
            .map(|ctx| ctx.http.clone())
            .ok_or_else(|| {
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "matched runtime is not ready for provider {} on channel {}",
                        provider_name,
                        channel_id.get()
                    ),
                })
                .to_string()
            })?;
        return Ok((http, shared));
    }

    if shared_candidates.len() == 1 {
        let (_, shared) = shared_candidates[0].clone();
        if let Some(ctx) = shared.cached_serenity_ctx.get() {
            return Ok((ctx.http.clone(), shared));
        }
        let http = resolve_bot_http(registry, provider_name)
            .await
            .map_err(|(_, body)| body)?;
        return Ok((http, shared));
    }

    Err(serde_json::json!({
        "ok": false,
        "error": format!(
            "could not resolve a unique runtime for provider {} on channel {}",
            provider_name,
            channel_id.get()
        ),
    })
    .to_string())
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
    let (http, shared) =
        resolve_direct_meeting_runtime(registry, channel_id, &owner_provider).await?;

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
pub async fn send_message(
    registry: &HealthRegistry,
    db: &Db,
    target: &str,
    content: &str,
    source: &str,
    bot: &str,
) -> (&'static str, String) {
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
    if !INTERNAL_SOURCES.contains(&source) && !super::settings::is_known_agent(source) {
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
            tracing::info!("  [{ts}] {emoji} ROUTE: [{source}] → channel {channel_id} (bot={bot})");
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
            tracing::warn!("  [{ts}] ⚠ ROUTE: failed to send to channel {channel_id}: {e}");
            (
                "500 Internal Server Error",
                format!(r#"{{"ok":false,"error":"Discord send failed: {}"}}"#, e),
            )
        }
    }
}

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

    send_message(registry, db, target, content, source, bot).await
}

/// Handle POST /api/senddm — send a DM to a Discord user.
/// Accepts JSON:
/// {"user_id":"...", "content":"...", "bot":"announce|notify|claude|codex"}
pub async fn handle_senddm(registry: &HealthRegistry, body: &str) -> (&'static str, String) {
    let request = match parse_senddm_body(body) {
        Ok(request) => request,
        Err(error) => {
            return (
                "400 Bad Request",
                serde_json::json!({"ok": false, "error": error}).to_string(),
            );
        }
    };

    let http = match resolve_bot_http(registry, &request.bot).await {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    let user_id_text = request.user_id.to_string();

    use poise::serenity_prelude::{CreateMessage, UserId};
    let user_id = UserId::new(request.user_id);
    match user_id.create_dm_channel(&*http).await {
        Ok(dm_channel) => {
            match dm_channel
                .id
                .send_message(&*http, CreateMessage::new().content(&request.content))
                .await
            {
                Ok(_) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!("  [{ts}] 📨 DM: → user {}", request.user_id);
                    (
                        "200 OK",
                        serde_json::json!({
                            "ok": true,
                            "user_id": user_id_text,
                        })
                        .to_string(),
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

#[derive(Debug, PartialEq)]
struct SendDmRequest {
    user_id: u64,
    content: String,
    bot: String,
}

fn parse_senddm_body(body: &str) -> Result<SendDmRequest, String> {
    let parsed: serde_json::Value = serde_json::from_str(body).map_err(|_| "invalid JSON")?;
    let user_id = parsed["user_id"]
        .as_str()
        .and_then(|value| value.parse().ok())
        .or_else(|| parsed["user_id"].as_u64())
        .ok_or("user_id required (string or number)")?;
    if user_id == 0 {
        return Err("user_id required (string or number)".to_string());
    }

    let content = parsed["content"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or("content required")?
        .to_string();
    let bot = parsed["bot"].as_str().unwrap_or("announce").to_string();

    Ok(SendDmRequest {
        user_id,
        content,
        bot,
    })
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
                        tracing::warn!(
                            "  [{ts}] 🩺 watchdog: health recovered after {consecutive_failures} failure(s)"
                        );
                    }
                    consecutive_failures = 0;
                } else {
                    consecutive_failures += 1;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] 🩺 watchdog: health check failed ({consecutive_failures}/{MAX_FAILURES})"
                    );
                    if consecutive_failures >= MAX_FAILURES {
                        tracing::warn!(
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
                            Ok(()) => tracing::warn!(
                                "  [{ts}] 🩺 watchdog: dump saved to {dump_path} — forcing exit"
                            ),
                            Err(e) => tracing::warn!(
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
    fn test_parse_senddm_body_without_reply_tracking() {
        let body = r#"{"user_id":"123","content":"hello","bot":"claude"}"#;
        let parsed = parse_senddm_body(body).expect("senddm body should parse");
        assert_eq!(
            parsed,
            SendDmRequest {
                user_id: 123,
                content: "hello".to_string(),
                bot: "claude".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_senddm_body_ignores_reply_tracking_fields() {
        let body = r#"{
            "user_id":"123",
            "content":"건강검진 요즘 했어?",
            "bot":"claude",
            "source_agent":"family-counsel",
            "channel_id":"1473922824350601297",
            "ttl_seconds":86400,
            "context":{"topicKey":"obujang.health_checkup","targetKey":"obujang"}
        }"#;
        let parsed = parse_senddm_body(body).expect("senddm body should parse");
        assert_eq!(parsed.user_id, 123);
        assert_eq!(parsed.content, "건강검진 요즘 했어?");
        assert_eq!(parsed.bot, "claude");
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
    async fn hard_stop_runtime_turn_uses_mailbox_canonical_cleanup_when_runtime_online() {
        let harness = TestHealthHarness::new().await;
        let channel_id = 123_456_789_012_345_678;
        harness
            .seed_channel_session(channel_id, "hard-stop-runtime", Some("session-live"))
            .await;
        harness.seed_active_turn(channel_id, 99, 101).await;
        harness
            .seed_queue(channel_id, &[(7001, "preserve me")])
            .await;
        harness.insert_dispatch_role_override(channel_id, 987_654_321_098_765_432);

        let registry = harness.registry();
        let result = hard_stop_runtime_turn(
            Some(registry.as_ref()),
            Some("claude"),
            Some(channel_id),
            None,
            "test hard stop",
        )
        .await;

        assert_eq!(result.cleanup_path, "mailbox_canonical");
        assert!(result.had_active_turn);
        assert!(result.has_pending_queue);
        assert!(result.runtime_session_cleared);

        let (has_active_turn, queue_depth, session_id) = harness.mailbox_state(channel_id).await;
        assert!(!has_active_turn);
        assert_eq!(queue_depth, 1);
        assert_eq!(session_id, None);
        assert!(harness.has_dispatch_role_override(channel_id));
    }

    #[tokio::test]
    async fn hard_stop_runtime_turn_removes_dispatch_override_when_queue_is_empty() {
        let harness = TestHealthHarness::new().await;
        let channel_id = 223_456_789_012_345_678;
        harness
            .seed_channel_session(channel_id, "hard-stop-empty", Some("session-empty"))
            .await;
        harness.seed_active_turn(channel_id, 77, 88).await;
        harness.insert_dispatch_role_override(channel_id, 887_654_321_098_765_432);

        let registry = harness.registry();
        let result = hard_stop_runtime_turn(
            Some(registry.as_ref()),
            Some("claude"),
            Some(channel_id),
            None,
            "test hard stop",
        )
        .await;

        assert_eq!(result.cleanup_path, "mailbox_canonical");
        assert!(result.had_active_turn);
        assert!(!result.has_pending_queue);
        assert!(result.runtime_session_cleared);

        let (has_active_turn, queue_depth, session_id) = harness.mailbox_state(channel_id).await;
        assert!(!has_active_turn);
        assert_eq!(queue_depth, 0);
        assert_eq!(session_id, None);
        assert!(!harness.has_dispatch_role_override(channel_id));
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

    #[tokio::test]
    async fn runtime_stop_fallback_preserves_mailbox_queue() {
        let harness = TestHealthHarness::new().await;
        let channel_id = 777_000_000_000_000_001u64;
        harness
            .set_queue_depth_for_channel(channel_id, ProviderKind::Claude, 2)
            .await;
        harness
            .start_active_turn(channel_id, 7, 70, Some("missing-runtime-stop"))
            .await;

        let result = stop_provider_channel_runtime(
            harness.registry().as_ref(),
            "claude",
            ChannelId::new(channel_id),
            "test runtime fallback",
        )
        .await
        .expect("runtime stop should resolve provider");

        assert_eq!(result.lifecycle_path, "runtime-fallback");
        assert_eq!(result.queue_depth, 2);
        assert_eq!(harness.queue_depth_for_channel(channel_id).await, 2);
        assert!(
            !harness
                .shared()
                .mailbox(ChannelId::new(channel_id))
                .has_active_turn()
                .await,
            "fallback cleanup should clear the active turn",
        );
    }

    #[tokio::test]
    async fn resolve_bot_http_reports_missing_notify_bot_token() {
        let harness = TestHealthHarness::new().await;

        let err = resolve_bot_http(harness.registry().as_ref(), "notify")
            .await
            .unwrap_err();

        assert_eq!(err.0, "503 Service Unavailable");
        assert!(err.1.contains("notify bot not configured"));
    }

    #[test]
    fn select_direct_meeting_runtime_candidate_prefers_explicit_channel_match() {
        let selected = select_direct_meeting_runtime_candidate(
            "claude",
            ChannelId::new(123),
            &[
                DirectMeetingRuntimeCandidate {
                    index: 0,
                    explicit_channel_match: false,
                    live_channel_match: true,
                },
                DirectMeetingRuntimeCandidate {
                    index: 1,
                    explicit_channel_match: true,
                    live_channel_match: true,
                },
            ],
        )
        .expect("selection should succeed");

        assert_eq!(selected, Some(1));
    }

    #[test]
    fn select_direct_meeting_runtime_candidate_rejects_ambiguous_explicit_matches() {
        let err = select_direct_meeting_runtime_candidate(
            "claude",
            ChannelId::new(123),
            &[
                DirectMeetingRuntimeCandidate {
                    index: 0,
                    explicit_channel_match: true,
                    live_channel_match: true,
                },
                DirectMeetingRuntimeCandidate {
                    index: 1,
                    explicit_channel_match: true,
                    live_channel_match: false,
                },
            ],
        )
        .expect_err("ambiguous explicit matches must fail");

        assert!(err.contains("multiple runtimes explicitly allow channel 123"));
    }
}
