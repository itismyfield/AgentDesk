use super::*;
use crate::services::discord::health::legacy_supervision::RetiredForTest;
use crate::services::discord::inflight::o_seed_observation::Guard;
use crate::services::discord::task_supervisor::n4d_spawn_observation::Scope;
use crate::services::discord::task_supervisor::watcher_completion::Outcome;
use crate::services::discord::tmux::n4d2_entry_support as support;
use crate::services::tmux_common as tc;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn assert_decoded_and_joined(
    fixture: &crate::services::discord::tmux::InstallFixture,
    observed: &Guard,
    submitted: &Scope,
) {
    support::until(observed, "watcher initialization", |s| {
        s.watcher_initialization_observed
    })
    .await;
    support::append(fixture, b"{\"type\":\"n4d2-source-observation\"}\n");
    support::until(observed, "first real source chunk", |s| {
        s.decoded_source_chunks > 0 && s.stream_decoder_initialization_observed
    })
    .await;
    let joined = submitted.cancel_and_join_all().await;
    assert_eq!(joined.len(), 1, "{joined:?}");
    assert!(
        joined
            .iter()
            .all(|(_, result)| *result == Outcome::Returned)
    );
    support::assert_zero_loaders(observed);
}

#[test]
fn n4d2_general_restore_installs_without_legacy_state() {
    if !support::isolated(
        module_path!(),
        "n4d2_general_restore_installs_without_legacy_state",
    ) {
        return;
    }
    runtime().block_on(async {
        let _absence = crate::services::discord::health::lock_watcher_absence_for_test().await;
        let (fixture, _discord) = support::fixture(ProviderKind::Claude, 632_520_101).await;
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        support::list_sessions(&[&fixture.session]);
        let row = crate::services::discord::inflight::InflightTurnState::new(
            fixture.provider.clone(),
            fixture.channel.get(),
            Some("n4d2-ignored-seed".into()),
            7,
            103,
            104,
            "stale Legacy prompt".into(),
            Some("legacy-id".into()),
            Some(fixture.session.clone()),
            Some(fixture.output.clone()),
            None,
            0,
        );
        let row_path =
            crate::services::discord::health::legacy_supervision::test_support::seed_backfill_row(
                &row,
            );
        let before =
            crate::services::discord::health::legacy_supervision::test_support::fingerprint(
                &row_path,
            );
        let submitted = Scope::new(&fixture.shared);
        let observed = support::observe(&fixture);
        crate::services::discord::tmux::restore_tmux_watchers(&fixture.http, &fixture.shared).await;
        assert_eq!(
            submitted.submissions(),
            1,
            "restore must submit the real O watcher"
        );
        assert!(fixture.shared.core.lock().await.sessions.is_empty());
        assert!(
            crate::services::discord::mailbox_snapshot(&fixture.shared, fixture.channel)
                .await
                .cancel_token
                .is_none()
        );
        assert_decoded_and_joined(&fixture, &observed, &submitted).await;
        assert_eq!(
            crate::services::discord::health::legacy_supervision::test_support::fingerprint(
                &row_path
            ),
            before
        );
    });
}

#[test]
fn n4d2_boot_api_installs_from_cold_durable_binding() {
    if !support::isolated(
        module_path!(),
        "n4d2_boot_api_installs_from_cold_durable_binding",
    ) {
        return;
    }
    runtime().block_on(async {
        let _absence = crate::services::discord::health::lock_watcher_absence_for_test().await;
        let (fixture, _discord) = support::fixture(ProviderKind::Codex, 632_520_102).await;
        let _retired = RetiredForTest::new("codex", fixture.channel.get());
        assert!(fixture.shared.core.lock().await.sessions.is_empty());
        assert!(fixture.shared.tmux_watchers.get(&fixture.channel).is_none());
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        assert!(
            crate::services::tui_prompt_dedupe::peek_tmux_runtime_binding(&fixture.session)
                .is_none()
        );
        support::list_sessions(&[&fixture.session]);
        let submitted = Scope::new(&fixture.shared);
        let observed = support::observe(&fixture);
        restore_retired_o_watchers(&RetiredOWatcherRestore {
            http: fixture.http.clone(),
            shared: fixture.shared.clone(),
        })
        .await;
        assert_eq!(submitted.submissions(), 1);
        restore_retired_o_watchers(&RetiredOWatcherRestore {
            http: fixture.http.clone(),
            shared: fixture.shared.clone(),
        })
        .await;
        assert_eq!(submitted.submissions(), 1, "live incumbent must be reused");
        assert!(fixture.shared.core.lock().await.sessions.is_empty());
        assert_decoded_and_joined(&fixture, &observed, &submitted).await;
    });
}

#[test]
fn n4d2_legacy_restore_preserves_backfill_and_session_adoption() {
    if !support::isolated(
        module_path!(),
        "n4d2_legacy_restore_preserves_backfill_and_session_adoption",
    ) {
        return;
    }
    runtime().block_on(async {
        let _absence = crate::services::discord::health::lock_watcher_absence_for_test().await;
        let (fixture, _discord) = support::fixture(ProviderKind::Claude, 632_520_106).await;
        let row = crate::services::discord::inflight::InflightTurnState::new(
            fixture.provider.clone(),
            fixture.channel.get(),
            Some("n4d2-legacy".into()),
            7,
            101,
            102,
            "legacy prompt".into(),
            None,
            Some(fixture.session.clone()),
            Some(fixture.output.clone()),
            None,
            0,
        );
        let path =
            crate::services::discord::health::legacy_supervision::test_support::seed_backfill_row(
                &row,
            );
        let before = std::fs::read(&path).unwrap();
        let submitted = Scope::new(&fixture.shared);
        let observed = support::observe(&fixture);
        crate::services::discord::tmux::restore_tmux_watchers(&fixture.http, &fixture.shared).await;
        assert_eq!(
            submitted.submissions(),
            1,
            "Legacy restore must still submit"
        );
        assert!(
            fixture
                .shared
                .core
                .lock()
                .await
                .sessions
                .contains_key(&fixture.channel)
        );
        support::until(&observed, "Legacy watcher initialization", |s| {
            s.watcher_initialization_observed
        })
        .await;
        let joined = submitted.cancel_and_join_all().await;
        assert_eq!(joined.len(), 1);
        assert!(
            joined
                .iter()
                .all(|(_, result)| *result == Outcome::Returned)
        );
        let effects = observed.snapshot();
        assert!(effects.writable_inflight_load_calls > 0, "{effects:?}");
        assert!(effects.compatibility_backfill_attempts > 0, "{effects:?}");
        assert_ne!(
            std::fs::read(path).unwrap(),
            before,
            "Legacy compatibility backfill remains active"
        );
    });
}

#[test]
fn n4d2_restore_orphan_preserves_retired_target_and_kills_legacy_pg() {
    if !support::isolated(
        module_path!(),
        "n4d2_restore_orphan_preserves_retired_target_and_kills_legacy_pg",
    ) {
        return;
    }
    runtime().block_on(async {
        let _absence = crate::services::discord::health::lock_watcher_absence_for_test().await;
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let (fixture, _discord) =
            support::fixture_with_pool(ProviderKind::Claude, 632_520_103, Some(pool.clone())).await;
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        support::pane("busy");
        support::list_sessions(&[&fixture.session]);
        let submitted = Scope::new(&fixture.shared);
        let observed = support::observe(&fixture);
        let pause = crate::services::discord::tmux::n4d2_pause_install_after_host(fixture.channel);
        let restore =
            crate::services::discord::tmux::restore_tmux_watchers(&fixture.http, &fixture.shared);
        tokio::pin!(restore);
        tokio::select! {
            _ = pause.entered.notified() => {},
            _ = &mut restore => panic!("orphan overlap missed the installation barrier"),
        }
        assert!(fixture.shared.tmux_watchers.get(&fixture.channel).is_none());
        support::pane("deadpane");
        crate::services::discord::tmux_reaper::cleanup_orphan_tmux_sessions(&fixture.shared).await;
        let early_kills =
            std::fs::read_to_string(support::root().join("tmux-kills")).unwrap_or_default();
        assert!(
            !early_kills.contains(&fixture.session),
            "orphan must preserve the cold target while O installation waits: {early_kills}"
        );
        support::assert_zero_loaders(&observed);
        support::pane("busy");
        pause.resume.notify_one();
        restore.await;
        assert_eq!(submitted.submissions(), 1);
        assert!(fixture.shared.core.lock().await.sessions.is_empty());
        assert_decoded_and_joined(&fixture, &observed, &submitted).await;

        let legacy = ProviderKind::Claude.build_tmux_session_name("n4d2-legacy-orphan-cc");
        std::fs::write(
            tc::tmux_owner_path(&legacy),
            tc::current_tmux_owner_marker(),
        )
        .unwrap();
        tc::write_tmux_channel_binding(&legacy, fixture.channel.get() + 1).unwrap();
        support::pane("deadpane");
        support::list_sessions(&[&fixture.session, &legacy]);
        crate::services::discord::tmux_reaper::cleanup_orphan_tmux_sessions(&fixture.shared).await;
        let root = crate::config::runtime_root().unwrap();
        let killed = std::fs::read_to_string(root.join("tmux-kills")).unwrap();
        assert!(
            !killed.contains(&fixture.session),
            "retired O target killed: {killed}"
        );
        assert!(
            killed.contains(&legacy),
            "Legacy orphan cleanup must still kill: {killed}"
        );
        assert!(fixture.shared.core.lock().await.sessions.is_empty());
        pool.close().await;
        db.drop().await;
    });
}

#[test]
fn n4d2_restore_retry_overlap_submits_once() {
    if !support::isolated(module_path!(), "n4d2_restore_retry_overlap_submits_once") {
        return;
    }
    runtime().block_on(async {
        let _absence = crate::services::discord::health::lock_watcher_absence_for_test().await;
        let (fixture, _discord) = support::fixture(ProviderKind::Claude, 632_520_104).await;
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        let now = chrono::Utc::now().timestamp();
        crate::services::discord::health::n4d2_arm_absence(&fixture.provider, fixture.channel, now);
        let submitted = Scope::new(&fixture.shared);
        let observed = support::observe(&fixture);
        let pause = crate::services::discord::tmux::n4d2_pause_install_after_host(fixture.channel);
        let restore =
            crate::services::discord::tmux::restore_tmux_watchers(&fixture.http, &fixture.shared);
        tokio::pin!(restore);
        tokio::select! {
            _ = pause.entered.notified() => {},
            _ = &mut restore => panic!("restore missed the production host-await barrier"),
        }
        assert!(
            crate::services::discord::health::retry_pending_watcher_respawn(
                &fixture.registry,
                &fixture.provider,
                std::slice::from_ref(&fixture.shared),
                fixture.channel,
                now,
            )
            .await
        );
        pause.resume.notify_one();
        restore.await;
        assert_eq!(
            submitted.submissions(),
            1,
            "restore+retry must submit exactly once"
        );
        assert!(
            crate::services::discord::health::n4d2_absence(&fixture.provider, fixture.channel)
                .is_none()
        );
        assert_decoded_and_joined(&fixture, &observed, &submitted).await;
    });
}

#[test]
fn n4d2_restore_failed_boot_retains_absence_due_and_direct_resume_reason() {
    if !support::isolated(
        module_path!(),
        "n4d2_restore_failed_boot_retains_absence_due_and_direct_resume_reason",
    ) {
        return;
    }
    runtime().block_on(async {
        use std::io::BufRead;
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Child, Command, Stdio};
        struct Resume(Child);
        impl Drop for Resume {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _absence = crate::services::discord::health::lock_watcher_absence_for_test().await;
        let (fixture, _discord) = support::fixture(ProviderKind::Codex, 632_520_105).await;
        let _retired = RetiredForTest::new("codex", fixture.channel.get());
        let submitted = Scope::new(&fixture.shared);
        let observed = support::observe(&fixture);
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        std::fs::rename(&fixture.output, format!("{}.absent", fixture.output)).unwrap();
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let native = support::root().join("codex/sessions").join(format!("rollout-{id}.jsonl"));
        let bytes = format!("{}\n", serde_json::json!({"type":"session_meta", "payload":{"id":id, "source":"cli", "cwd":"/test"}})).into_bytes();
        std::fs::write(&native, &bytes).unwrap();
        let bin = support::root().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let binary = bin.join("codex");
        std::fs::write(&binary, "#!/bin/sh\nprintf 'ready\\n'\nread -r hold\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut process = Resume(Command::new(binary).args(["resume", &id])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap());
        let mut ready = String::new();
        std::io::BufReader::new(process.0.stdout.take().unwrap()).read_line(&mut ready).unwrap();
        assert_eq!(ready, "ready\n");
        std::fs::write(support::root().join("pane.pid"), process.0.id().to_string()).unwrap();

        crate::services::discord::tmux::restore_tmux_watchers(&fixture.http, &fixture.shared).await;
        let first = crate::services::discord::health::n4d2_absence(&fixture.provider, fixture.channel).unwrap();
        assert_eq!(first.1, 1);
        assert_eq!(first.3, Some(OOnlyInstallReason::RequiresDirectResumeCommit));
        restore_retired_o_watchers(&RetiredOWatcherRestore {
            http: fixture.http.clone(),
            shared: fixture.shared.clone(),
        }).await;
        assert_eq!(crate::services::discord::health::n4d2_absence(&fixture.provider, fixture.channel), Some(first), "boot recall before due must preserve its budget and deadline");
        assert!(crate::services::discord::health::retry_pending_watcher_respawn(
            &fixture.registry, &fixture.provider, std::slice::from_ref(&fixture.shared),
            fixture.channel, first.2,
        ).await);
        let second = crate::services::discord::health::n4d2_absence(&fixture.provider, fixture.channel).unwrap();
        assert_eq!(second.0, first.0);
        assert_eq!(second.1, 2);
        assert_eq!(second.3, first.3);
        assert!(second.2 > first.2);
        assert_eq!(submitted.submissions(), 0, "direct resume may not commit or fall back");
        assert!(fixture.shared.core.lock().await.sessions.is_empty());
        assert!(crate::services::tui_prompt_dedupe::peek_tmux_runtime_binding(&fixture.session).is_none());
        assert!(crate::services::codex_tui::session::read_codex_tui_rollout_marker(&fixture.session).is_none());
        assert_eq!(std::fs::read(native).unwrap(), bytes);
        support::assert_zero_loaders(&observed);
        assert!(submitted.cancel_and_join_all().await.is_empty());
    });
}
