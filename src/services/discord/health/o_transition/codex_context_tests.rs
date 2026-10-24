use super::*;
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
use crate::services::tui_prompt_dedupe::binding_events::{
    self as p5, BindingCause, BindingEvent, BindingEvidence, BindingTarget,
};
use serde_json::json;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

const CHANNEL: u64 = 7;
const ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";
pub(crate) struct Fixture {
    _root: crate::config::TestRuntimeRootGuard,
    _host: crate::config::TestEnvVarGuard,
    _p5: crate::config::TestEnvVarGuard,
    _path: crate::config::TestEnvVarGuard,
    bin: tempfile::TempDir,
    pub(crate) context: BindingContext,
    pub(crate) source: crate::services::tui_o::shadow::SourceId,
}
impl Fixture {
    pub(crate) fn new(herdr: bool) -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let host = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_INSTANCE_ID",
            std::ffi::OsStr::new("bo-test-host"),
        );
        let runtime_root = crate::config::runtime_root().unwrap();
        let p5 = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_TEST_CODEX_BINDING_ROOT",
            &runtime_root,
        );
        p5::set_test_root(Some(&runtime_root));
        let bin = tempfile::tempdir().unwrap();
        let tmux = "AgentDesk-codex-bo-observe";
        std::fs::write(
            bin.path().join("tmux"),
            format!("#!/bin/sh\ncase \"$1\" in list-sessions) echo {tmux} ;; esac\nexit 0\n"),
        )
        .unwrap();
        std::fs::set_permissions(
            bin.path().join("tmux"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let path =
            crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(bin.path());
        let native = runtime_root.join("native");
        std::fs::create_dir_all(&native).unwrap();
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: "a".repeat(32),
            tmux_session: tmux.into(),
            channel_id: Some(CHANNEL),
            owner_runtime_root: runtime_root.display().to_string(),
            host: context::stable_host_identity(),
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(native.clone()),
            first_prompt_digest: None,
            source_policy: (!herdr).then(|| "shadow".into()),
        };
        PreparedIncarnation::create(context.clone()).unwrap();
        let source_path = native.join(format!("rollout-test-{ID}.jsonl"));
        std::fs::write(&source_path, format!("{}\n", json!({"type":"session_meta", "payload": {"id": ID, "cwd": "/test", "source":"cli", "timestamp": chrono::Utc::now().to_rfc3339()}}))).unwrap();
        let meta = std::fs::metadata(&source_path).unwrap();
        let source = crate::services::tui_o::shadow::SourceId {
            session_id: ID.into(),
            path: source_path,
            dev: meta.dev(),
            ino: meta.ino(),
        };
        std::fs::create_dir_all(PathBuf::from(tc::tmux_owner_path(tmux)).parent().unwrap())
            .unwrap();
        std::fs::write(tc::tmux_owner_path(tmux), tc::current_tmux_owner_marker()).unwrap();
        std::fs::write(
            tc::session_temp_path(tmux, "spawn_nonce"),
            &context.execution_nonce,
        )
        .unwrap();
        dedupe::register_tmux_channel(tmux, CHANNEL);
        dedupe::register_tmux_runtime_binding(
            tmux,
            dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: source.path.display().to_string(),
                session_id: Some(ID.into()),
                relay_output_path: None,
                input_fifo_path: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let event = BindingEvent {
            seq: 1,
            channel_id: CHANNEL,
            provider: "codex".into(),
            tmux_session: tmux.into(),
            execution_nonce: Some(context.execution_nonce.clone()),
            old: None,
            new: BindingTarget::Source(source.clone()),
            cause: BindingCause::Startup,
            parent_hint: None,
            evidence: BindingEvidence {
                hook_event: Some("session_start".into()),
                received_at: chrono::Utc::now(),
            },
            committed_at: chrono::Utc::now(),
        };
        std::fs::create_dir_all(runtime_root.join("binding_events")).unwrap();
        std::fs::write(
            runtime_root.join(format!("binding_events/{CHANNEL}.log")),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        Self {
            _root: root,
            _host: host,
            _p5: p5,
            _path: path,
            bin,
            context,
            source,
        }
    }
    pub(crate) fn shared(&self, pool: Option<sqlx::PgPool>) -> Arc<SharedData> {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(pool);
        let pane = self.context.tmux_session.clone();
        shared.tmux_watchers.insert(
            ChannelId::new(CHANNEL),
            crate::services::discord::TmuxWatcherHandle {
                tmux_session_name: pane,
                output_path: self.source.path.display().to_string(),
                paused: Arc::new(false.into()),
                resume_offset: Arc::new(std::sync::Mutex::new(None)),
                cancel: Arc::new(false.into()),
                pause_epoch: Arc::new(0.into()),
                turn_delivered: Arc::new(false.into()),
                last_heartbeat_ts_ms: Arc::new(0.into()),
            },
        );
        shared
    }
    fn observe(&self, prepared: &Prepared) -> Result<model::Inputs, String> {
        tc::with_tmux_source_authority(&prepared.tmux, |authority| {
            prepared.observer.observe(authority)
        })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        dedupe::reset_state_for_tests();
        p5::forget_channel_for_tests(CHANNEL);
        p5::set_test_root(None);
    }
}

#[test]
fn legacy_production_prepare_is_complete_without_pg_and_changes_fail_closed() {
    let fixture = Fixture::new(false);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let prepared = runtime
        .block_on(prepare(
            &HealthRegistry::new(),
            fixture.shared(None),
            CHANNEL,
        ))
        .unwrap();
    let observed = fixture.observe(&prepared).unwrap();
    assert_eq!(observed.coverage, model::Coverage::Complete);
    assert_eq!(observed.current, fixture.source);
    assert_eq!(
        observed.panes,
        [model::Pane {
            name: prepared.tmux.clone(),
            state: model::PaneState::Live
        }]
    );
    std::fs::write(
        tc::session_temp_path(&prepared.tmux, "spawn_nonce"),
        "b".repeat(32),
    )
    .unwrap();
    assert!(fixture.observe(&prepared).is_err());
    std::fs::write(
        tc::session_temp_path(&prepared.tmux, "spawn_nonce"),
        &fixture.context.execution_nonce,
    )
    .unwrap();
    std::fs::write(
        fixture.bin.path().join("tmux"),
        format!(
            "#!/bin/sh\necho {}\necho AgentDesk-codex-unassigned\n",
            prepared.tmux
        ),
    )
    .unwrap();
    assert!(
        runtime
            .block_on(prepare(
                &HealthRegistry::new(),
                fixture.shared(None),
                CHANNEL
            ))
            .is_err()
    );
}

#[test]
fn existing_codex_sweep_reaches_the_actual_actor_with_complete_legacy_evidence() {
    let fixture = Fixture::new(false);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let shared = fixture.shared(None);
        shared.settings.write().await.allowed_channel_ids = vec![CHANNEL];
        let registry = HealthRegistry::new();
        registry.register("codex".into(), shared).await;
        crate::services::tui_o::writer::tests::with_ready_codex(
            fixture.source.clone(),
            CHANNEL,
            |ready| async {
                let reply =
                    super::super::automatic::codex_sweep_for_test(&registry, &ready, CHANNEL)
                        .await
                        .unwrap();
                assert!(reply.refused.is_none(), "{reply:?}");
                let diagnostics =
                    serde_json::to_value(reply.diagnostics.as_ref().unwrap()).unwrap();
                assert!(
                    diagnostics["codex"]["Complete"].is_object(),
                    "{diagnostics}"
                );
                assert!(
                    matches!(reply.scope, Some(Err(_))),
                    "Codex Apply scope stays unavailable"
                );
            },
        )
        .await;
    });
}

#[test]
fn pg_configured_herdr_production_prepare_is_complete_and_extra_pane_is_unknown() {
    let fixture = Fixture::new(true);
    let _node =
        crate::config::session_hosts::force_for_test(Some("test-node"), &[(CHANNEL, "test-node")]);
    let rig = crate::services::session_host::herdr_socket_rig_tests::HerdrRig::start();
    rig.show_panes(&["w1-1"]);
    let _registry = rig.registry_as_on_this_thread(
        "test-node",
        "test-node-endpoint",
        rig.socket(),
        "agentdesk",
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let shared = fixture.shared(Some(pool.clone()));
        let mut owner = crate::db::dispatched_sessions::hosted_execution::tests::owner(&CHANNEL.to_string());
        owner.provider = "codex".into(); owner.discord_token_hash = shared.token_hash.clone();
        owner.logical_key = fixture.context.tmux_session.clone(); owner.runtime_root = fixture.context.owner_runtime_root.clone();
        let mut row = crate::db::dispatched_sessions::hosted_execution::tests::record(&owner, &fixture.context.execution_nonce, crate::db::dispatched_sessions::hosted_execution::HostedState::Bound);
        let location = row.location.as_mut().unwrap();
        location.pane_id = "w1-1".into(); location.endpoint_config_key = "test-node-endpoint".into(); location.socket_addr = rig.socket().display().to_string();
        row.expected.as_mut().unwrap().binding_provider = "codex".into();
        let key = format!("codex/{}/test-node:{}", shared.token_hash, fixture.context.tmux_session);
        sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, 'codex', 'idle', 'discord_channel', $2, $3, $4)")
            .bind(key).bind(&shared.token_hash).bind(CHANNEL.to_string()).bind(serde_json::to_value(&row).unwrap()).execute(&pool).await.unwrap();
        let prepared = prepare(&HealthRegistry::new(), shared.clone(), CHANNEL).await.unwrap();
        shared.settings.write().await.allowed_channel_ids = vec![CHANNEL];
        let health = HealthRegistry::new();
        health.register("codex".into(), shared.clone()).await;
        crate::services::tui_o::writer::tests::with_ready_codex(fixture.source.clone(), CHANNEL, |ready| async {
            let reply = super::super::automatic::codex_sweep_for_test(&health, &ready, CHANNEL).await.unwrap();
            assert!(reply.refused.is_none(), "{reply:?}");
            let diagnostics = serde_json::to_value(reply.diagnostics.as_ref().unwrap()).unwrap();
            assert!(diagnostics["codex"]["Complete"].is_object(), "{diagnostics}");
        }).await;
        assert!(matches!(fixture.observe(&prepared).unwrap().target.host_path, model::HostPath::Herdr { .. }));
        rig.show_panes(&["w1-1", "w1-2"]);
        assert!(prepare(&HealthRegistry::new(), shared, CHANNEL).await.is_err());
        pool.close().await;
        drop(db);
    });
    assert!(rig.sends().is_empty());
}
