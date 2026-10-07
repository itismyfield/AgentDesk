use super::*;
use crate::config::TestEnvVarGuard;
use crate::services::discord::{
    mailbox_snapshot, mailbox_try_start_turn, make_shared_data_for_tests,
};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::provider_teardown::tests::test_support::FakeTmux;
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::*};
use crate::services::turn_orchestrator::{Intervention, InterventionMode, QueuePersistenceContext};
use poise::serenity_prelude::{MessageId, UserId};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) fn pin_verified_execution(
    tmux: &str,
    channel: u64,
    provider_root: &Path,
) -> BindingContext {
    prepare_execution(tmux, channel, provider_root, "verified")
}

fn prepare_execution(
    tmux: &str,
    channel: u64,
    provider_root: &Path,
    policy: &str,
) -> BindingContext {
    let context = BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: tmux.to_owned(),
        channel_id: Some(channel),
        owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(provider_root.to_owned()),
        first_prompt_digest: None,
        source_policy: Some(policy.into()),
    };
    PreparedIncarnation::create(context.clone()).unwrap();
    let marker = crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce");
    fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
    fs::write(marker, &context.execution_nonce).unwrap();
    dedupe::register_tmux_channel(tmux, channel);
    context
}

pub(crate) async fn seed_accepted_queue(shared: &Arc<SharedData>, channel: ChannelId) {
    let item = Intervention {
        author_id: UserId::new(1),
        author_is_bot: false,
        message_id: MessageId::new(6_845_014),
        queued_generation: super::super::runtime_store::process_generation(),
        source_message_ids: vec![MessageId::new(6_845_014)],
        source_message_queued_generations: vec![],
        source_text_segments: vec![],
        text: "retained accepted queue".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: vec![],
        voice_announcement: None,
    };
    let persistence = QueuePersistenceContext::new(&ProviderKind::Codex, &shared.token_hash, None);
    assert!(
        shared
            .mailbox(channel)
            .enqueue(item.clone(), persistence)
            .await
            .enqueued
    );
    crate::services::turn_orchestrator::save_channel_pending_dispatch_marker(
        &ProviderKind::Codex,
        &shared.token_hash,
        channel,
        &item,
        None,
    )
    .unwrap();
}

pub(crate) fn pending_dispatch_bytes(shared: &SharedData, channel: ChannelId) -> Vec<u8> {
    let path = super::super::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("codex")
        .join(&shared.token_hash)
        .join(format!("{}.dispatch", channel.get()));
    fs::read(path).unwrap()
}

pub(crate) async fn accepted_queue_len(shared: &SharedData, channel: ChannelId) -> usize {
    mailbox_snapshot(shared, channel)
        .await
        .intervention_queue
        .len()
}

pub(crate) async fn seed_reset_race_session(
    shared: &SharedData,
    channel: ChannelId,
    name: &str,
    upload: &Path,
) {
    shared.core.lock().await.sessions.insert(
        channel,
        super::super::DiscordSession {
            session_id: Some("reset-race-native-session".into()),
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: vec![crate::ui::ai_screen::HistoryItem {
                item_type: crate::ui::ai_screen::HistoryType::User,
                content: "retained history before verified pin".into(),
            }],
            pending_uploads: vec![upload.to_string_lossy().into_owned().into()],
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(channel.get()),
            channel_name: Some(name.into()),
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: shared.restart.current_generation,
        },
    );
}

pub(crate) async fn assert_reset_race_session(
    shared: &SharedData,
    channel: ChannelId,
    upload: &Path,
) {
    let core = shared.core.lock().await;
    let session = core.sessions.get(&channel).unwrap();
    assert_eq!(
        session.session_id.as_deref(),
        Some("reset-race-native-session")
    );
    assert_eq!(session.history.len(), 1);
    assert_eq!(
        session.history[0].content,
        "retained history before verified pin"
    );
    let upload = upload.to_string_lossy().into_owned();
    assert_eq!(
        session.pending_uploads,
        vec![crate::services::cluster::attachment_transfer::uploads::Upload::Local(upload)]
    );
    assert!(!session.cleared);
    drop(core);
}

struct Fixture {
    fake: FakeTmux,
    root: tempfile::TempDir,
    _env: [TestEnvVarGuard; 2],
    channel: ChannelId,
    name: String,
    tmux: String,
    files: Vec<(PathBuf, Vec<u8>)>,
    alive: Arc<AtomicBool>,
    _dedupe_lock: std::sync::MutexGuard<'static, ()>,
    _env_lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl Fixture {
    fn new() -> Self {
        let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, env) = binding_context_fixture();
        dedupe::reset_state_for_tests();
        dedupe::binding_events::set_test_root(Some(root.path()));
        let channel = ChannelId::new(6_845_012);
        let name = format!("reset-late-{}", uuid::Uuid::new_v4().simple());
        let tmux = ProviderKind::Codex.build_tmux_session_name(&name);
        let nonce_path = crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce");
        fs::create_dir_all(Path::new(&nonce_path).parent().unwrap()).unwrap();
        let mut files: Vec<_> = [
            "out",
            "jsonl",
            crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        ]
        .into_iter()
        .map(|extension| {
            let path = PathBuf::from(crate::services::tmux_common::session_temp_path(
                &tmux, extension,
            ));
            let bytes = format!("retained-{extension}\n").into_bytes();
            fs::write(&path, &bytes).unwrap();
            (path, bytes)
        })
        .collect();
        let upload = root
            .path()
            .join("runtime/discord_uploads")
            .join(channel.get().to_string())
            .join("retained.txt");
        fs::create_dir_all(upload.parent().unwrap()).unwrap();
        fs::write(&upload, b"retained upload artifact").unwrap();
        files.push((upload, b"retained upload artifact".to_vec()));
        let fake = FakeTmux::install(&tmux);
        let alive = Arc::new(AtomicBool::new(true));
        crate::services::session_backend::insert_process_session(
            tmux.clone(),
            crate::services::session_backend::SessionHandle::TestProcess {
                pid: 6_845_012,
                alive: alive.clone(),
            },
        );
        Self {
            fake,
            root,
            _env: env,
            channel,
            name,
            tmux,
            files,
            alive,
            _dedupe_lock: dedupe_lock,
            _env_lock: env_lock,
        }
    }

    async fn runtime(
        &self,
    ) -> (
        Arc<SharedData>,
        super::super::health::HealthRegistry,
        Arc<CancelToken>,
    ) {
        let shared = make_shared_data_for_tests();
        shared.settings.write().await.provider = ProviderKind::Codex;
        shared.core.lock().await.sessions.insert(
            self.channel,
            super::super::DiscordSession {
                session_id: Some("retained-session".into()),
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: None,
                history: vec![crate::ui::ai_screen::HistoryItem {
                    item_type: crate::ui::ai_screen::HistoryType::User,
                    content: "retained history".into(),
                }],
                pending_uploads: vec![
                    self.root
                        .path()
                        .join("runtime/discord_uploads")
                        .join(self.channel.get().to_string())
                        .join("retained.txt")
                        .to_string_lossy()
                        .into_owned()
                        .into(),
                ],
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(self.channel.get()),
                channel_name: Some(self.name.clone()),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: shared.restart.current_generation,
            },
        );
        let token = Arc::new(CancelToken::new());
        token.bind_unmanaged_session_name(&self.tmux);
        assert!(
            mailbox_try_start_turn(
                &shared,
                self.channel,
                token.clone(),
                UserId::new(1),
                MessageId::new(6_845_013)
            )
            .await
        );
        seed_accepted_queue(&shared, self.channel).await;
        let registry = super::super::health::HealthRegistry::new();
        registry.register("codex".into(), shared.clone()).await;
        (shared, registry, token)
    }

    fn queue_path(&self, shared: &SharedData) -> PathBuf {
        self.root
            .path()
            .join("runtime/discord_pending_queue/codex")
            .join(&shared.token_hash)
            .join(format!("{}.json", self.channel.get()))
    }

    async fn assert_unchanged(
        &self,
        shared: &SharedData,
        token: &CancelToken,
        queue_bytes: &[u8],
        dispatch_bytes: &[u8],
    ) {
        let snapshot = mailbox_snapshot(shared, self.channel).await;
        assert!(snapshot.cancel_token.is_some(), "the active turn survives");
        assert_eq!(snapshot.active_turn_nonce.as_deref(), token.turn_nonce());
        assert!(!token.cancelled.load(Ordering::SeqCst));
        assert_eq!(snapshot.intervention_queue.len(), 1);
        assert_eq!(fs::read(self.queue_path(shared)).unwrap(), queue_bytes);
        assert_eq!(pending_dispatch_bytes(shared, self.channel), dispatch_bytes);
        let data = shared.core.lock().await;
        let session = data.sessions.get(&self.channel).unwrap();
        assert_eq!(session.session_id.as_deref(), Some("retained-session"));
        assert_eq!(session.history.len(), 1);
        assert_eq!(session.history[0].content, "retained history");
        let upload = self
            .root
            .path()
            .join("runtime/discord_uploads")
            .join(self.channel.get().to_string())
            .join("retained.txt")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            session.pending_uploads,
            vec![crate::services::cluster::attachment_transfer::uploads::Upload::Local(upload)]
        );
        assert!(!session.cleared);
        assert!(self.alive.load(Ordering::SeqCst));
        for (path, bytes) in &self.files {
            assert_eq!(fs::read(path).unwrap(), *bytes);
        }
        let calls = self.fake.take_calls();
        assert!(
            calls
                .iter()
                .all(|call| !call.starts_with("kill-session") && !call.starts_with("send-keys")),
            "{calls:?}"
        );
    }
}

fn binding_context_fixture() -> (tempfile::TempDir, [TestEnvVarGuard; 2]) {
    dedupe::binding_context::tests::fixture_after_shared_test_env_lock()
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = crate::services::session_backend::remove_process_session(&self.tmux);
        dedupe::reset_state_for_tests();
        dedupe::binding_events::set_test_root(None);
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn verified_reset_verdict_refuses_before_host_validation_and_preserves_queue() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let (shared, registry, token) = fixture.runtime().await;
        pin_verified_execution(&fixture.tmux, fixture.channel.get(), fixture.root.path());
        let queue_bytes = fs::read(fixture.queue_path(&shared)).unwrap();
        let dispatch_bytes = pending_dispatch_bytes(&shared, fixture.channel);
        let verdict = managed_reset_verdict(&registry, "codex", fixture.channel, None)
            .await
            .unwrap();
        assert!(
            verdict
                .refusal()
                .is_some_and(|reason| reason.contains("검증된 Codex"))
        );
        assert!(matches!(verdict.apply().await, ManagedReset::Refused(_)));
        fixture
            .assert_unchanged(&shared, &token, &queue_bytes, &dispatch_bytes)
            .await;
    });
}

#[test]
fn verified_reset_verdict_rechecks_late_pin_before_first_mailbox_clear() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let (shared, registry, token) = fixture.runtime().await;
        let verdict = managed_reset_verdict(&registry, "codex", fixture.channel, None).await.unwrap();
        assert!(verdict.refusal().is_none(), "the legacy target was approved");
        pin_verified_execution(&fixture.tmux, fixture.channel.get(), fixture.root.path());
        let queue_bytes = fs::read(fixture.queue_path(&shared)).unwrap();
        let dispatch_bytes = pending_dispatch_bytes(&shared, fixture.channel);
        assert!(matches!(verdict.apply().await, ManagedReset::Refused(reason) if reason.contains("검증된 Codex")));
        fixture.assert_unchanged(&shared, &token, &queue_bytes, &dispatch_bytes).await;
    });
}

#[test]
fn verified_health_clear_rechecks_late_pin_after_host_validation() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let (shared, registry, token) = fixture.runtime().await;
        let tmux = fixture.tmux.clone();
        let channel = fixture.channel.get();
        let root = fixture.root.path().to_owned();
        let visited = Arc::new(AtomicBool::new(false));
        let callback_visited = visited.clone();
        before_verified_clear_recheck_for_tests(channel, move || {
            callback_visited.store(true, Ordering::SeqCst);
            pin_verified_execution(&tmux, channel, &root);
        });
        let queue_bytes = fs::read(fixture.queue_path(&shared)).unwrap();
        let dispatch_bytes = pending_dispatch_bytes(&shared, fixture.channel);
        let result = super::super::health::clear_provider_channel_runtime(
            &registry,
            "codex",
            fixture.channel,
            None,
        )
        .await;
        assert!(
            visited.load(Ordering::SeqCst),
            "the host admitted the legacy target first"
        );
        assert!(
            matches!(result, Some(ManagedReset::Refused(reason)) if reason.contains("검증된 Codex"))
        );
        fixture
            .assert_unchanged(&shared, &token, &queue_bytes, &dispatch_bytes)
            .await;
    });
}

#[test]
fn verified_startup_reset_hold_is_limited_to_the_exact_trial_target() {
    use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
    use crate::services::codex_tui::session::source_observation::{
        CodexSourceMode, SOURCE_MODE_TEST,
    };
    struct ModeRestore(Option<CodexSourceMode>);
    impl Drop for ModeRestore {
        fn drop(&mut self) {
            SOURCE_MODE_TEST.with(|mode| mode.set(self.0));
        }
    }
    let fixture = Fixture::new();
    let _mode =
        ModeRestore(SOURCE_MODE_TEST.with(|mode| mode.replace(Some(CodexSourceMode::Verified))));
    runtime().block_on(async {
        let (shared, _, _) = fixture.runtime().await;
        let channel = ChannelId::new(CANARY_CHANNEL);
        {
            let mut core = shared.core.lock().await;
            let mut session = core.sessions.remove(&fixture.channel).unwrap();
            session.channel_id = Some(CANARY_CHANNEL);
            session.channel_name = Some("adk-codex-tui-e2e".into());
            core.sessions.insert(channel, session);
        }
        let guard = super::super::commands::control::verified_codex_reset_refusal_for_target;
        assert!(
            guard(&shared, &ProviderKind::Codex, channel, None)
                .await
                .is_some(),
            "hold the actual trial target before its first pin"
        );
        prepare_execution(CANARY_TMUX, CANARY_CHANNEL, fixture.root.path(), "legacy");
        assert!(
            guard(&shared, &ProviderKind::Codex, channel, None)
                .await
                .is_some(),
            "startup verified intentionally holds even a legacy-pinned exact trial target"
        );
        assert_eq!(
            guard(
                &shared,
                &ProviderKind::Codex,
                fixture.channel,
                Some(CANARY_TMUX)
            )
            .await,
            None,
            "the same tmux name on a different channel retains legacy admission"
        );
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&channel)
            .unwrap()
            .channel_name = Some(fixture.name.clone());
        dedupe::register_tmux_channel(&fixture.tmux, CANARY_CHANNEL);
        assert_eq!(
            guard(&shared, &ProviderKind::Codex, channel, None).await,
            None,
            "an alternate legacy tmux on the trial channel remains admitted"
        );
        shared.core.lock().await.sessions.remove(&channel);
        dedupe::reset_state_for_tests();
        assert!(
            guard(&shared, &ProviderKind::Codex, channel, Some(CANARY_TMUX))
                .await
                .is_some(),
            "an explicitly judged target holds with no runtime row or owner mirror"
        );
        crate::services::tmux_common::write_tmux_channel_binding(CANARY_TMUX, CANARY_CHANNEL)
            .unwrap();
        assert!(
            guard(&shared, &ProviderKind::Codex, channel, None)
                .await
                .is_some(),
            "a durable exact channel witness covers the lost session row"
        );
        SOURCE_MODE_TEST.with(|mode| mode.set(Some(CodexSourceMode::Legacy)));
        assert_eq!(
            guard(&shared, &ProviderKind::Codex, channel, Some(CANARY_TMUX)).await,
            None,
            "legacy startup mode preserves the prior admission"
        );
    });
}
