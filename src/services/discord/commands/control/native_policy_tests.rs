use super::*;

#[test]
fn admission_outside_the_scoped_channels_reads_no_native_state_pg() {
    runtime().block_on(async {
        let fixture = Fixture::new(50).await;
        let fake = Arc::new(Fake::default());
        let on = switch_on_for_tests(fake.clone());
        fixture.unresolved().await;
        TEST_CHANNELS.with(|cell| *cell.borrow_mut() = Some(vec![fixture.channel_id.get() + 1]));
        fixture.pool.close().await;
        assert_eq!(
            fixture.admits().await,
            (true, Some("stale".into())),
            "outside scope does not read native state"
        );
        assert!(fake.calls().is_empty());
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
            fixture.unresolved().await;
            fixture.record("new", BindingCause::Clear, false);
            assert!(
                !fixture.admits().await.0,
                "the next admission must stay held"
            );
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
