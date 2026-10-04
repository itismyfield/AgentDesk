#![cfg(unix)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use self::serenity::ChannelId;
use super::*;
use crate::db::session_transcripts::native_channel_clear_state;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::DiscordSession;
use crate::services::tmux_common;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use crate::services::tui_prompt_dedupe::binding_events::{
    self, BindingCause, CauseSource, HookSignal, Proposal, SourceId,
};

/// Records every native effect in order; a submit may run a hook and may wait to be released.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    save_fails: AtomicBool,
    on_submit: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    submit_entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    submit_release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    submitted: Mutex<Option<NativeClearSubmission>>,
}

impl Fake {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn note(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

impl NativeClearEffects for Fake {
    fn clear_selector<'a>(&'a self, session_key: &'a str) -> Effect<'a> {
        self.note(format!("clear:{session_key}"));
        Box::pin(async { true })
    }
    fn save_selector<'a>(&'a self, _: &'a str, session: &'a str, _: ChannelId) -> Effect<'a> {
        self.note(format!("save:{session}"));
        let ok = !self.save_fails.load(Ordering::SeqCst);
        Box::pin(async move { ok })
    }
    fn submit(&self, ticket: &ClearTicket, _: Instant) -> NativeClearSubmission {
        self.note("submit".into());
        if let Some(entered) = self.submit_entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        if let Some(release) = self.submit_release.lock().unwrap().take() {
            let _ = release.recv();
        }
        assert!(
            crate::services::claude_tui::host_input::MutationGate::admit(
                ticket,
                &ticket.context.tmux_session
            )
            .is_ok()
        );
        if let Some(hook) = self.on_submit.lock().unwrap().take() {
            hook();
        }
        self.submitted
            .lock()
            .unwrap()
            .unwrap_or(NativeClearSubmission::Confirmed)
    }
    fn composer_empty(&self, _: &str, _: Instant) -> bool {
        true
    }
    fn reset_process(&self, tmux: &str) {
        self.note(format!("reset:{tmux}"));
    }
}

struct Fixture {
    db: Option<crate::dispatch::test_support::DispatchPostgresTestDb>,
    pool: sqlx::PgPool,
    shared: Arc<SharedData>,
    http: Arc<serenity::Http>,
    channel_id: ChannelId,
    tmux: String,
    session_key: String,
    binding_root: tempfile::TempDir,
    _o: crate::services::tui_o::cutover::test_override::ChannelsGuard,
    _host: crate::config::TestEnvVarGuard,
    _root: crate::config::TestEnvVarGuard,
    _root_dir: tempfile::TempDir,
}

impl Fixture {
    async fn new(n: u64) -> Self {
        let root_dir = tempfile::tempdir().unwrap();
        // A config in the scratch root keeps node identity off any machine-wide config.
        let config = root_dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let data = serde_json::to_string(&root_dir.path().join("data")).unwrap();
        let yaml =
            format!("server: {{}}\ndata:\n  dir: {data}\ncluster: {{instance_id: test-node}}\n");
        std::fs::write(config.join("agentdesk.yaml"), yaml).unwrap();
        let root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root_dir.path());
        let host = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_INSTANCE_ID",
            "test-node".as_ref(),
        );
        let binding_root = tempfile::tempdir().unwrap();
        binding_events::set_test_root(Some(binding_root.path()));
        let channel_id = ChannelId::new(6_577_200 + n);
        let channel_name = format!("adk-6577-native-{n}-{}", uuid::Uuid::new_v4().simple());
        let tmux = ProviderKind::Claude.build_tmux_session_name(&channel_name);
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_native_clear_6577",
            "native clear wiring",
        )
        .await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        shared.core.lock().await.sessions.insert(
            channel_id,
            DiscordSession {
                session_id: Some("old".into()),
                memento_context_loaded: true,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(channel_id.get()),
                channel_name: Some(channel_name),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: shared.restart.current_generation,
            },
        );
        let build = super::super::super::super::adk_session::build_namespaced_session_key;
        let session_key = build(&shared.token_hash, &ProviderKind::Claude, &tmux);
        let seed = crate::services::discord::inflight::seed_session_row_keyed;
        seed(&pool, &session_key, channel_id.get(), None).await;
        let context = BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: tmux.clone(),
            channel_id: Some(channel_id.get()),
            owner_runtime_root: root_dir.path().display().to_string(),
            host: Some("test-node".into()),
            expected_native_session_id: Some("old".into()),
            launch_mode: "fresh".into(),
            provider_root: None,
        };
        let contexts = root_dir.path().join("runtime/binding_contexts/claude");
        std::fs::create_dir_all(&contexts).unwrap();
        let context_file = contexts.join(format!("{}.json", context.execution_nonce));
        std::fs::write(context_file, serde_json::to_vec(&context).unwrap()).unwrap();
        let marker = tmux_common::session_temp_path(&tmux, "spawn_nonce");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(&marker, &context.execution_nonce).unwrap();
        tmux_common::write_tmux_runtime_kind_marker(&tmux, RuntimeHandoffKind::ClaudeTui).unwrap();
        let force = crate::services::tui_o::cutover::test_override::force_channels;
        let fixture = Self {
            db: Some(db),
            pool,
            shared,
            http: Arc::new(serenity::Http::new("")),
            channel_id,
            tmux,
            session_key,
            binding_root,
            _o: force(&[(channel_id.get(), RuntimeHandoffKind::ClaudeTui)]),
            _host: host,
            _root: root,
            _root_dir: root_dir,
        };
        fixture.record("old", BindingCause::Startup, false);
        fixture
    }

    fn record(&self, session: &str, cause: BindingCause, pending: bool) {
        record_on(
            self.binding_root.path(),
            self.channel_id,
            &self.tmux,
            session,
            cause,
            pending,
        );
    }

    fn marker(&self) -> String {
        tmux_common::session_temp_path(&self.tmux, "spawn_nonce")
    }

    async fn clear(&self) -> anyhow::Result<()> {
        super::super::clear_channel_session_state(
            &self.http,
            &self.shared,
            &ProviderKind::Claude,
            self.channel_id,
            "!clear",
            super::super::SoftClearNotifyMode::Suppress,
        )
        .await
    }

    async fn admits(&self) -> (bool, Option<String>) {
        let mut state = (Some("stale".to_string()), true, String::new());
        let admitted = native_clear_admits(
            &self.http,
            &self.shared,
            &ProviderKind::Claude,
            self.channel_id,
            &mut state,
        )
        .await;
        (admitted, state.0)
    }

    async fn state(&self) -> NativeClearBoundary {
        native_channel_clear_state(&self.pool, &self.channel_id.get().to_string())
            .await
            .unwrap()
    }

    /// An unresolved native boundary as a clear leaves it when the process dies after commit.
    async fn unresolved(&self) -> ClearTicket {
        let capture = crate::services::tui_prompt_dedupe::native_clear::capture_live_clear;
        let ticket = capture(self.channel_id.get(), &self.tmux).unwrap().ticket;
        let tx = session_transcripts::begin_channel_clear_boundary_tx(&self.pool)
            .await
            .unwrap();
        let key = self.channel_id.get().to_string();
        let value = serde_json::to_value(&ticket).unwrap();
        session_transcripts::finish_native_channel_clear_boundary_tx(tx, &key, &value)
            .await
            .unwrap();
        ticket
    }

    async fn session(&self) -> (Option<String>, bool) {
        let data = self.shared.core.lock().await;
        let session = data.sessions.get(&self.channel_id).unwrap();
        (session.session_id.clone(), session.cleared)
    }

    async fn drop_db(mut self) {
        binding_events::forget_channel_for_tests(self.channel_id.get());
        binding_events::set_test_root(None);
        self.pool.close().await;
        self.db.take().unwrap().drop().await;
    }
}

fn record_on(
    root: &std::path::Path,
    channel_id: ChannelId,
    tmux: &str,
    session: &str,
    cause: BindingCause,
    pending: bool,
) {
    let source = SourceId {
        session_id: session.into(),
        path: root.join(format!("{session}.jsonl")),
        dev: 1,
        ino: session.len() as u64,
    };
    let path = source.path.display().to_string();
    let hook = HookSignal::from_payload("session_start", &serde_json::json!({"source":"clear"}));
    let proposal = Proposal {
        channel_id: channel_id.get(),
        provider: "claude",
        tmux_session: tmux,
        session_id: Some(session),
        path: &path,
        replaced: None,
        cause: CauseSource::Hook(cause),
        hook: Some(&hook),
    };
    tmux_common::with_tmux_source_authority(tmux, |_| {
        if pending {
            binding_events::record_pending(&proposal).unwrap();
        } else {
            binding_events::record_verified(&proposal, &source).unwrap();
        }
    });
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// The clear hook the pane would send after `/clear`, run from the worker's submit.
fn hook_on_submit(fake: &Fake, fixture: &Fixture, pending: bool) {
    let root = fixture.binding_root.path().to_path_buf();
    let (channel_id, tmux) = (fixture.channel_id, fixture.tmux.clone());
    *fake.on_submit.lock().unwrap() = Some(Box::new(move || {
        record_on(
            &root,
            channel_id,
            &tmux,
            "new",
            BindingCause::Clear,
            pending,
        );
    }));
}

#[test]
fn native_clear_keeps_the_pane_and_completes_inside_the_worker_pg() {
    runtime().block_on(async {
        let fixture = Fixture::new(1).await;
        let fake = Arc::new(Fake::default());
        let _on = switch_on_for_tests(fake.clone());
        hook_on_submit(&fake, &fixture, false);
        let channel_id = fixture.channel_id;
        fixture
            .shared
            .overrides
            .model_session_reset_pending
            .insert(channel_id);

        fixture.clear().await.expect("native clear succeeds");

        let key = &fixture.session_key;
        assert_eq!(
            fake.calls(),
            [format!("clear:{key}"), "submit".into(), "save:new".into()],
            "the selector is cleared before `/clear` and no process is reset"
        );
        assert_eq!(fixture.state().await, NativeClearBoundary::Resolved);
        assert_eq!(fixture.session().await, (Some("new".into()), true));
        assert!(
            std::path::Path::new(&fixture.marker()).exists(),
            "the running execution is kept"
        );
        assert!(
            fixture
                .shared
                .overrides
                .model_session_reset_pending
                .contains(&channel_id),
            "a native clear owes the unrestarted process its pending model reset"
        );
        drop(_on);
        fixture.drop_db().await;
    });
}

#[test]
fn native_clear_without_a_clear_hook_falls_back_to_the_managed_reset_pg() {
    runtime().block_on(async {
        let fixture = Fixture::new(2).await;
        let fake = Arc::new(Fake::default());
        let _on = switch_on_for_tests(fake.clone());
        *fake.submitted.lock().unwrap() = Some(NativeClearSubmission::NotSent);
        let channel_id = fixture.channel_id;
        fixture
            .shared
            .overrides
            .model_session_reset_pending
            .insert(channel_id);

        fixture.clear().await.expect("the fallback clear succeeds");

        let key = &fixture.session_key;
        assert_eq!(
            fake.calls(),
            [
                format!("clear:{key}"),
                "submit".into(),
                format!("clear:{key}"),
                format!("reset:{}", fixture.tmux),
            ]
        );
        assert_eq!(fixture.state().await, NativeClearBoundary::Resolved);
        assert!(
            !std::path::Path::new(&fixture.marker()).exists(),
            "the fallback retires the execution before the reset"
        );
        assert!(
            !fixture
                .shared
                .overrides
                .model_session_reset_pending
                .contains(&channel_id)
        );
        drop(_on);
        fixture.drop_db().await;
    });
}

/// Dropping the caller mid-clear neither frees admission early nor skips the completion mark.
#[test]
fn native_clear_marks_completion_before_the_worker_releases_admission_pg() {
    runtime().block_on(async {
        let fixture = Fixture::new(3).await;
        let fake = Arc::new(Fake::default());
        let _on = switch_on_for_tests(fake.clone());
        hook_on_submit(&fake, &fixture, false);
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        *fake.submit_entered.lock().unwrap() = Some(entered_tx);
        *fake.submit_release.lock().unwrap() = Some(release_rx);

        let mut clear = Box::pin(fixture.clear());
        tokio::select! {
            result = &mut clear => panic!("clear finished before submit: {result:?}"),
            _ = entered => {}
        }
        drop(clear);
        let lock = fixture.shared.session_transition_lock(fixture.channel_id);
        // A caller-side completion mark would race the reopened admission below.
        assert!(lock.try_lock().is_err(), "the worker still owns admission");
        release.send(()).unwrap();
        let admitted = lock.lock_owned().await;

        assert_eq!(
            fixture.state().await,
            NativeClearBoundary::Resolved,
            "the completion mark is durable once admission reopens"
        );
        drop(admitted);
        drop(_on);
        fixture.drop_db().await;
    });
}

#[test]
fn held_native_clear_holds_input_until_admission_completes_it_pg() {
    runtime().block_on(async {
        let fixture = Fixture::new(4).await;
        let fake = Arc::new(Fake::default());
        let _on = switch_on_for_tests(fake.clone());
        hook_on_submit(&fake, &fixture, true);
        fake.save_fails.store(true, Ordering::SeqCst);

        assert!(
            fixture.clear().await.is_err(),
            "a held clear is not success"
        );
        assert!(matches!(
            fixture.state().await,
            NativeClearBoundary::Unresolved { .. }
        ));
        assert_eq!(
            fixture.admits().await.0,
            false,
            "a failed selector save keeps input held"
        );

        fake.save_fails.store(false, Ordering::SeqCst);
        assert_eq!(fixture.admits().await, (true, Some("new".into())));
        assert_eq!(fixture.state().await, NativeClearBoundary::Resolved);
        assert!(
            !fake.calls().iter().any(|call| call.starts_with("reset:")),
            "a durable clear is completed without a reset"
        );
        drop(_on);
        fixture.drop_db().await;
    });
}

#[test]
fn switched_off_or_unowned_clear_keeps_the_managed_reset_pg() {
    runtime().block_on(async {
        for case in ["off", "unowned"] {
            let fixture = Fixture::new(if case == "off" { 5 } else { 6 }).await;
            let fake = Arc::new(Fake::default());
            let on = (case == "unowned").then(|| switch_on_for_tests(fake.clone()));
            let unowned = (case == "unowned")
                .then(|| crate::services::tui_o::cutover::test_override::force_channels(&[]));
            let alive = Arc::new(AtomicBool::new(true));
            crate::services::session_backend::insert_process_session(
                fixture.tmux.clone(),
                crate::services::session_backend::SessionHandle::TestProcess {
                    pid: 6_577_200,
                    alive: alive.clone(),
                },
            );

            fixture.clear().await.expect("managed clear succeeds");

            assert!(fake.calls().is_empty(), "{case}: no native effect");
            assert_eq!(fixture.state().await, NativeClearBoundary::Legacy, "{case}");
            assert!(!alive.load(Ordering::SeqCst), "{case}: managed reset ran");
            assert_eq!(fixture.session().await, (None, true), "{case}");
            if case == "off" {
                fixture.pool.close().await;
                assert_eq!(
                    fixture.admits().await,
                    (true, Some("stale".into())),
                    "switched off, admission reads nothing"
                );
            }
            drop((on, unowned));
            fixture.drop_db().await;
        }
    });
}

/// Restart disposition per recovery-table row, through the intake admission entry.
#[test]
fn restart_admission_settles_each_unresolved_native_clear_row_pg() {
    runtime().block_on(async {
        let cases = [
            "commit-missing",
            "raced-launch",
            "marker-absent",
            "pending-durable",
            "source-durable",
            "prior-clear-baseline",
            "resolved",
            "superseded",
            "legacy",
        ];
        for (n, case) in cases.into_iter().enumerate() {
            let fixture = Fixture::new(10 + n as u64).await;
            let fake = Arc::new(Fake::default());
            let _on = switch_on_for_tests(fake.clone());
            if case == "prior-clear-baseline" {
                fixture.record("first", BindingCause::Clear, false);
            }
            if case != "legacy" {
                fixture.unresolved().await;
            }
            match case {
                "raced-launch" => {
                    fixture.record("new", BindingCause::Clear, false);
                    std::fs::write(fixture.marker(), "f".repeat(32)).unwrap();
                }
                "marker-absent" => std::fs::remove_file(fixture.marker()).unwrap(),
                "pending-durable" => fixture.record("new", BindingCause::Clear, true),
                "source-durable" => fixture.record("new", BindingCause::Clear, false),
                "resolved" => {
                    let key = fixture.channel_id.get().to_string();
                    let generation = match fixture.state().await {
                        NativeClearBoundary::Unresolved { generation, .. } => generation,
                        other => panic!("{other:?}"),
                    };
                    session_transcripts::resolve_native_channel_clear(
                        &fixture.pool,
                        &key,
                        generation,
                    )
                    .await
                    .unwrap();
                }
                "superseded" => {
                    let tx = session_transcripts::begin_channel_clear_boundary_tx(&fixture.pool)
                        .await
                        .unwrap();
                    let key = fixture.channel_id.get().to_string();
                    session_transcripts::finish_channel_clear_boundary_tx(tx, &key)
                        .await
                        .unwrap();
                }
                _ => {}
            }
            let before = fixture.state().await;

            let (admitted, session) = fixture.admits().await;

            let key = &fixture.session_key;
            let reset = vec![format!("clear:{key}"), format!("reset:{}", fixture.tmux)];
            let (calls, session_expected) = match case {
                "commit-missing" | "raced-launch" | "marker-absent" | "prior-clear-baseline" => {
                    (reset, None)
                }
                "pending-durable" | "source-durable" => {
                    (vec!["save:new".to_string()], Some("new".to_string()))
                }
                _ => (Vec::new(), Some("stale".to_string())),
            };
            assert!(admitted, "{case}");
            assert_eq!(fake.calls(), calls, "{case}");
            assert_eq!(session, session_expected, "{case}");
            let after = fixture.state().await;
            if calls.is_empty() {
                assert_eq!(after, before, "{case}: preserved rows are not written");
            } else {
                assert_eq!(after, NativeClearBoundary::Resolved, "{case}");
                assert_eq!(fixture.admits().await.0, true, "{case}: settled once");
                assert_eq!(fake.calls(), calls, "{case}: settled once");
            }
            drop(_on);
            fixture.drop_db().await;
        }
    });
}

/// Undecidable restart rows hold input without any effect, and a later `!clear` frees them.
#[test]
fn restart_admission_holds_undecidable_rows_until_a_new_clear_pg() {
    runtime().block_on(async {
        let cases = [
            "marker-unreadable",
            "other-host",
            "other-tmux",
            "pending-moved",
            "save-fails",
            "state-unreadable",
        ];
        for (n, case) in cases.into_iter().enumerate() {
            let fixture = Fixture::new(30 + n as u64).await;
            let fake = Arc::new(Fake::default());
            let _on = switch_on_for_tests(fake.clone());
            let ticket = fixture.unresolved().await;
            match case {
                "marker-unreadable" => {
                    std::fs::remove_file(fixture.marker()).unwrap();
                    std::fs::create_dir(fixture.marker()).unwrap();
                }
                "other-host" | "other-tmux" => {
                    let mut moved = ticket.clone();
                    match case {
                        "other-host" => moved.context.host = Some("other-node".into()),
                        _ => moved.context.tmux_session = format!("{}-other", fixture.tmux),
                    }
                    let tx = session_transcripts::begin_channel_clear_boundary_tx(&fixture.pool)
                        .await
                        .unwrap();
                    let key = fixture.channel_id.get().to_string();
                    let value = serde_json::to_value(&moved).unwrap();
                    session_transcripts::finish_native_channel_clear_boundary_tx(tx, &key, &value)
                        .await
                        .unwrap();
                }
                "pending-moved" => {
                    fixture.record("new", BindingCause::Clear, true);
                    fixture.record("later", BindingCause::Resume, false);
                }
                "save-fails" => {
                    fixture.record("new", BindingCause::Clear, false);
                    fake.save_fails.store(true, Ordering::SeqCst);
                }
                _ => fixture.pool.close().await,
            }

            let (admitted, session) = fixture.admits().await;

            assert!(!admitted, "{case}: input is held");
            assert_eq!(session, Some("stale".into()), "{case}");
            let effects = fake.calls();
            assert!(
                effects
                    .iter()
                    .all(|call| !call.starts_with("reset:") && !call.starts_with("clear:")),
                "{case}: no reset or selector clear: {effects:?}"
            );
            if case == "state-unreadable" {
                fixture.drop_db().await;
                continue;
            }
            assert!(
                matches!(
                    fixture.state().await,
                    NativeClearBoundary::Unresolved { .. }
                ),
                "{case}"
            );
            fake.save_fails.store(false, Ordering::SeqCst);
            *fake.submitted.lock().unwrap() = Some(NativeClearSubmission::NotSent);
            fixture
                .clear()
                .await
                .expect("a held channel still takes `!clear`");
            assert!(
                fixture.admits().await.0,
                "{case}: the new clear frees admission"
            );
            drop(_on);
            fixture.drop_db().await;
        }
    });
}
