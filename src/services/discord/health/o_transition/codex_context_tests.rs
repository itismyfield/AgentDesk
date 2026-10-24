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
    context: BindingContext,
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
            format!("#!/bin/sh\n[ \"$1\" = -u ] && shift\ncase \"$1\" in list-sessions) echo {tmux} ;; esac\nexit 0\n"),
        )
        .unwrap();
        std::fs::set_permissions(
            bin.path().join("tmux"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let path =
            crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(bin.path());
        assert_eq!(
            crate::services::platform::tmux::list_session_names().unwrap(),
            [tmux]
        );
        let native = runtime_root.join("native");
        std::fs::create_dir_all(&native).unwrap();
        let native = native.canonicalize().unwrap();
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
fn codex_prepare_unknown_preserves_nonce_component_in_stats_and_log() {
    struct Log(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let fixture = Fixture::new(false);
    std::fs::remove_file(tc::session_temp_path(
        &fixture.context.tmux_session,
        "spawn_nonce",
    ))
    .unwrap();
    let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = recorded.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || Log(writer.clone()))
        .finish();
    let _tracing = tracing::subscriber::set_default(subscriber);
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
            |ready| async move {
                use super::super::automatic::test_support;
                assert!(
                    test_support::codex_sweep_for_test(&registry, &ready, CHANNEL)
                        .await
                        .is_err()
                );
                assert_eq!(
                    test_support::codex_skips_for_test(),
                    std::collections::BTreeMap::from([(
                        "codex_context_unavailable:nonce_marker".into(),
                        1
                    )])
                );
            },
        )
        .await;
    });
    let text = String::from_utf8(recorded.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("[tui_o] Codex retention observation unknown"),
        "{text}"
    );
    assert!(
        text.contains("nonce_marker") && text.contains("channel=7"),
        "{text}"
    );
    assert!(text.contains("current spawn nonce unavailable"), "{text}");
}

#[tokio::test(start_paused = true)]
async fn codex_sweep_timeout_keeps_later_observations_running() {
    let fixture = Fixture::new(false);
    let registry = HealthRegistry::new();
    let shared = fixture.shared(None);
    shared.settings.write().await.allowed_channel_ids = vec![CHANNEL];
    registry.register("codex".into(), shared).await;
    crate::services::tui_o::writer::tests::with_blocked_codex(
        fixture.source.clone(),
        |ready, _suspend| async move {
            use super::super::automatic::test_support;
            for _ in 0..2 {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(90),
                    test_support::codex_sweep_for_test(&registry, &ready, CHANNEL),
                )
                .await
                .expect("Codex sweep must finish a stalled Inspect before its outer deadline");
                assert_eq!(result.unwrap_err(), "codex_inspect_timeout");
                assert_eq!(
                    test_support::codex_skips_for_test(),
                    std::collections::BTreeMap::from([("codex_inspect_timeout".into(), 1)])
                );
            }
        },
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn codex_sweep_classifies_existing_and_inflight_suspension() {
    let fixture = Fixture::new(false);
    let registry = HealthRegistry::new();
    let shared = fixture.shared(None);
    shared.settings.write().await.allowed_channel_ids = vec![CHANNEL];
    registry.register("codex".into(), shared).await;
    crate::services::tui_o::writer::tests::with_blocked_codex(
        fixture.source.clone(),
        |ready, suspend| async move {
            use super::super::automatic::test_support;
            // The in-flight request was prepared against the old, unsuspended scope.
            let sweep = test_support::codex_sweep_for_test(&registry, &ready, CHANNEL);
            tokio::pin!(sweep);
            assert!(futures::poll!(&mut sweep).is_pending());
            let mut refused = false;
            for _ in 0..10000 {
                if suspend() {
                    refused = true;
                    break;
                }
                tokio::task::yield_now().await;
                assert!(futures::poll!(&mut sweep).is_pending());
            }
            assert!(
                refused,
                "the sweep reached the mailbox before fresh suspension"
            );
            let result = tokio::time::timeout(std::time::Duration::from_secs(90), &mut sweep)
                .await
                .expect("inflight suspension must not stall the sweep");
            assert_eq!(result.unwrap_err(), "codex_suspended_sources");
            assert_eq!(
                test_support::codex_skips_for_test(),
                std::collections::BTreeMap::from([("codex_suspended_sources".into(), 1)])
            );
            // A later tick must report policy suspension before any prepare I/O.
            std::fs::remove_file(tc::session_temp_path(
                &fixture.context.tmux_session,
                "spawn_nonce",
            ))
            .unwrap();
            assert!(
                test_support::codex_sweep_for_test(&registry, &ready, CHANNEL)
                    .await
                    .is_err()
            );
            assert_eq!(
                test_support::codex_skips_for_test(),
                std::collections::BTreeMap::from([("codex_suspended_sources".into(), 1)])
            );
        },
    )
    .await;
}

#[test]
fn codex_fixture_pins_identity_when_inherited_host_source_is_missing() {
    let fixture = Fixture::new(false);
    let _missing = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_INSTANCE_ID",
        std::ffi::OsStr::new(""),
    );
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
        .expect("fixture must supply explicit identity without the inherited host source");
    assert_eq!(
        fixture.observe(&prepared).unwrap().target.host,
        "bo-test-host"
    );
}

#[test]
fn codex_prepare_without_explicit_identity_refuses_host_identity() {
    let fixture = Fixture::new(false);
    let config = fixture.bin.path().join("no-identity.yaml");
    std::fs::write(&config, "server: {}\n").unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    let _missing = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_INSTANCE_ID",
        std::ffi::OsStr::new(""),
    );
    assert_eq!(context::stable_host_identity(), None);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let unavailable = runtime
        .block_on(prepare(
            &HealthRegistry::new(),
            fixture.shared(None),
            CHANNEL,
        ))
        .err()
        .expect("missing explicit identity must remain Unknown");
    assert_eq!(unavailable.component.as_str(), "host_identity");
    assert_eq!(unavailable.reason, "local host identity unavailable");
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
    assert_eq!(observed.coverage, Ok(()));
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
    let mut pending = fixture.context.clone();
    pending.execution_nonce = "b".repeat(32);
    pending.created_at = chrono::Utc::now();
    let pending = PreparedIncarnation::create(pending).unwrap();
    assert!(
        fixture.observe(&prepared).is_err(),
        "new prepared context changes census"
    );
    std::fs::remove_file(pending.path).unwrap();
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
            |ready| async move {
                let reply = super::super::automatic::test_support::codex_sweep_for_test(
                    &registry, &ready, CHANNEL,
                )
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
        crate::services::tui_o::writer::tests::with_ready_codex(fixture.source.clone(), CHANNEL, |ready| async move {
            let reply = super::super::automatic::test_support::codex_sweep_for_test(&health, &ready, CHANNEL).await.unwrap();
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

async fn assert_actual_codex_sweep(fixture: &Fixture) {
    let shared = fixture.shared(None);
    shared.settings.write().await.allowed_channel_ids = vec![CHANNEL];
    let registry = HealthRegistry::new();
    registry.register("codex".into(), shared).await;
    crate::services::tui_o::writer::tests::with_ready_codex(
        fixture.source.clone(),
        CHANNEL,
        |ready| async move {
            let reply = super::super::automatic::test_support::codex_sweep_for_test(
                &registry, &ready, CHANNEL,
            )
            .await
            .unwrap();
            assert!(reply.refused.is_none(), "{reply:?}");
            let diagnostics = serde_json::to_value(reply.diagnostics.as_ref().unwrap()).unwrap();
            assert!(
                diagnostics["codex"]["Complete"].is_object(),
                "{diagnostics}"
            );
            assert!(matches!(reply.scope, Some(Err(_))));
        },
    )
    .await;
}

#[test]
fn verified_fresh_durable_claim_and_marker_reach_actual_codex_sweep() {
    let mut fixture = Fixture::new(false);
    let predecessor = fixture.source.clone();
    let fresh_id = "019e660d-4859-7522-9cee-8ba7c4e7c744";
    let mut next = fixture.context.clone();
    next.execution_nonce = "b".repeat(32);
    next.created_at = chrono::Utc::now();
    next.source_policy = Some("verified".into());
    PreparedIncarnation::create(next.clone()).unwrap();
    fixture.context = next;
    std::fs::write(
        tc::session_temp_path(&fixture.context.tmux_session, "spawn_nonce"),
        &fixture.context.execution_nonce,
    )
    .unwrap();
    let fresh_path = fixture
        .context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-fresh-{fresh_id}.jsonl"));
    std::fs::write(&fresh_path, format!("{}\n", json!({"type":"session_meta", "payload": {"id": fresh_id, "cwd": "/test", "source":"cli", "timestamp": chrono::Utc::now().to_rfc3339()}}))).unwrap();
    let meta = std::fs::metadata(&fresh_path).unwrap();
    fixture.source = crate::services::tui_o::shadow::SourceId {
        session_id: fresh_id.into(),
        path: fresh_path,
        dev: meta.dev(),
        ino: meta.ino(),
    };
    let claim = p5::codex::Claim {
        session_id: fresh_id.into(),
        path: Some(fixture.source.path.clone()),
        evidence: p5::codex::ClaimEvidence::NativeHook {
            event: "session_start".into(),
            source: Some("startup".into()),
            first_prompt_digest: None,
        },
    };
    let fold = tc::with_tmux_source_authority(&fixture.context.tmux_session, |_| {
        p5::codex::commit_claim(
            &fixture.context,
            &claim,
            p5::codex::Decision::Verified(fixture.source.clone()),
            || Ok(()),
        )
        .unwrap()
    });
    let proof = fold
        .verified
        .expect("a unique native source may establish fresh verified ownership");
    let history = p5::codex::observe_channel(CHANNEL, census::LIMIT).unwrap();
    assert_eq!(history.events.len(), 2);
    assert_eq!(history.events[0].new, BindingTarget::Source(predecessor));
    assert_eq!(
        history.events[1].execution_nonce.as_deref(),
        Some(fixture.context.execution_nonce.as_str())
    );
    assert_eq!(
        history.events[1].old, None,
        "the actual initial typed proof has no previous ownership source"
    );
    assert_eq!(
        history.events[1].new,
        BindingTarget::Source(fixture.source.clone())
    );
    let marker = tc::session_temp_path(
        &fixture.context.tmux_session,
        tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    );
    std::fs::write(&marker, serde_json::to_vec(&json!({"rollout_path": proof.source.path, "session_id": fresh_id,
        "codex_ownership": {"execution_nonce": fixture.context.execution_nonce, "proof_seq": proof.seq,
            "dev": proof.source.dev, "ino": proof.source.ino}})).unwrap()).unwrap();
    dedupe::register_launched_tmux_runtime_binding(
        &fixture.context.tmux_session,
        dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: fixture.source.path.display().to_string(),
            session_id: Some(fresh_id.into()),
            relay_output_path: None,
            input_fifo_path: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let log = crate::config::runtime_root()
        .unwrap()
        .join(format!("binding_events/{CHANNEL}.log"));
    let before = (
        std::fs::read(&log).unwrap(),
        std::fs::read(&marker).unwrap(),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(assert_actual_codex_sweep(&fixture));
    assert_eq!(
        (
            std::fs::read(&log).unwrap(),
            std::fs::read(&marker).unwrap()
        ),
        before
    );
    std::fs::write(&marker, "{}").unwrap();
    assert!(
        runtime
            .block_on(prepare(
                &HealthRegistry::new(),
                fixture.shared(None),
                CHANNEL
            ))
            .is_err(),
        "verified source cannot become Complete after its consumption marker is withheld"
    );
}

#[test]
fn same_source_resume_keeps_p5_bytes_and_reaches_actual_codex_sweep() {
    let mut fixture = Fixture::new(false);
    let root = crate::config::runtime_root().unwrap();
    let log = root.join(format!("binding_events/{CHANNEL}.log"));
    let before = std::fs::read(&log).unwrap();
    let mut resumed = fixture.context.clone();
    resumed.execution_nonce = "b".repeat(32);
    resumed.created_at = chrono::Utc::now();
    resumed.launch_mode = "resume".into();
    resumed.expected_native_session_id = Some(ID.into());
    PreparedIncarnation::create(resumed.clone()).unwrap();
    fixture.context = resumed;
    std::fs::write(
        tc::session_temp_path(&fixture.context.tmux_session, "spawn_nonce"),
        &fixture.context.execution_nonce,
    )
    .unwrap();
    dedupe::register_launched_tmux_runtime_binding(
        &fixture.context.tmux_session,
        dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: fixture.source.path.display().to_string(),
            session_id: Some(ID.into()),
            relay_output_path: None,
            input_fifo_path: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    assert_eq!(
        std::fs::read(&log).unwrap(),
        before,
        "same-source resume must not synthesize a P5 row"
    );
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
    let inputs = fixture.observe(&prepared).unwrap();
    assert!(
        inputs
            .events
            .iter()
            .all(|event| event.execution_nonce.as_deref()
                != Some(fixture.context.execution_nonce.as_str()))
    );
    assert_eq!(inputs.current, fixture.source);
    runtime.block_on(assert_actual_codex_sweep(&fixture));
    assert_eq!(
        std::fs::read(&log).unwrap(),
        before,
        "observation must leave the no-op resume log unchanged"
    );
}
