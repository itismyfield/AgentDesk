//! Shared fixture for the in-process Discord relay e2e scenarios.
//!
//! A scenario boots [`RelayE2eHarness`], injects through a production entry
//! point — Discord `FullEvent` intake or a tmux-direct prompt observation — and
//! reads mailbox, durable queue, dispatch witnesses, relay leases and turn
//! completion edges back out.
//!
//! Every wait here is state- or event-driven under a deadline. The harness needs
//! a `multi_thread` runtime to drive a real HTTP server and real production
//! workers, and `tokio`'s clock cannot be paused there, so a fixed sleep would
//! be wall-clock flake. Scenarios must not add one.

// The fixture carries the observation surface the #5997 scenarios need; those
// scenarios land per repair lane, so parts are not called yet.
#![allow(dead_code)]

mod discord_mock;

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

pub(super) use discord_mock::{CHANNEL_ID, USER_ID, user_message};

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

/// Wall-clock probe for per-injection-path settlement cost, anchored when created.
pub(super) struct SettlementProbe {
    rx: Receiver<TurnCompletionEvent>,
    started: tokio::time::Instant,
}

impl SettlementProbe {
    /// Settlement latency from this probe's creation to `expected`. Panics at
    /// `budget`, naming every edge that arrived instead.
    pub(super) async fn await_edge(
        &mut self,
        expected: TurnCompletionEvent,
        budget: Duration,
    ) -> Duration {
        let deadline = self.started + budget;
        let mut observed: Vec<TurnCompletionEvent> = Vec::new();
        loop {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Err(_) => panic!(
                    "settlement did not reach {expected:?} within {budget:?}; observed={observed:?}"
                ),
                Ok(Ok(event)) if event == expected => return self.started.elapsed(),
                Ok(Ok(event)) => observed.push(event),
                Ok(Err(error)) => {
                    panic!("settlement receiver must remain open; recv error={error:?}")
                }
            }
        }
    }
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
    _server: AbortOnDrop<()>,
    _dedupe_guard: std::sync::MutexGuard<'static, ()>,
    _intake_guard: crate::config::TestEnvVarGuard,
    _root_guard: crate::config::TestEnvVarGuard,
    _env_lock: std::sync::MutexGuard<'static, ()>,
    root: tempfile::TempDir,
}

impl RelayE2eHarness {
    pub(super) async fn start() -> Self {
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
        let dedupe_guard = dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        let mock = discord_mock::DiscordMockState::new();
        let (proxy, gateway_url, server) = discord_mock::start(mock.clone()).await;
        let ctx = discord_mock::serenity_context(proxy, gateway_url).await;
        let shared = crate::services::discord::make_shared_data_for_tests();
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
        crate::services::discord::rebind_channel_session(
            &shared,
            &ProviderKind::Claude,
            channel_id,
            &cwd,
            SESSION_UUID,
        )
        .await;

        Self {
            data,
            shared,
            ctx,
            channel_id,
            mock,
            _server: AbortOnDrop(Some(server)),
            _dedupe_guard: dedupe_guard,
            _intake_guard: intake_guard,
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
        let mut task = tokio::spawn({
            let ctx = self.ctx.clone();
            let data = self.clone_data();
            async move { router::handle_event(&ctx, &event, &data).await }
        });
        tokio::select! {
            _ = self.mock.first_placeholder_arrived.notified() => {}
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

    pub(super) fn start_settlement_probe(&self) -> SettlementProbe {
        SettlementProbe {
            rx: self.subscribe_completions(),
            started: tokio::time::Instant::now(),
        }
    }

    /// Placeholder POSTs seen by the mock: the harness' dispatch witness.
    pub(super) fn placeholder_posts(&self) -> usize {
        self.mock.placeholder_posts.load(Ordering::SeqCst)
    }

    /// Non-placeholder POSTs, which carry local-only control notes.
    pub(super) fn local_note_posts(&self) -> usize {
        self.mock.local_note_posts.load(Ordering::SeqCst)
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

    /// Edge-triggered: register before the action that should dispatch.
    pub(super) async fn wait_for_next_placeholder(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.mock.second_placeholder_arrived.notified())
            .await
            .is_ok()
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
