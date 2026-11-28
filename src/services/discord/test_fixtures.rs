//! Discord runtime test fixtures shared by the crate's tests.

#[cfg(unix)]
use super::tmux;
use super::{Arc, ChannelId, SharedData, TmuxWatcherHandle, tmux_watcher_now_ms};
#[cfg(unix)]
use super::{ProviderKind, queue_dispatch, serenity, shared_state};

#[cfg(all(test, unix))]
pub(crate) fn claim_cross_channel_tmux_watcher_for_high_risk_test(
    requested_channel_id: ChannelId,
    existing_channel_id: ChannelId,
    thread_parent_channel_id: Option<ChannelId>,
) {
    tmux::claim_cross_channel_tmux_watcher_for_test(
        requested_channel_id,
        existing_channel_id,
        thread_parent_channel_id,
    );
}

#[cfg(all(test, unix))]
pub(crate) fn input_mailbox_for_test(
    shared: &SharedData,
    channel: u64,
) -> crate::services::turn_orchestrator::ChannelMailboxHandle {
    shared.mailbox(ChannelId::new(channel))
}

#[cfg(unix)]
pub(in crate::services::discord) const BOT_ID: u64 = 940_487_400_000_003;

#[cfg(unix)]
pub(in crate::services::discord) fn discord_user_json(
    id: u64,
    name: &str,
    bot: bool,
) -> serde_json::Value {
    serde_json::json!({
        "id": id.to_string(),
        "username": name,
        "discriminator": "0",
        "global_name": null,
        "avatar": null,
        "bot": bot,
        "system": false,
        "mfa_enabled": false,
        "banner": null,
        "accent_color": null,
        "locale": null,
        "verified": null,
        "email": null,
        "flags": 0,
        "premium_type": 0,
        "public_flags": 0,
        "member": null,
        "primary_guild": null,
        "avatar_decoration_data": null,
        "collectibles": null
    })
}

#[cfg(unix)]
struct NoopFramework;

#[cfg(unix)]
#[async_trait::async_trait]
impl serenity::framework::Framework for NoopFramework {
    async fn dispatch(&self, _ctx: serenity::Context, _event: serenity::FullEvent) {}
}

/// A real context whose REST calls go through `http` and whose shard dials `gateway_url`.
#[cfg(all(test, unix))]
pub(crate) async fn gateway_context_for_test(
    http: Arc<serenity::Http>,
    gateway_url: String,
) -> serenity::Context {
    let cache = Arc::new(serenity::cache::Cache::new());
    let mut ready: serenity::ReadyEvent = serde_json::from_value(serde_json::json!({
        "v": 10,
        "user": discord_user_json(BOT_ID, "queue-bot", true),
        "guilds": [],
        "session_id": "queue-wake-e2e",
        "resume_gateway_url": gateway_url,
        "shard": [0, 1],
        "application": {"id": BOT_ID.to_string(), "flags": 0}
    }))
    .expect("ready fixture");
    cache.update(&mut ready);

    let data = Arc::new(tokio::sync::RwLock::new(serenity::prelude::TypeMap::new()));
    let ws_url = Arc::new(tokio::sync::Mutex::new(gateway_url));
    let framework: Arc<dyn serenity::framework::Framework> = Arc::new(NoopFramework);
    let (manager, _manager_result) =
        serenity::gateway::ShardManager::new(serenity::gateway::ShardManagerOptions {
            data: data.clone(),
            event_handlers: vec![],
            raw_event_handlers: vec![],
            framework: Arc::new(std::sync::OnceLock::from(framework)),
            shard_index: 0,
            shard_init: 0,
            shard_total: 1,
            ws_url: ws_url.clone(),
            cache: cache.clone(),
            http: http.clone(),
            intents: serenity::GatewayIntents::DIRECT_MESSAGES
                | serenity::GatewayIntents::MESSAGE_CONTENT,
            presence: None,
        });
    let shard = serenity::gateway::Shard::new(
        ws_url,
        "test-token",
        serenity::model::gateway::ShardInfo {
            id: serenity::ShardId(0),
            total: 1,
        },
        serenity::GatewayIntents::DIRECT_MESSAGES | serenity::GatewayIntents::MESSAGE_CONTENT,
        None,
    )
    .await
    .expect("test shard");
    let runner = serenity::gateway::ShardRunner::new(serenity::gateway::ShardRunnerOptions {
        data: data.clone(),
        event_handlers: vec![],
        raw_event_handlers: vec![],
        framework: Some(Arc::new(NoopFramework)),
        manager,
        shard,
        cache: cache.clone(),
        http: http.clone(),
    });

    serenity::Context {
        data,
        shard: serenity::ShardMessenger::new(&runner),
        shard_id: serenity::ShardId(0),
        http,
        cache,
    }
}

#[cfg(all(test, unix))]
pub(crate) fn ready_gateway_for_test(shared: &SharedData, context: serenity::Context) {
    shared.http.cached_serenity_ctx.set(context).unwrap();
    shared
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
}

#[cfg(all(test, unix))]
pub(crate) struct RestGuardForTest(pub(super) Option<shared_state::test_rest::Guard>);

#[cfg(all(test, unix))]
impl Drop for RestGuardForTest {
    fn drop(&mut self) {
        drop(self.0.take());
    }
}

#[cfg(all(test, unix))]
pub(crate) fn rest_for_test(http: Arc<serenity::Http>) -> RestGuardForTest {
    RestGuardForTest(Some(shared_state::test_rest::install(http)))
}

#[cfg(all(test, unix))]
pub(crate) fn input_queue_path_for_test(shared: &SharedData, channel: u64) -> std::path::PathBuf {
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(channel);
    let context = queue_dispatch::persistence_context(shared, &provider, channel);
    crate::services::turn_orchestrator::v2_queue(&context, channel)
        .unwrap()
        .0
}

#[cfg(test)]
pub(crate) fn register_resume_watcher_for_tests(
    shared: &SharedData,
    channel_id: ChannelId,
    tmux_session_name: &str,
) {
    shared.tmux_watchers.insert(
        channel_id,
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: format!("/runtime/{tmux_session_name}.jsonl"),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(
                std::sync::atomic::AtomicI64::new(tmux_watcher_now_ms()),
            ),
        },
    );
}

#[cfg(test)]
pub(crate) fn resume_owner_channel_for_tests(
    shared: &SharedData,
    tmux_session_name: &str,
) -> Option<ChannelId> {
    shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session_name)
}

#[cfg(all(test, unix))]
pub(crate) async fn revoke_notice_author_for_test(shared: &SharedData) {
    shared.settings.write().await.allowed_user_ids.clear();
}

// Keep the parent fixture path available to tests on every platform.
#[cfg(not(unix))]
const _: fn(&SharedData, &str) -> Option<ChannelId> = super::resume_owner_channel_for_tests;

/// Moves a file's mtime into the past so any rewrite is visible as an mtime change.
pub(in crate::services::discord) fn age_file(path: &std::path::Path, secs: i64) {
    let at = chrono::Utc::now().timestamp() - secs;
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(at, 0)).unwrap();
}

/// Bytes and mtime of a durable record; `None` when it does not exist.
pub(in crate::services::discord) fn fingerprint(
    path: &std::path::Path,
) -> Option<(Vec<u8>, std::time::SystemTime)> {
    let bytes = std::fs::read(path).ok()?;
    Some((bytes, std::fs::metadata(path).ok()?.modified().ok()?))
}

pub(in crate::services::discord) struct TestOverrideGuard<T: Copy + 'static> {
    cell: &'static std::sync::Mutex<Option<T>>,
    previous: Option<T>,
}

impl<T: Copy + 'static> TestOverrideGuard<T> {
    pub(in crate::services::discord) fn current(cell: &std::sync::Mutex<Option<T>>) -> Option<T> {
        *cell.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    pub(in crate::services::discord) fn replace(
        cell: &'static std::sync::Mutex<Option<T>>,
        value: T,
    ) -> Self {
        let previous = cell
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .replace(value);
        Self { cell, previous }
    }
}

impl<T: Copy + 'static> Drop for TestOverrideGuard<T> {
    fn drop(&mut self) {
        *self
            .cell
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = self.previous.take();
    }
}
