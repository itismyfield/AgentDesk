use super::*;
use crate::config::TestEnvVarGuard;
use crate::services::discord::{mailbox_try_start_turn, make_shared_data_for_tests};
use crate::services::provider_teardown::tests::test_support::FakeTmux;
use crate::services::tui_o::{
    shadow::{ShadowProvider, UnitKey, UnitKind},
    store::{Initialized, OStore, StoreConfig, ledger::LedgerEntry},
};
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::*};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Fixture {
    fake: FakeTmux,
    root: tempfile::TempDir,
    _env: [TestEnvVarGuard; 2],
    context: BindingContext,
    canonical: PathBuf,
    files: Vec<(PathBuf, Vec<u8>)>,
    alive: Arc<AtomicBool>,
    _dedupe_lock: std::sync::MutexGuard<'static, ()>,
    _env_lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl Fixture {
    fn new(policy: &str) -> Self {
        let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
        dedupe::reset_state_for_tests();
        dedupe::binding_events::set_test_root(Some(root.path()));
        let channel = 6_845_002;
        let name = format!("codex-clear-{}", uuid::Uuid::new_v4().simple());
        let tmux = ProviderKind::Codex.build_tmux_session_name(&name);
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: tmux.clone(),
            channel_id: Some(channel),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(root.path().to_owned()),
            first_prompt_digest: None,
            source_policy: Some(policy.into()),
        };
        let prepared = PreparedIncarnation::create(context.clone()).unwrap();
        let tc = crate::services::tmux_common::session_temp_path;
        let nonce = PathBuf::from(tc(&tmux, "spawn_nonce"));
        fs::create_dir_all(nonce.parent().unwrap()).unwrap();
        fs::write(&nonce, &context.execution_nonce).unwrap();
        dedupe::register_tmux_channel(&tmux, channel);
        dedupe::set_codex_delivery_permission_for_tests(
            &context,
            dedupe::CodexDeliveryPermissionForTests::Allowed,
        );
        let marker = PathBuf::from(tc(
            &tmux,
            crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        ));
        fs::write(&marker, br#"{"output_path":"pre-hook","last_offset":12}"#).unwrap();
        let native = PathBuf::from(tc(&tmux, "out"));
        let relay = PathBuf::from(tc(&tmux, "jsonl"));
        fs::write(&native, b"pre-hook native output\n").unwrap();
        fs::write(&relay, b"pre-hook relay output\n").unwrap();
        let fake = FakeTmux::install(&tmux);
        let alive = Arc::new(AtomicBool::new(true));
        crate::services::session_backend::insert_process_session(
            tmux,
            crate::services::session_backend::SessionHandle::TestProcess {
                pid: 6_845_002,
                alive: alive.clone(),
            },
        );
        let files = [nonce, marker, native, relay, prepared.path.clone()]
            .into_iter()
            .map(|path| {
                let bytes = fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        Self {
            fake,
            root,
            _env: env,
            context,
            canonical: prepared.path,
            files,
            alive,
            _dedupe_lock: dedupe_lock,
            _env_lock: env_lock,
        }
    }

    fn channel(&self) -> serenity::ChannelId {
        serenity::ChannelId::new(self.context.channel_id.unwrap())
    }

    async fn shared(&self) -> Arc<SharedData> {
        let shared = make_shared_data_for_tests();
        clear_persist_failure_tests::seed_session(&shared, self.channel()).await;
        {
            let mut core = shared.core.lock().await;
            let session = core.sessions.get_mut(&self.channel()).unwrap();
            session.channel_name = Some(
                self.context
                    .tmux_session
                    .strip_prefix("AgentDesk-codex-")
                    .unwrap()
                    .to_owned(),
            );
            session.history.push(crate::ui::ai_screen::HistoryItem {
                item_type: crate::ui::ai_screen::HistoryType::User,
                content: "history before reset".into(),
            });
        }
        shared
    }

    fn assert_files_and_process_unchanged(&self) {
        for (path, before) in &self.files {
            assert_eq!(fs::read(path).unwrap(), *before, "preserved {path:?}");
        }
        assert!(
            self.alive.load(Ordering::SeqCst),
            "the process is never killed"
        );
        let calls = self.fake.take_calls();
        assert!(
            calls.iter().all(|call| {
                !call.starts_with("kill-session") && !call.starts_with("send-keys")
            }),
            "no kill or next input: {calls:?}"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ =
            crate::services::session_backend::remove_process_session(&self.context.tmux_session);
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
fn codex_verified_clear_before_first_hook_preserves_output_nonce_marker_and_owed() {
    let fixture = Fixture::new("verified");
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, fixture.root.path())
        .unwrap()
        .unwrap();
    let channel = fixture.channel();
    let init = Initialized {
        channel: channel.get(),
        sources: vec![],
        initial_anchor: 101,
        build_digest: "clear-gate-test".into(),
        at: chrono::Utc::now(),
    };
    let era = store
        .begin_era(&[channel.get()], init.at, |_| Ok(init.clone()))
        .unwrap();
    let mut owed = store.open_channel(&era, channel.get()).unwrap().unwrap();
    owed.append_ledger(LedgerEntry::Prepared {
        serial: 1,
        unit_key: UnitKey {
            channel_id: channel.get(),
            provider: ShadowProvider::Codex,
            native_key: "pre-hook-owed".into(),
            kind: UnitKind::Body,
        },
        piece_index: 0,
        payload: "owed before hook".into(),
        anchor_id: 101,
        epoch: 1,
    })
    .unwrap();
    let ledger = fixture
        .root
        .path()
        .join(format!("o_store/{}/ledger.jsonl", channel.get()));
    let owed_before = fs::read(&ledger).unwrap();
    runtime().block_on(async {
        let shared = fixture.shared().await;
        let provider = ProviderKind::Codex;
        let token = Arc::new(CancelToken::new());
        token.bind_unmanaged_session_name(&fixture.context.tmux_session);
        assert!(
            mailbox_try_start_turn(
                &shared,
                channel,
                token.clone(),
                serenity::UserId::new(1),
                serenity::MessageId::new(6_845_004)
            )
            .await
        );
        clear_persist_failure_tests::seed_backlog(&shared, &provider, channel).await;
        let starts = Arc::new(AtomicUsize::new(0));
        let count = starts.clone();
        let _kick = super::super::super::queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
            move |_, _, _, _| {
                count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Some(crate::services::discord::IdleQueueKickoffChannelOutcome {
                        started: false,
                    })
                })
            },
        ));
        let before = shared.mailbox(channel).snapshot().await.active_turn_nonce;
        let http = Arc::new(serenity::Http::new(""));
        let actual = clear_channel_session_state_fenced(
            &http,
            &shared,
            &provider,
            channel,
            "/clear",
            SoftClearNotifyMode::Suppress,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(actual.to_string(), VERIFIED_CODEX_RESET_REFUSAL);
        let public = clear_channel_session_state(
            &http,
            &shared,
            &provider,
            channel,
            "/clear",
            SoftClearNotifyMode::Suppress,
        )
        .await
        .unwrap_err();
        assert_eq!(public.to_string(), VERIFIED_CODEX_RESET_REFUSAL);
        assert_eq!(
            shared.mailbox(channel).snapshot().await.active_turn_nonce,
            before
        );
        assert!(!token.cancelled.load(Ordering::SeqCst));
        assert_eq!(
            clear_persist_failure_tests::queue_len(&shared, channel).await,
            1
        );
        assert_eq!(
            clear_persist_failure_tests::session_state(&shared, channel).await,
            (Some(clear_persist_failure_tests::SESSION_ID.into()), false)
        );
        tokio::task::yield_now().await;
        assert_eq!(
            starts.load(Ordering::SeqCst),
            0,
            "next queue input is never scheduled"
        );
    });
    assert_eq!(fs::read(ledger).unwrap(), owed_before);
    assert_eq!(
        owed.ledger().unresolved().unwrap().1.payload,
        "owed before hook"
    );
    fixture.assert_files_and_process_unchanged();
}

#[test]
fn codex_verified_managed_reset_refuses_history_reset_and_recreation() {
    let fixture = Fixture::new("verified");
    runtime().block_on(async {
        let shared = fixture.shared().await;
        let http = Arc::new(serenity::Http::new(""));
        for (reset, history, recreate) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, true),
        ] {
            let result = reset_channel_provider_state(
                &http,
                &shared,
                &ProviderKind::Codex,
                fixture.channel(),
                "test managed reset",
                reset,
                history,
                recreate,
            )
            .await;
            assert_eq!(
                result,
                ManagedReset::Refused(VERIFIED_CODEX_RESET_REFUSAL.into())
            );
            let core = shared.core.lock().await;
            let history = &core.sessions.get(&fixture.channel()).unwrap().history;
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].content, "history before reset");
            drop(core);
            assert_eq!(
                clear_persist_failure_tests::session_state(&shared, fixture.channel()).await,
                (Some(clear_persist_failure_tests::SESSION_ID.into()), false)
            );
        }
    });
    fixture.assert_files_and_process_unchanged();
}

#[test]
fn codex_verified_low_level_reset_and_unavailable_context_preserve_evidence() {
    let fixture = Fixture::new("verified");
    assert!(!reset_managed_process_session(
        &fixture.context.tmux_session
    ));
    fixture.assert_files_and_process_unchanged();
    fs::write(&fixture.canonical, b"unreadable canonical context").unwrap();
    let marker = crate::services::tmux_common::session_temp_path(
        &fixture.context.tmux_session,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    );
    fs::write(marker, br#"{"codex_ownership":{"proof_seq":1}}"#).unwrap();
    runtime().block_on(async {
        let shared = fixture.shared().await;
        // The channel mirror keeps the gate even if its Discord session row was lost.
        shared.core.lock().await.sessions.remove(&fixture.channel());
        assert_eq!(
            verified_codex_reset_refusal(&shared, &ProviderKind::Codex, fixture.channel()).await,
            Some(VERIFIED_CODEX_RESET_REFUSAL)
        );
    });
    assert!(!reset_managed_process_session(
        &fixture.context.tmux_session
    ));
    assert!(fixture.alive.load(Ordering::SeqCst));
}

#[test]
fn legacy_codex_and_claude_clear_admission_remains_open() {
    let fixture = Fixture::new("legacy");
    runtime().block_on(async {
        let shared = fixture.shared().await;
        assert_eq!(
            verified_codex_reset_refusal(&shared, &ProviderKind::Codex, fixture.channel()).await,
            None
        );
        assert_eq!(
            verified_codex_reset_refusal(&shared, &ProviderKind::Claude, fixture.channel()).await,
            None
        );
    });
    assert!(reset_managed_process_session(&fixture.context.tmux_session));
    assert!(
        !fixture.alive.load(Ordering::SeqCst),
        "legacy process reset still happens"
    );
}
