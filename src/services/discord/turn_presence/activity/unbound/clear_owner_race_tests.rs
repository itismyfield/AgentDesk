//! Actual clear, watcher exit and automatic kickoff; provider entry is a deterministic stand-in.
use super::*;
use crate::services::discord::queue_dispatch::kickoff::{
    install_owner_race_pause, owner_race_kick_attempts,
};
use crate::services::discord::router::message_handler::{
    CLAUDE_LEGACY_PROBE, INPUT_EFFECT_PROBE, InputEffectProbe,
};
use std::sync::mpsc;

#[path = "midread_owner_tests.rs"]
mod midread_owner_tests;

const DRAIN_LIMIT: Duration = Duration::from_secs(3 * 6);

struct ProviderGuard(Arc<AtomicUsize>);
impl ProviderGuard {
    fn new(channel: u64) -> Self {
        let extra = Arc::new(AtomicUsize::new(0));
        let count = extra.clone();
        assert!(
            CLAUDE_LEGACY_PROBE
                .lock()
                .unwrap()
                .replace((
                    channel,
                    Box::new(move || {
                        count.fetch_add(1, Ordering::SeqCst);
                        Err("unexpected duplicate provider entry".into())
                    })
                ))
                .is_none()
        );
        Self(extra)
    }
}
impl Drop for ProviderGuard {
    fn drop(&mut self) {
        INPUT_EFFECT_PROBE.lock().unwrap().take();
        CLAUDE_LEGACY_PROBE.lock().unwrap().take();
    }
}
struct ProviderTurn {
    entered: tokio::sync::oneshot::Receiver<std::thread::ThreadId>,
    release: mpsc::Sender<()>,
}
impl ProviderTurn {
    fn arm(channel: u64) -> Self {
        let (entered, received) = tokio::sync::oneshot::channel();
        let (release, wait) = mpsc::channel();
        assert!(
            INPUT_EFFECT_PROBE
                .lock()
                .unwrap()
                .replace(InputEffectProbe {
                    channel,
                    entered,
                    release: wait,
                    terminal_before_release: false,
                })
                .is_none()
        );
        Self {
            entered: received,
            release,
        }
    }
    async fn wait(&mut self, fixture: &Fixture, expected: u64) -> u64 {
        self.wait_with_limit(fixture, expected, DRAIN_LIMIT).await
    }
    async fn wait_with_limit(&mut self, fixture: &Fixture, expected: u64, limit: Duration) -> u64 {
        let entry = tokio::time::timeout(limit, &mut self.entered).await;
        if !matches!(&entry, Ok(Ok(_))) {
            let mailbox = tokio::time::timeout(
                Duration::from_secs(1),
                fixture.rig.shared.mailbox(fixture.rig.channel).snapshot(),
            )
            .await;
            if let Ok(state) = mailbox {
                let ids = state
                    .intervention_queue
                    .iter()
                    .map(|item| item.message_id.get())
                    .collect::<Vec<_>>();
                eprintln!(
                    "A2_FIFO_TIMEOUT expected={expected} queue_ids={ids:?} disk_ids={:?} token_present={} active_user_message_id={:?} pending_user_dispatch={:?}",
                    fixture.disk_ids(),
                    state.cancel_token.is_some(),
                    state.active_user_message_id.map(|id| id.get()),
                    state.pending_user_dispatch.map(|id| id.get()),
                );
            } else {
                eprintln!("A2_FIFO_TIMEOUT expected={expected} mailbox_snapshot=timeout");
            }
            match inflight::load_inflight_state_read_only_result(
                &fixture.provider,
                fixture.rig.channel.get(),
            ) {
                Ok(Some(row)) => eprintln!(
                    "A2_FIFO_TIMEOUT_INFLIGHT expected={expected} state=present user_msg_id={} runtime_kind={:?} runtime_unknown={}",
                    row.user_msg_id, row.runtime_kind, row.runtime_kind_unknown_on_disk
                ),
                Ok(None) => eprintln!("A2_FIFO_TIMEOUT_INFLIGHT expected={expected} state=absent"),
                Err(_) => {
                    eprintln!("A2_FIFO_TIMEOUT_INFLIGHT expected={expected} state=unreadable")
                }
            }
        }
        assert!(
            matches!(&entry, Ok(Ok(_))),
            "queued FIFO source {expected} must enter provider within {limit:?}: {entry:?}"
        );
        let thread = entry.unwrap().unwrap();
        assert_ne!(thread, std::thread::current().id());
        assert!(INPUT_EFFECT_PROBE.lock().unwrap().is_none());
        let row =
            inflight::load_inflight_state(&fixture.provider, fixture.rig.channel.get()).unwrap();
        assert_eq!(
            row.user_msg_id, expected,
            "the entered provider must own the FIFO source"
        );
        row.user_msg_id
    }
    fn finish(self) {
        self.release.send(()).unwrap();
    }
}

async fn until(mut ready: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(DRAIN_LIMIT, async {
        loop {
            if ready().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("drain exceeded six supervisor ticks");
}

impl Fixture {
    async fn attach_old_watcher(&self) -> tokio::task::JoinHandle<()> {
        let name = self.rig.shared.core.lock().await.sessions[&self.rig.channel]
            .channel_name
            .clone()
            .unwrap();
        let tmux = self.provider.build_tmux_session_name(&name);
        let key = adk_session::build_namespaced_session_key(
            &self.rig.shared.token_hash,
            &self.provider,
            &tmux,
        );
        inflight::seed_session_row_hashed(
            self.rig.shared.pg_pool.as_ref().unwrap(),
            &key,
            self.rig.channel.get(),
            &self.rig.shared.token_hash,
            None,
        )
        .await;
        self.host.start(&tmux);
        let transcript = self.rig.root.path().join("before-clear.jsonl");
        std::fs::write(&transcript, "{\"type\":\"user\",\"uuid\":\"old\",\"message\":{\"content\":\"old direct turn\"}}\n{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n").unwrap();
        let helper =
            crate::services::discord::turn_presence::activity::tests::companion_attach_log_watcher;
        helper(&self.rig.shared, self.rig.channel.get(), &tmux, &transcript);
        crate::services::discord::turn_presence::activity::tests::log_turn_source(
            &self.provider,
            self.rig.channel.get(),
            &tmux,
            "old-cleared",
            &transcript,
        );
        let handle = self
            .rig
            .shared
            .tmux_watchers
            .get(&self.rig.channel)
            .unwrap();
        let flags = (
            handle.cancel.clone(),
            handle.paused.clone(),
            handle.resume_offset.clone(),
            handle.pause_epoch.clone(),
            handle.turn_delivered.clone(),
            handle.last_heartbeat_ts_ms.clone(),
        );
        drop(handle);
        let task = tokio::spawn(
            crate::services::discord::tmux::tmux_output_watcher_with_restore(
                self.rig.channel,
                self.ctx.http.clone(),
                self.rig.shared.clone(),
                transcript.display().to_string(),
                tmux,
                0,
                flags.0,
                flags.1,
                flags.2,
                flags.3,
                flags.4,
                flags.5,
                None,
            ),
        );
        assert!(!task.is_finished(), "actual watcher must run before clear");
        task
    }
    async fn clear(&self) {
        use poise::serenity_prelude::{Message, UserId};
        let mut message = Message::default();
        message.id = MessageId::new(self.rig.channel.get() + 1);
        message.channel_id = self.rig.channel;
        message.author.id = UserId::new(100);
        message.author.name = "a2-owner".into();
        message.content = "!clear".into();
        let mut voice_config = crate::voice::VoiceConfig::default();
        voice_config.audio.recordings_dir = self.rig.root.path().join("voice");
        let data = crate::services::discord::Data {
            shared: self.rig.shared.clone(),
            token: "test-token".into(),
            provider: self.provider.clone(),
            voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
            voice_config,
            ingress_shadow: None,
        };
        let mut permit = None;
        assert!(
            Box::pin(
                crate::services::discord::commands::handle_text_command_with_uploads(
                    &self.ctx,
                    &message,
                    &data,
                    self.rig.channel,
                    "!clear",
                    &[],
                    &mut permit,
                )
            )
            .await
            .unwrap()
        );
        let core = self.rig.shared.core.lock().await;
        let session = core.sessions.get(&self.rig.channel).unwrap();
        assert!(
            session.cleared && session.channel_name.is_some(),
            "actual clear retains the core basis"
        );
        drop(core);
        assert!(
            self.rig
                .shared
                .mailbox(self.rig.channel)
                .snapshot()
                .await
                .intervention_queue
                .is_empty()
        );
    }
    async fn enqueue_pair(&self, a: u64, b: u64) {
        let context =
            || QueuePersistenceContext::new(&self.provider, &self.rig.shared.token_hash, None);
        for (id, text) in [(a, "AFTER_CLEAR_A"), (b, "AFTER_CLEAR_B")] {
            assert!(
                self.rig
                    .shared
                    .mailbox(self.rig.channel)
                    .enqueue(queued(id, text), context())
                    .await
                    .enqueued
            );
        }
        assert_eq!(self.disk_ids(), [a, b]);
    }
    async fn assert_finished(&self) {
        until(async || {
            let state = self.rig.shared.mailbox(self.rig.channel).snapshot().await;
            state.cancel_token.is_none()
                && state.pending_user_dispatch.is_none()
                && state.intervention_queue.is_empty()
                && inflight::load_inflight_state(&self.provider, self.rig.channel.get()).is_none()
        })
        .await;
        assert!(self.disk_ids().is_empty());
        assert!(
            self.mock.unhandled.lock().unwrap().is_empty(),
            "{:?}",
            self.mock.unhandled.lock().unwrap()
        );
    }
}

async fn clear_race(id: u64, kick_before_exit: bool) {
    let fixture = Fixture::connect_and_migrate_fixture(id, ProviderKind::Claude).await;
    fixture.produce_owners();
    let (before, release_before) =
        install_owner_race_pause(fixture.rig.channel, "watcher_before_release");
    let (after, release_after) =
        install_owner_race_pause(fixture.rig.channel, "watcher_after_release");
    let mut watcher = Some(fixture.attach_old_watcher().await);
    fixture.clear().await;
    reached(&before).await;
    assert!(
        fixture
            .rig
            .shared
            .tmux_watchers
            .channel_binding(&fixture.rig.channel)
            .is_some()
    );
    if !kick_before_exit {
        release_before.notify_one();
        reached(&after).await;
        assert!(
            fixture
                .rig
                .shared
                .tmux_watchers
                .channel_binding(&fixture.rig.channel)
                .is_none()
        );
        release_after.notify_one();
        tokio::time::timeout(DRAIN_LIMIT, watcher.take().unwrap())
            .await
            .unwrap()
            .unwrap();
    }
    let (a, b) = (id + 201, id + 202);
    fixture.enqueue_pair(a, b).await;
    let guard = ProviderGuard::new(id);
    let mut first = ProviderTurn::arm(id);
    let (at_kick, release_kick) =
        install_owner_race_pause(fixture.rig.channel, "kickoff_before_take");
    let began = tokio::time::Instant::now();
    crate::services::discord::queue_io::schedule_post_enqueue_idle_queue_kick(
        fixture.rig.shared.clone(),
        fixture.provider.clone(),
        fixture.rig.channel,
    );
    reached(&at_kick).await;
    if kick_before_exit {
        assert!(
            fixture
                .rig
                .shared
                .tmux_watchers
                .channel_binding(&fixture.rig.channel)
                .is_some(),
            "E1 kick must enter before watcher release"
        );
        release_before.notify_one();
        reached(&after).await;
        assert!(
            fixture
                .rig
                .shared
                .tmux_watchers
                .channel_binding(&fixture.rig.channel)
                .is_none()
        );
        release_after.notify_one();
        tokio::time::timeout(DRAIN_LIMIT, watcher.take().unwrap())
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(
        fixture.observed().await,
        observed(Activity::Idle, "no_session"),
        "kick must consume complete Unbound evidence after watcher exit"
    );
    release_kick.notify_one();
    let mut ledger = vec![first.wait(&fixture, a).await];
    let first_entry = began.elapsed();
    assert!(first_entry <= DRAIN_LIMIT);
    assert_eq!(fixture.disk_ids(), [b]);
    let snapshot = fixture
        .rig
        .shared
        .mailbox(fixture.rig.channel)
        .snapshot()
        .await;
    assert_eq!(snapshot.active_user_message_id, Some(MessageId::new(a)));
    assert_eq!(
        snapshot
            .intervention_queue
            .iter()
            .map(|item| item.message_id.get())
            .collect::<Vec<_>>(),
        [b]
    );
    let second = ProviderTurn::arm(id);
    // These real consumers race the already running head and must preserve the tail.
    crate::services::discord::queue_io::schedule_post_enqueue_idle_queue_kick(
        fixture.rig.shared.clone(),
        fixture.provider.clone(),
        fixture.rig.channel,
    );
    let duplicate =
        crate::services::discord::queue_io::kick_idle_queue_channel_if_context_available(
            &fixture.rig.shared,
            &fixture.provider,
            fixture.rig.channel,
            "idle_queue_backstop",
        )
        .await;
    assert!(!duplicate.started);
    assert_eq!(fixture.disk_ids(), [b]);
    first.finish();
    let mut second = second;
    ledger.push(second.wait(&fixture, b).await);
    second.finish();
    fixture.assert_finished().await;
    let drained = began.elapsed();
    assert!(
        drained <= DRAIN_LIMIT,
        "final drain exceeded six supervisor ticks: {drained:?}"
    );
    assert_eq!(ledger, [a, b]);
    assert_eq!(ledger.iter().filter(|id| **id == a).count(), 1);
    assert_eq!(ledger.iter().filter(|id| **id == b).count(), 1);
    assert_eq!(guard.0.load(Ordering::SeqCst), 0);
    let attempts = owner_race_kick_attempts(fixture.rig.channel);
    assert!(attempts >= 1);
    eprintln!(
        "A2_CLEAR_RACE kick_before_exit={kick_before_exit} entries={ledger:?} duplicates={} ready_kick_attempts={attempts} first_entry_ms={} final_drain_ms={} limit_ticks=6 tick_ms=3000",
        guard.0.load(Ordering::SeqCst),
        first_entry.as_millis(),
        drained.as_millis()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a2_clear_enqueue_kick_then_watcher_exit_drains_fifo_once_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_clear_enqueue_kick_then_watcher_exit_drains_fifo_once_pg"
    )) {
        return;
    }
    clear_race(69_184_111, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a2_clear_watcher_exit_then_enqueue_kick_drains_fifo_once_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_clear_watcher_exit_then_enqueue_kick_drains_fifo_once_pg"
    )) {
        return;
    }
    clear_race(69_184_112, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a2_closed_bound_read_survives_clear_until_watcher_release_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_closed_bound_read_survives_clear_until_watcher_release_pg"
    )) {
        return;
    }
    let fixture = Fixture::connect_and_migrate_fixture(69_184_113, ProviderKind::Claude).await;
    fixture.produce_owners();
    let (before, release) = install_owner_race_pause(fixture.rig.channel, "watcher_before_release");
    let watcher = fixture.attach_old_watcher().await;
    until(async || fixture.observed().await.reason == "closed").await;
    fixture.clear().await;
    reached(&before).await;
    assert!(
        fixture
            .rig
            .shared
            .tmux_watchers
            .channel_binding(&fixture.rig.channel)
            .is_some()
    );
    assert_eq!(fixture.observed().await, observed(Activity::Idle, "closed"));
    release.notify_one();
    tokio::time::timeout(DRAIN_LIMIT, watcher)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fixture.observed().await,
        observed(Activity::Idle, "no_session")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a2_turn_start_route_requires_foreign_owner_then_starts_once_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_turn_start_route_requires_foreign_owner_then_starts_once_pg"
    )) {
        return;
    }
    let fixture = Fixture::connect_and_migrate_fixture(69_184_114, ProviderKind::Claude).await;
    let pool = fixture.rig.shared.pg_pool.as_ref().unwrap();
    crate::db::agents::insert_agent_channels_for_tests(
        pool,
        "a2-route",
        Some(&fixture.rig.channel.to_string()),
        None,
    )
    .await;
    let guard = ProviderGuard::new(fixture.rig.channel.get());
    let mut turn = ProviderTurn::arm(fixture.rig.channel.get());
    let (blocked, body) = post_start(&fixture.app, "a2-route").await;
    assert_eq!(blocked, axum::http::StatusCode::CONFLICT, "{body}");
    assert!(
        INPUT_EFFECT_PROBE.lock().unwrap().is_some(),
        "missing owner entered a provider"
    );
    fixture.produce_owners();
    let (started, body) = post_start(&fixture.app, "a2-route").await;
    assert_eq!(started, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["status"], "started");
    let source = inflight::load_inflight_state(&fixture.provider, fixture.rig.channel.get())
        .unwrap()
        .user_msg_id;
    turn.wait(&fixture, source).await;
    assert_eq!(
        body["turn_id"],
        format!("discord:{}:{source}", fixture.rig.channel)
    );
    let (again, refused) = post_start(&fixture.app, "a2-route").await;
    assert_eq!(again, axum::http::StatusCode::CONFLICT, "{refused}");
    turn.finish();
    fixture.assert_finished().await;
    assert_eq!(guard.0.load(Ordering::SeqCst), 0);
}

#[derive(Clone)]
struct LogSink(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a2_relaunch_open_canary_foreign_self_holds_dequeue_with_watcher_reason_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_relaunch_open_canary_foreign_self_holds_dequeue_with_watcher_reason_pg"
    )) {
        return;
    }
    canary_conflict_holds(false, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a2_local_foreign_owner_cannot_hide_runtime_self_claim_at_canary_dequeue_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_local_foreign_owner_cannot_hide_runtime_self_claim_at_canary_dequeue_pg"
    )) {
        return;
    }
    canary_conflict_holds(true, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a2_conflicting_foreign_runtime_owners_hold_canary_dequeue_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_conflicting_foreign_runtime_owners_hold_canary_dequeue_pg"
    )) {
        return;
    }
    canary_conflict_holds(true, false).await;
}

async fn canary_conflict_holds(local_conflict: bool, foreign_self: bool) {
    use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
    use crate::services::tui_prompt_dedupe::{
        binding_context::PreparedIncarnation, binding_events::codex, clear_relaunch,
    };
    let logs = LogSink(Arc::new(std::sync::Mutex::new(Vec::new())));
    tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(logs.clone())
        .try_init()
        .unwrap();
    let fixture = Fixture::connect_and_migrate_fixture(CANARY_CHANNEL, ProviderKind::Codex).await;
    fixture.produce_owners();
    if foreign_self {
        for runtime in &fixture.foreign {
            runtime
                .tmux_watchers
                .restore_owner_channel_for_tmux_session(&fixture.foreign_name, fixture.rig.channel);
        }
    }
    if local_conflict {
        assert!(
            fixture
                .rig
                .shared
                .tmux_watchers
                .restore_owner_channel_for_tmux_session(
                    &fixture.foreign_name,
                    ChannelId::new(fixture.foreign_channel.get() + 1),
                )
        );
    }
    let digest = format!("sha256:{}", "0".repeat(64));
    let prepared = PreparedIncarnation::prepare_pinned(
        "codex",
        CANARY_TMUX,
        Some(CANARY_CHANNEL),
        None,
        false,
        Some(
            fixture
                .rig
                .root
                .path()
                .join("codex-home")
                .canonicalize()
                .unwrap(),
        ),
        (Some(digest.clone()), Some("verified".into())),
    )
    .unwrap();
    let claim = codex::Claim {
        session_id: uuid::Uuid::new_v4().to_string(),
        path: None,
        evidence: codex::ClaimEvidence::NativeHook {
            event: "session_start".into(),
            source: Some("startup".into()),
            first_prompt_digest: Some(digest),
        },
    };
    codex::commit_claim(&prepared.context, &claim, codex::Decision::Pending, || {
        Ok(())
    })
    .unwrap();
    let captured = clear_relaunch::capture(CANARY_TMUX)
        .unwrap()
        .expect("durable pinned ownership fixture");
    clear_relaunch::commit(&captured, "explicit_clear").unwrap();
    assert!(
        clear_relaunch::relaunch_open(CANARY_TMUX),
        "the existing canary exception must be open in this test"
    );
    assert_eq!(
        fixture.observed().await,
        observed(Activity::Unknown, "watcher_unbound")
    );
    let source = CANARY_CHANNEL + 201;
    assert!(
        fixture
            .rig
            .shared
            .mailbox(fixture.rig.channel)
            .enqueue(
                queued(source, "self owner must hold"),
                QueuePersistenceContext::new(
                    &fixture.provider,
                    &fixture.rig.shared.token_hash,
                    None
                )
            )
            .await
            .enqueued
    );
    let mut probe = ProviderTurn::arm(CANARY_CHANNEL);
    let before = fixture.disk_ids();
    let outcome = crate::services::discord::queue_io::kick_idle_queue_channel_if_context_available(
        &fixture.rig.shared,
        &fixture.provider,
        fixture.rig.channel,
        "a2_self_canary",
    )
    .await;
    assert!(!outcome.started);
    assert_eq!(fixture.disk_ids(), before);
    assert_eq!(
        fixture
            .rig
            .shared
            .mailbox(fixture.rig.channel)
            .snapshot()
            .await
            .intervention_queue
            .len(),
        1
    );
    assert!(INPUT_EFFECT_PROBE.lock().unwrap().is_some());
    assert!(matches!(
        probe.entered.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.lines()
            .any(|line| line.contains("dequeue_held") && line.contains("watcher_unbound")),
        "the actual dequeue did not report its self-owner hold: {text}"
    );
    INPUT_EFFECT_PROBE.lock().unwrap().take();
}
