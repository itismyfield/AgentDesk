//! Shared fixture for the in-process Discord relay e2e scenarios.
//!
//! A scenario boots [`RelayE2eHarness`], injects through a production entry
//! point — Discord `FullEvent` intake or a tmux-direct prompt observation — and
//! reads mailbox, durable queue, dispatch witnesses, relay leases and turn
//! completion edges back out.
//!
//! Every wait here is state- or event-driven under a deadline. Scenarios run on
//! a `multi_thread` runtime, where `tokio`'s clock cannot be paused, so a fixed
//! sleep is wall-clock flake. Scenarios must not add one.
//!
//! CI pins these to `env -u AGENTDESK_ROOT_DIR ... -- --test-threads=1`; a new
//! scenario module inherits that only once it is named in the same invocation.

mod catch_up_pagination_e2e;
mod discord_mock;
#[path = "prompt_identity_e2e_tests.rs"]
mod prompt_identity_e2e;
mod registered_bootstrap_e2e;
mod stale_resume_retry_e2e;
#[cfg(unix)]
mod thread_guard_host_e2e;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::future::BoxFuture;
use poise::serenity_prelude as serenity;
use serenity::ChannelId;
use tokio::sync::broadcast::Receiver;

use crate::services::discord::turn_completion_events::{
    TurnCompletionEvent, subscribe_turn_completion_events,
};
use crate::services::discord::{
    ChannelMailboxSnapshot, Data, Error, ProviderKind, SharedData, TmuxWatcherHandle, inflight,
    mailbox_snapshot, router,
};
use crate::services::tui_prompt_dedupe as dedupe;
use crate::services::turn_orchestrator as orchestrator;

pub(super) use discord_mock::CHANNEL_ID;
use discord_mock::{HistoryQuery, USER_ID, history_message_json, user_message};

/// Dedupe and lease tables key on the provider's wire name, not [`ProviderKind`].
pub(super) const PROVIDER_KEY: &str = "claude";
const SESSION_UUID: &str = "48740000-0000-0000-0000-000000000001";

pub(super) struct AbortOnDrop<T>(Option<tokio::task::JoinHandle<T>>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

/// Polls `predicate` until it holds or `timeout` expires; reports whether it held.
pub(super) async fn wait_until(
    timeout: Duration,
    mut predicate: impl FnMut() -> BoxFuture<'static, bool>,
) -> bool {
    tokio::time::timeout(timeout, async {
        loop {
            if predicate().await {
                return;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

fn watcher_handle(tmux_session_name: &str, output_path: &std::path::Path) -> TmuxWatcherHandle {
    TmuxWatcherHandle {
        tmux_session_name: tmux_session_name.to_string(),
        output_path: output_path.display().to_string(),
        paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
            crate::services::discord::tmux_watcher_now_ms(),
        )),
    }
}

/// What the `claude` stand-in answers.
#[derive(Clone, Copy)]
pub(super) enum ProviderStub {
    /// Every turn succeeds at once on the bound session.
    Success,
    /// A `--resume` launch is rejected as a stale session, as a real CLI rejects the
    /// synthetic id; a fresh launch succeeds.
    StaleResumeThenSuccess,
}

/// One line per provider launch the stand-in served, `--version` probes aside.
const PROVIDER_STARTS_FILE: &str = "claude-stub-starts";

fn write_provider_stub(root: &std::path::Path, stub: ProviderStub) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = root.join("claude-stub");
    let stale = match stub {
        ProviderStub::Success => "",
        ProviderStub::StaleResumeThenSuccess => {
            "case \"$*\" in *--resume*) echo '{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true}'\n\
             echo 'No conversation found with session ID' >&2; exit 1;; esac\n"
        }
    };
    let starts = root.join(PROVIDER_STARTS_FILE);
    let starts = starts.display();
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '0.0.0 (stub)'; exit 0; fi\n\
         echo start >> '{starts}'\ncat >/dev/null\n{stale}\
         echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{SESSION_UUID}\"}}'\n\
         echo '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"ok\",\"session_id\":\"{SESSION_UUID}\"}}'\n"
    );
    std::fs::write(&path, script).expect("write provider stub");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod provider stub");
    path
}

/// An isolated AgentDesk root, a mock Discord transport, a real
/// `serenity::Context` over it, and a channel already bound to a session.
///
/// Field order is drop order and is load-bearing: the mock server and the
/// workers holding `shared` must stop before the guards unset
/// `AGENTDESK_ROOT_DIR` and before `root` deletes the tree they write into.
pub(super) struct RelayE2eHarness {
    pub(super) data: Data,
    pub(super) shared: Arc<SharedData>,
    pub(super) ctx: serenity::Context,
    pub(super) channel_id: ChannelId,
    mock: discord_mock::DiscordMockState,
    proxy: String,
    /// Only set by [`Self::start_with_health_registry`]; `shared` holds it weakly.
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    _server: AbortOnDrop<()>,
    _dedupe_guard: std::sync::MutexGuard<'static, ()>,
    _intake_guard: crate::config::TestEnvVarGuard,
    _provider_guard: crate::config::TestEnvVarGuard,
    _config_guard: crate::config::TestEnvVarGuard,
    _root_guard: crate::config::TestEnvVarGuard,
    _env_lock: std::sync::MutexGuard<'static, ()>,
    root: tempfile::TempDir,
}

impl Drop for RelayE2eHarness {
    /// A synthetic start left pending in this root would block the next harness's kickoff.
    fn drop(&mut self) {
        let pending = crate::services::discord::tui_direct_pending_start::load_all();
        for record in pending
            .iter()
            .filter(|record| record.channel_id == CHANNEL_ID)
        {
            crate::services::discord::tui_direct_pending_start::delete(record);
        }
    }
}

impl RelayE2eHarness {
    pub(super) async fn start() -> Self {
        Self::start_with_provider(ProviderStub::Success).await
    }

    pub(super) async fn start_with_provider(stub: ProviderStub) -> Self {
        Self::start_inner(stub, false, std::future::ready(None)).await
    }

    /// A runtime on the pool `storage` opens once the env lock is held; no session is bound.
    pub(super) async fn start_unbound_on(
        storage: impl std::future::Future<Output = sqlx::PgPool>,
    ) -> Self {
        let storage = async { Some(storage.await) };
        Self::start_inner(ProviderStub::Success, false, storage).await
    }

    /// Adds a health registry, the source of the utility bots SSH-direct
    /// announcements post through.
    pub(super) async fn start_with_health_registry() -> Self {
        Self::start_inner(ProviderStub::Success, true, std::future::ready(None)).await
    }

    async fn start_inner(
        stub: ProviderStub,
        with_health_registry: bool,
        storage: impl std::future::Future<Output = Option<sqlx::PgPool>>,
    ) -> Self {
        let env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("isolated AgentDesk root");
        let root_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let intake_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "ADK_INTAKE_ROUTING_MODE",
            std::path::Path::new("disabled"),
        );
        // Dispatched turns must not reach a host `claude` or host config: a real
        // CLI rejects the synthetic resume id and triggers a stale-resume re-dispatch.
        let provider_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CLAUDE_PATH",
            &write_provider_stub(root.path(), stub),
        );
        let config_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &root.path().join("config").join("agentdesk.yaml"),
        );
        let dedupe_guard = dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        let mock = discord_mock::DiscordMockState::new();
        let (proxy, gateway_url, server) = discord_mock::start(mock.clone()).await;
        let ctx = discord_mock::serenity_context(proxy.clone(), gateway_url).await;
        // Storage opens after the shared test-env lock, the canonical lock order.
        let pool = storage.await;
        let unbound = pool.is_some();
        let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(pool);
        let health_registry = with_health_registry.then(|| {
            let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
            Arc::get_mut(&mut shared)
                .expect("fresh shared")
                .health_registry = Arc::downgrade(&registry);
            registry
        });
        {
            let mut settings = shared.settings.write().await;
            settings.owner_user_id = Some(USER_ID);
            settings.allow_all_users = true;
        }
        let voice_config = crate::voice::VoiceConfig::default();
        let data = Data {
            shared: shared.clone(),
            token: "test-token".to_string(),
            provider: ProviderKind::Claude,
            voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
            voice_config,
        };
        let channel_id = ChannelId::new(CHANNEL_ID);
        let cwd = root.path().to_str().expect("utf8 test root").to_string();
        if !unbound {
            let rebind = crate::services::discord::rebind_channel_session;
            rebind(
                &shared,
                &ProviderKind::Claude,
                channel_id,
                &cwd,
                SESSION_UUID,
            )
            .await;
        }

        Self {
            data,
            shared,
            ctx,
            channel_id,
            mock,
            proxy,
            health_registry,
            _server: AbortOnDrop(Some(server)),
            _dedupe_guard: dedupe_guard,
            _intake_guard: intake_guard,
            _provider_guard: provider_guard,
            _config_guard: config_guard,
            _root_guard: root_guard,
            _env_lock: env_lock,
            root,
        }
    }

    /// `Data` is not `Clone`; production handlers take it by reference per task.
    fn clone_data(&self) -> Data {
        Data {
            shared: self.data.shared.clone(),
            token: self.data.token.clone(),
            provider: self.data.provider.clone(),
            voice_config: self.data.voice_config.clone(),
            voice_receiver: self.data.voice_receiver.clone(),
        }
    }

    /// The Discord-API injection path: production `FullEvent` intake, run to completion.
    pub(super) async fn deliver_user_message(&self, id: u64, text: &str) -> Result<(), Error> {
        let event = serenity::FullEvent::Message {
            new_message: user_message(id, text),
        };
        router::handle_event(&self.ctx, &event, &self.data).await
    }

    /// Spawns production intake for `id` and returns once the mock has its
    /// placeholder POST parked, leaving the mailbox occupied until
    /// [`Self::release_held_placeholder`]. The guard aborts the turn on drop.
    pub(super) async fn spawn_turn_held_at_placeholder(
        &self,
        id: u64,
        text: &str,
        timeout: Duration,
    ) -> AbortOnDrop<Result<(), Error>> {
        let event = serenity::FullEvent::Message {
            new_message: user_message(id, text),
        };
        // Register the waiter before the spawn that dispatches the POST:
        // `notify_waiters` leaves no permit, so a child that outruns the parent
        // would otherwise strand this wait until its deadline.
        let arrived = self.mock.first_placeholder_arrived.notified();
        tokio::pin!(arrived);
        arrived.as_mut().enable();
        let mut task = tokio::spawn({
            let ctx = self.ctx.clone();
            let data = self.clone_data();
            async move { router::handle_event(&ctx, &event, &data).await }
        });
        tokio::select! {
            _ = &mut arrived => {}
            result = &mut task => {
                let snapshot = self.mailbox().await;
                let checkpoint = self.checkpoint();
                panic!(
                    "turn {id} exited before the placeholder POST: {result:?}; current_bot={}; active={:?}; queue_len={}; checkpoint={checkpoint:?}",
                    self.ctx.cache.current_user().id,
                    snapshot.active_user_message_id,
                    snapshot.intervention_queue.len()
                )
            }
            _ = tokio::time::sleep(timeout) => {
                panic!("turn {id} did not reach the real placeholder POST")
            }
        }
        AbortOnDrop(Some(task))
    }

    /// Releases the parked placeholder POST as a 500, driving the production
    /// placeholder-failure recovery path.
    pub(super) fn release_held_placeholder(&self) {
        self.mock.release_first_placeholder.notify_waiters();
    }

    pub(super) async fn mailbox(&self) -> ChannelMailboxSnapshot {
        mailbox_snapshot(&self.shared, self.channel_id).await
    }

    /// Queue rows that survive a restart, read straight from durable storage.
    pub(super) fn durable_queue(&self) -> Vec<orchestrator::Intervention> {
        orchestrator::load_channel_pending_queue_for_tests(
            &self.data.provider,
            &self.shared.token_hash,
            self.channel_id,
        )
        .0
    }

    /// The channel's catch-up checkpoint. A queued message that is checkpointed
    /// away instead of dispatched is the #5997 queue-reclaim failure shape.
    pub(super) fn checkpoint(&self) -> Option<u64> {
        self.shared
            .last_message_ids
            .get(&self.channel_id)
            .map(|entry| *entry)
    }

    pub(super) fn subscribe_completions(&self) -> Receiver<TurnCompletionEvent> {
        subscribe_turn_completion_events(&self.shared)
    }

    /// Provider launches the `claude` stand-in has served so far.
    pub(super) fn provider_starts(&self) -> usize {
        let starts = self.root.path().join(PROVIDER_STARTS_FILE);
        std::fs::read_to_string(starts).map_or(0, |starts| starts.lines().count())
    }

    /// Placeholder POSTs seen by the mock: the harness' dispatch witness.
    pub(super) fn placeholder_posts(&self) -> usize {
        self.mock.placeholder_posts.load(Ordering::SeqCst)
    }

    /// Non-placeholder POSTs, which carry local-only control notes.
    pub(super) fn local_note_posts(&self) -> usize {
        self.mock.local_note_posts.load(Ordering::SeqCst)
    }

    /// Every message the mock minted, oldest first, as `(reply_to, latest content)`.
    pub(super) fn messages(&self) -> Vec<(Option<u64>, String)> {
        let messages = self.mock.messages.lock().expect("mock messages");
        messages.values().cloned().collect()
    }

    /// Requests the mock could not answer. A non-empty list means production
    /// took a failure path the scenario never asserted on.
    pub(super) fn unhandled_requests(&self) -> Vec<String> {
        self.mock.unhandled.lock().expect("unhandled log").clone()
    }

    /// Seeds the history `catch_up` reads, in any order, as
    /// `(message_id, content, is_bot)`.
    pub(super) fn seed_channel_history(&self, entries: &[(u64, &str, bool)]) {
        *self.mock.history.lock().expect("mock history") = entries
            .iter()
            .map(|(id, content, bot)| history_message_json(*id, content, *bot))
            .collect();
    }

    /// Every `GET /messages` query the mock answered, in arrival order.
    pub(super) fn history_queries(&self) -> Vec<HistoryQuery> {
        self.mock
            .history_queries
            .lock()
            .expect("history queries")
            .clone()
    }

    /// Registers the channel in the role map with no checkpoint, which is what
    /// makes `catch_up` scan it in `Recent` mode.
    pub(super) fn register_channel_in_role_map(&self) {
        let path = crate::runtime_layout::role_map_path(self.root.path());
        std::fs::create_dir_all(path.parent().expect("role map dir")).expect("role map dir");
        let role_map = serde_json::json!({
            "byChannelId": {
                CHANNEL_ID.to_string(): {"roleId": "adk-cc", "promptFile": "prompt.md", "provider": PROVIDER_KEY}
            }
        });
        std::fs::write(path, role_map.to_string()).expect("write role map");
    }

    /// One production catch-up sweep, both phases, over the mock transport.
    pub(super) async fn run_catch_up(&self) {
        crate::services::discord::catch_up::catch_up_missed_messages(
            &self.ctx.http,
            &self.shared,
            &self.data.provider,
        )
        .await;
    }

    /// Level-triggered: safe to call after the POST has already landed.
    pub(super) async fn wait_for_placeholder_posts(&self, count: usize, timeout: Duration) -> bool {
        let posts = self.mock.placeholder_posts.clone();
        wait_until(timeout, move || {
            let posts = posts.clone();
            Box::pin(async move { posts.load(Ordering::SeqCst) >= count })
        })
        .await
    }

    /// Publishes this fixture's context and token on the shared HTTP cache, which
    /// is how relay workers reach Discord without a live gateway.
    pub(super) fn cache_relay_transport(&self) {
        self.shared
            .http
            .cached_serenity_ctx
            .set(self.ctx.clone())
            .expect("cache test Serenity context");
        self.shared
            .http
            .cached_bot_token
            .set(self.data.token.clone())
            .expect("cache test bot token");
    }

    /// Answers the first `...` at once instead of parking it.
    pub(super) fn answer_placeholders_immediately(&self) {
        self.mock
            .park_first_placeholder
            .store(false, Ordering::SeqCst);
    }

    pub(super) fn answer_notes_with(&self, answer: discord_mock::NoteAnswer) {
        *self.mock.note_answer.lock().expect("note answer") = answer;
    }

    /// Holds the next non-placeholder POST open until [`Self::release_held_note`].
    pub(super) fn hold_next_note(&self) {
        self.mock.hold_next_note.store(true, Ordering::SeqCst);
    }

    pub(super) async fn wait_for_held_note(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.mock.note_held.notified())
            .await
            .is_ok()
    }

    pub(super) fn release_held_note(&self) {
        self.mock.release_held_note.notify_one();
    }

    /// Points the notify bot at the mock; `timeout` bounds each of its requests.
    pub(super) async fn use_mock_notify_bot(&self, timeout: Duration) {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("notify bot client");
        let http = serenity::HttpBuilder::new("notify-test-token")
            .client(client)
            .proxy(self.proxy.clone())
            .ratelimiter_disabled(true)
            .build();
        self.health_registry
            .as_ref()
            .expect("started with a health registry")
            .set_utility_bot_http_for_tests(
                crate::services::discord::bot_role::UtilityBotRole::Notify,
                Arc::new(http),
            )
            .await;
    }

    /// Registers a watcher over a fresh empty transcript in the isolated root,
    /// as attaching to a live TUI session would.
    pub(super) fn attach_tmux_watcher(&self, tmux: &str, transcript_name: &str) -> PathBuf {
        let transcript_path = self.root.path().join(transcript_name);
        std::fs::write(&transcript_path, "").expect("write watcher transcript");
        self.shared
            .tmux_watchers
            .insert(self.channel_id, watcher_handle(tmux, &transcript_path));
        transcript_path
    }

    pub(super) fn spawn_relay_worker(&self) {
        super::spawn_tui_prompt_relay(self.shared.clone(), self.data.provider.clone());
    }

    /// The tmux-direct injection path: a prompt observed on the TUI rather than
    /// arriving through Discord intake.
    pub(super) fn observe_tui_prompt(&self, tmux: &str, text: &str) -> dedupe::PromptObservation {
        dedupe::observe_prompt_by_provider_session(PROVIDER_KEY, tmux, text)
    }

    pub(super) fn relay_lease_present(&self, tmux: &str) -> bool {
        dedupe::external_input_relay_lease_present(PROVIDER_KEY, tmux, CHANNEL_ID)
    }

    pub(super) fn ssh_direct_observation_pending(&self, tmux: &str) -> bool {
        dedupe::is_ssh_direct_observation_pending(PROVIDER_KEY, tmux)
    }

    pub(super) fn prompt_anchor(&self, tmux: &str) -> Option<dedupe::TuiPromptAnchor> {
        dedupe::prompt_anchor_for_response(PROVIDER_KEY, tmux, CHANNEL_ID)
    }

    /// Whether persisted inflight state claims synthetic TUI-direct ownership of
    /// `tmux`. `InflightTurnState` is not nameable outside `discord::inflight`,
    /// so the fixture exposes the predicate rather than the value.
    pub(super) fn synthetic_inflight_matches(&self, tmux: &str, generation: u64) -> bool {
        let state = inflight::load_inflight_state_read_only(&self.data.provider, CHANNEL_ID);
        super::tui_direct_watcher_synthetic_inflight_matches(state.as_ref(), tmux, generation)
    }
}

mod headless_turn_provider_tests {
    use super::*;
    use crate::services::discord::{mailbox_try_start_turn, make_shared_data_for_tests};
    use crate::services::provider::CancelToken;
    use poise::serenity_prelude::{MessageId, UserId};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn headless_turn_setup_failure_releases_only_its_slot_and_active_count() {
        use crate::services::discord::{health, increment_global_active};
        let harness = RelayE2eHarness::start().await;
        harness.register_channel_in_role_map();
        let shared = make_shared_data_for_tests();
        let mut other = make_shared_data_for_tests();
        Arc::get_mut(&mut other).unwrap().provider = ProviderKind::Codex;
        other.settings.write().await.provider = ProviderKind::Codex;
        shared
            .http
            .cached_serenity_ctx
            .set(harness.ctx.clone())
            .ok()
            .unwrap();
        shared
            .http
            .cached_bot_token
            .set("test-token".into())
            .ok()
            .unwrap();
        let registry = health::HealthRegistry::new();
        registry.register("codex".into(), other.clone()).await;
        registry.register("claude".into(), shared.clone()).await;
        let unrelated_channel = ChannelId::new(CHANNEL_ID + 1);
        let unrelated = Arc::new(CancelToken::new());
        assert!(
            mailbox_try_start_turn(
                &shared,
                unrelated_channel,
                unrelated.clone(),
                UserId::new(1),
                MessageId::new(901)
            )
            .await
        );
        increment_global_active(&shared, "test_unrelated_turn");
        let bindings = crate::db::agents::AgentChannelBindings {
            provider: Some("codex".into()),
            discord_channel_cc: Some(CHANNEL_ID.to_string()),
            discord_channel_cdx: Some((CHANNEL_ID + 2).to_string()),
            ..Default::default()
        };
        let provider = bindings
            .provider_for_channel(|channel| channel == CHANNEL_ID.to_string())
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            health::start_headless_agent_turn(
                &registry,
                harness.channel_id,
                provider,
                "status".into(),
                Some("imessage".into()),
                None,
                Some("unconfigured-workspace".into()),
            ),
        )
        .await
        .unwrap();
        assert!(
            matches!(result, Err(router::HeadlessTurnStartError::Internal(ref reason)) if reason.contains("no workspace resolved")),
            "{result:?}"
        );
        assert!(
            mailbox_snapshot(&shared, harness.channel_id)
                .await
                .cancel_token
                .is_none(),
            "setup failure must release its mailbox"
        );
        assert_eq!(
            shared.restart.global_active.load(Ordering::Relaxed),
            1,
            "failed setup must preserve another active turn's count"
        );
        assert!(Arc::ptr_eq(
            mailbox_snapshot(&shared, unrelated_channel)
                .await
                .cancel_token
                .as_ref()
                .unwrap(),
            &unrelated
        ));
        assert!(!unrelated.cancelled.load(Ordering::Relaxed));
        assert_eq!(other.restart.global_active.load(Ordering::Relaxed), 0);
        assert!(other.mailbox_peek(harness.channel_id).is_none());
        assert!(shared.core.lock().await.sessions.is_empty());
        assert_eq!(harness.placeholder_posts(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn headless_turn_provider_mismatch_refuses_before_mailbox_or_session_mutation() {
        let harness = RelayE2eHarness::start().await;
        let cases = [
            (ProviderKind::Claude, ProviderKind::Codex),
            (ProviderKind::Codex, ProviderKind::Claude),
            (ProviderKind::Claude, ProviderKind::Claude),
            (ProviderKind::Codex, ProviderKind::Codex),
        ];
        for (runtime, execution) in cases {
            let mut shared = make_shared_data_for_tests();
            Arc::get_mut(&mut shared).unwrap().provider = runtime.clone();
            shared.settings.write().await.provider = runtime.clone();
            let channel = harness.channel_id;
            let path = crate::runtime_layout::role_map_path(harness.root.path());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, serde_json::json!({"byChannelId": {
            channel.to_string(): {"roleId": "provider-test", "promptFile": "prompt.md", "provider": execution.as_str()}
        }}).to_string()).unwrap();
            let incumbent = Arc::new(CancelToken::new());
            assert!(
                mailbox_try_start_turn(
                    &shared,
                    channel,
                    incumbent.clone(),
                    UserId::new(1),
                    MessageId::new(900)
                )
                .await
            );
            let outcome = router::start_reserved_headless_turn_with_owner(
                &harness.ctx,
                channel,
                "status",
                "test-owner",
                UserId::new(1),
                &shared,
                "test-token",
                None,
                None,
                Some("provider-test".into()),
                None,
                None,
                router::reserve_headless_turn(),
            )
            .await;
            if runtime != execution {
                assert_eq!(
                    outcome,
                    Err(router::HeadlessTurnStartError::InvalidTarget(format!(
                        "headless provider mismatch: mailbox={} execution={}",
                        runtime.as_str(),
                        execution.as_str()
                    )))
                );
            } else {
                assert!(
                    matches!(outcome, Err(router::HeadlessTurnStartError::Conflict(ref reason)) if reason.contains("agent mailbox is busy"))
                );
            }
            let snapshot = mailbox_snapshot(&shared, channel).await;
            assert!(Arc::ptr_eq(
                snapshot.cancel_token.as_ref().unwrap(),
                &incumbent
            ));
            assert!(shared.core.lock().await.sessions.is_empty());
            assert_eq!(harness.placeholder_posts(), 0);
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        }
    }
}
