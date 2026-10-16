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
use crate::services::tui_prompt_dedupe::native_clear::ClearTicket;

/// Records every native effect in order.
#[derive(Default)]
pub(in crate::services::discord) struct Fake {
    calls: Mutex<Vec<String>>,
    pub(in crate::services::discord) save_fails: AtomicBool,
    composer: Mutex<Option<Option<String>>>,
    /// What a pane would have answered to a typed `/clear`; a user clear no longer types one.
    pub(in crate::services::discord) submitted:
        Mutex<Option<crate::services::claude_tui::host_input::NativeClearSubmission>>,
}

impl Fake {
    pub(in crate::services::discord) fn calls(&self) -> Vec<String> {
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
    fn composer_empty(&self, _: &str, _: Instant) -> bool {
        self.composer
            .lock()
            .unwrap()
            .as_ref()
            .map_or(true, |capture| {
                capture.as_ref().is_some_and(|capture| {
                    crate::services::claude_tui::host_input::native_clear_composer_empty(capture)
                })
            })
    }
    fn reset_process(&self, tmux: &str) {
        self.note(format!("reset:{tmux}"));
    }
}

pub(in crate::services::discord) struct Fixture {
    db: Option<crate::dispatch::test_support::DispatchPostgresTestDb>,
    pub(in crate::services::discord) pool: sqlx::PgPool,
    pub(in crate::services::discord) shared: Arc<SharedData>,
    pub(in crate::services::discord) http: Arc<serenity::Http>,
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) tmux: String,
    session_key: String,
    binding_root: tempfile::TempDir,
    _o: crate::services::tui_o::cutover::test_override::ChannelsGuard,
    _config: crate::config::TestEnvVarGuard,
    _host: crate::config::TestEnvVarGuard,
    _root: crate::config::TestEnvVarGuard,
    _root_dir: tempfile::TempDir,
    _env_lock: Option<crate::config::test_env_lock::SharedTestEnvLockGuard>,
}

impl Fixture {
    pub(in crate::services::discord) async fn new(n: u64) -> Self {
        let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        Self::new_locked(n, Some(lock)).await
    }

    pub(in crate::services::discord) async fn new_locked(
        n: u64,
        env_lock: Option<crate::config::test_env_lock::SharedTestEnvLockGuard>,
    ) -> Self {
        let root_dir = tempfile::tempdir().unwrap();
        // A config in the scratch root keeps node identity off any machine-wide config.
        let config = root_dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let data = serde_json::to_string(&root_dir.path().join("data")).unwrap();
        let yaml =
            format!("server: {{}}\ndata:\n  dir: {data}\ncluster: {{instance_id: test-node}}\n");
        std::fs::write(config.join("agentdesk.yaml"), yaml).unwrap();
        let root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root_dir.path(),
        );
        let host = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_INSTANCE_ID",
            "test-node".as_ref(),
        );
        let explicit_config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &config.join("agentdesk.yaml"),
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
            first_prompt_digest: None,
            source_policy: None,
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
            _config: explicit_config,
            _host: host,
            _root: root,
            _root_dir: root_dir,
            _env_lock: env_lock,
        };
        fixture.record("old", BindingCause::Startup, false);
        fixture
    }

    pub(in crate::services::discord) fn record(
        &self,
        session: &str,
        cause: BindingCause,
        pending: bool,
    ) {
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

    async fn clear(&self) -> anyhow::Result<super::super::ClearReport> {
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
            &mut false,
        )
        .await;
        (admitted, state.0)
    }

    pub(in crate::services::discord) async fn state(&self) -> NativeClearBoundary {
        native_channel_clear_state(&self.pool, &self.channel_id.get().to_string())
            .await
            .unwrap()
    }

    /// An unresolved native boundary as a clear leaves it when the process dies after commit.
    pub(in crate::services::discord) async fn unresolved(&self) -> ClearTicket {
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

    pub(in crate::services::discord) async fn drop_db(mut self) {
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

pub(in crate::services::discord) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// A switched-on Claude channel's clear types no `/clear`: it ends the session like any other,
/// and its boundary supersedes an unresolved native one, so admission no longer holds input.
#[test]
fn a_switched_on_clear_ends_the_session_and_frees_an_unresolved_native_boundary_pg() {
    runtime().block_on(async {
        let fixture = Fixture::new(1).await;
        let fake = Arc::new(Fake::default());
        let _on = switch_on_for_tests(fake.clone());
        fixture.unresolved().await;
        assert!(matches!(
            fixture.state().await,
            NativeClearBoundary::Unresolved { .. }
        ));
        let alive = Arc::new(AtomicBool::new(true));
        crate::services::session_backend::insert_process_session(
            fixture.tmux.clone(),
            crate::services::session_backend::SessionHandle::TestProcess {
                pid: 6_577_201,
                alive: alive.clone(),
            },
        );

        fixture.clear().await.expect("the clear commits");

        assert_eq!(fake.calls(), Vec::<String>::new(), "no native effect");
        assert!(!alive.load(Ordering::SeqCst), "the session's process ends");
        assert_eq!(fixture.session().await, (None, true));
        assert_eq!(fixture.state().await, NativeClearBoundary::Superseded);
        let key = fixture.channel_id.get().to_string();
        let record = session_transcripts::native_channel_clear_record(&fixture.pool, &key);
        let record = record
            .await
            .unwrap()
            .expect("the earlier native record stays readable");
        assert!(record.superseded && !record.resolved, "{record:?}");
        assert_eq!(fixture.admits().await, (true, Some("stale".into())));
        drop(_on);
        fixture.drop_db().await;
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
            "input-cutoff",
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
                "other-host" | "other-tmux" | "input-cutoff" => {
                    let mut moved = ticket.clone();
                    match case {
                        "other-host" => moved.context.host = Some("other-node".into()),
                        "other-tmux" => {
                            moved.context.tmux_session = format!("{}-other", fixture.tmux)
                        }
                        // A ledger clear's cutoff is the input runtime's to settle, never native's.
                        _ => {
                            let cutoff =
                                crate::services::tui_prompt_dedupe::native_clear::InputCutoff {
                                    ledger_generation: 0,
                                    ledger_seq: 1,
                                    affected_keys: vec![11],
                                };
                            moved.input = Some(cutoff);
                        }
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

#[path = "native_policy_tests.rs"]
mod policy_tests;
