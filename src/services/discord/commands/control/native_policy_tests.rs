use super::*;

#[test]
fn scoped_native_clear_selects_only_listed_channels_and_skips_outside_state_pg() {
    runtime().block_on(async {
        let fixture = Fixture::new(50).await;
        let fake = Arc::new(Fake::default());
        let on = switch_on_for_tests(fake.clone());
        TEST_CHANNELS.with(|cell| *cell.borrow_mut() = Some(vec![fixture.channel_id.get()]));
        hook_on_submit(&fake, &fixture, false);
        fixture.clear().await.unwrap();
        assert!(fake.calls().contains(&"submit".into()));
        let before = session_transcripts::native_channel_clear_record(
            &fixture.pool,
            &fixture.channel_id.get().to_string(),
        )
        .await
        .unwrap()
        .unwrap();
        TEST_CHANNELS.with(|cell| *cell.borrow_mut() = Some(vec![fixture.channel_id.get() + 1]));
        fake.calls.lock().unwrap().clear();
        let alive = Arc::new(AtomicBool::new(true));
        crate::services::session_backend::insert_process_session(
            fixture.tmux.clone(),
            crate::services::session_backend::SessionHandle::TestProcess {
                pid: 6_577_250,
                alive: alive.clone(),
            },
        );
        fixture.clear().await.unwrap();
        assert!(
            fake.calls().is_empty(),
            "outside channel must use only managed effects"
        );
        assert!(!alive.load(Ordering::SeqCst));
        let after = session_transcripts::native_channel_clear_record(
            &fixture.pool,
            &fixture.channel_id.get().to_string(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            (after.generation, after.ticket, after.resolved),
            (before.generation, before.ticket, before.resolved),
            "outside clear preserves the prior native record"
        );
        assert!(
            after.superseded,
            "the managed boundary supersedes the prior native generation"
        );
        fixture.pool.close().await;
        assert_eq!(
            fixture.admits().await,
            (true, Some("stale".into())),
            "outside scope does not read native state"
        );
        TEST_CHANNELS.with(|cell| cell.borrow_mut().take());
        drop(on);
        fixture.drop_db().await;
    });
}

#[test]
fn composer_failure_keeps_native_boundary_unresolved_and_next_input_held_pg() {
    runtime().block_on(async {
        for (n, capture) in [
            Some("Claude Code\n❯ remaining draft\nstatus".to_string()),
            None,
        ]
        .into_iter()
        .enumerate()
        {
            let fixture = Fixture::new(60 + n as u64).await;
            let fake = Arc::new(Fake::default());
            *fake.composer.lock().unwrap() = Some(capture);
            let on = switch_on_for_tests(fake.clone());
            hook_on_submit(&fake, &fixture, false);
            assert!(fixture.clear().await.is_err());
            assert!(
                fake.calls().contains(&"save:new".into()),
                "Y is saved before composer confirmation"
            );
            assert!(
                matches!(
                    fixture.state().await,
                    NativeClearBoundary::Unresolved { .. }
                ),
                "Hold must never resolve the boundary"
            );
            assert!(
                !fixture.admits().await.0,
                "the next admission must stay held"
            );
            assert!(matches!(
                fixture.state().await,
                NativeClearBoundary::Unresolved { .. }
            ));
            *fake.composer.lock().unwrap() = None;
            assert!(fixture.admits().await.0);
            assert_eq!(fixture.state().await, NativeClearBoundary::Resolved);
            drop(on);
            fixture.drop_db().await;
        }
    });
}

#[test]
fn native_fixture_overrides_and_restores_an_inherited_explicit_config_pg() {
    runtime().block_on(async {
        let inherited = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            inherited.path(),
            "server: {}\ncluster: {instance_id: inherited-config}\n",
        )
        .unwrap();
        let inherited_path = inherited.path().to_path_buf();
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &inherited_path,
        );
        let fixture = Fixture::new_locked(70, None).await;
        assert_eq!(
            std::env::var_os("AGENTDESK_CONFIG").unwrap(),
            fixture._root_dir.path().join("config/agentdesk.yaml")
        );
        assert_eq!(
            crate::config::load_graceful()
                .cluster
                .instance_id
                .as_deref(),
            Some("test-node")
        );
        fixture.drop_db().await;
        assert_eq!(
            std::env::var_os("AGENTDESK_CONFIG").unwrap(),
            inherited_path
        );
        drop(env);
    });
}
