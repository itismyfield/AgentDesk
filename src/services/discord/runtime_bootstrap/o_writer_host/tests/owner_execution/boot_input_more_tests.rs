//! Append as a child of boot_input_tests; all cases enter real live dispatch.
use super::*;
use crate::services::tui_o::ownership::OwnershipGate;
use std::pin::Pin;

struct Fixture {
    shared: Arc<SharedData>,
    channel: u64,
    message: u64,
    global: Arc<OwnershipGate>,
    local: Option<Arc<OwnershipGate>>,
    release: Arc<tokio::sync::Semaphore>,
    handles: Vec<host::ManagedWriterHandle>,
    discord: Discord,
    effects: Arc<AtomicUsize>,
    checkpoints: Arc<AtomicUsize>,
}
impl Fixture {
    fn request(&self) -> IntakeRequest {
        IntakeRequest {
            intake_outbox_id: None,
            channel_id: serenity::ChannelId::new(self.channel),
            user_msg_id: serenity::MessageId::new(self.message),
            source_message_ids: vec![],
            busy_followup_retry_user_msg_id: serenity::MessageId::new(self.message),
            request_owner: serenity::UserId::new(7),
            request_owner_name: "boot-user".into(),
            user_text: "one boot original".into(),
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            merge_consecutive: false,
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: Some(false),
            turn_kind: TurnKind::Foreground,
            preserve_on_cancel: true,
        }
    }
    async fn live(&self, provider: ProviderKind) -> Result<(), crate::services::discord::Error> {
        let checkpoint = self.checkpoints.clone();
        let deps = crate::services::discord::router::IntakeDeps {
            http: &self.discord.http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared: &self.shared,
            token: "test-token",
        };
        // Required cfg(test) adapter described in impl3-more-hooks.md; no fake admission.
        herdr_consent::dispatch_boot_with_provider_for_tests(
            &deps,
            provider,
            self.request(),
            move || {
                checkpoint.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
    }
    fn disk(&self) -> Vec<crate::services::turn_orchestrator::Intervention> {
        crate::services::turn_orchestrator::load_channel_pending_queue_checked(
            &ProviderKind::Claude,
            &self.shared.token_hash,
            serenity::ChannelId::new(self.channel),
        )
        .unwrap()
        .0
    }
    fn assert_refused(&self) {
        assert_eq!(self.checkpoints.load(Ordering::SeqCst), 0);
        assert_eq!(self.effects.load(Ordering::SeqCst), 0);
        assert!(
            self.discord
                .posts
                .lock()
                .unwrap()
                .iter()
                .any(|body| body["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("⛔"))),
            "an actual Discord refusal ACK is recorded"
        );
    }
}

async fn scenario(
    test: &'static str,
    channel: u64,
    owner: &'static str,
    run: impl for<'a> FnOnce(&'a mut Fixture) -> Pin<Box<dyn Future<Output = ()> + 'a>>,
) {
    if !cutover::test_override::isolated_binding_case(&format!("{}::{test}", module_path!())) {
        return;
    }
    let root = crate::config::TestRuntimeRootGuard::new();
    let routing_mode = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_INTAKE_ROUTING_MODE",
        std::ffi::OsStr::new("disabled"),
    );
    let config_path = root.path().join("isolated-boot.json");
    std::fs::write(&config_path, serde_json::to_vec(&json!({
        "server": {},
        "cluster": {"enabled": true, "instance_id": "mac-a", "gateway_preferred_instance_id": "mac-a"},
        "agents": [{"id": "custody-agent", "name": "fixture", "channels": {"claude": {"id": channel.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [channel]}}
    })).unwrap()).unwrap();
    let config_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config_path,
    );
    let host_mapping = crate::config::session_hosts::force_for_test(Some("mac-a"), &[]);
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let selection = select(&ProviderKind::Claude, channel, "mac-a", "mac-a");
    execution::tests::admission(&pool, "claude", channel, owner).await;
    let result = execution::read(&pool, "claude", channel, "mac-a", || Ok(false)).await;
    let local = result
        .as_ref()
        .ok()
        .and_then(|permit| permit.as_ref())
        .map(|p| p.gate());
    let snapshot = cutover::boot_snapshot().unwrap();
    match result {
        Ok(Some(permit)) => snapshot.install_execution(channel, Some(Ok(permit))),
        Err(reason) => snapshot.install_execution(channel, Some(Err(reason))),
        Ok(None) => panic!("fixture must be a managed PG channel"),
    }
    let global = crate::services::tui_o::ownership::gate("claude");
    global.acquired();
    if let Some(local) = &local {
        assert!(!Arc::ptr_eq(local, &global));
        assert!(local.current().epoch().is_some());
    }
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
        .unwrap()
        .unwrap();
    store
        .begin_fixture_era(&[channel], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                policy_parent: None,
                channel,
                sources: vec![],
                initial_anchor: 100,
                build_digest: "boot-more-test".into(),
                at: chrono::Utc::now(),
            })
        })
        .unwrap();
    let (registry, shared) = runtime(&pool, ProviderKind::Claude, channel, "gateway").await;
    let server = gateway_context(&shared, channel).await;
    let discord = Discord::new().await;
    let ready = host::process_readiness();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    struct Release(Arc<tokio::sync::Semaphore>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.add_permits(1);
        }
    }
    let release_on_panic = Release(release.clone());
    let parts = parts_with_rest(
        &shared,
        &ProviderKind::Claude,
        None,
        Some(discord.http.clone()),
    );
    let handles = host::start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership(),
        || host::HostParts {
            io: Arc::new(Paused {
                io: parts.io,
                entered: entered.clone(),
                release: release.clone(),
                once: AtomicBool::new(false),
            }),
            runtime_root: parts.runtime_root,
            gate: parts.gate,
            readiness: ready.clone(),
        },
    );
    if local.is_some() {
        assert_eq!(handles.len(), 1);
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        assert!(!ready.accepts(channel));
    } else {
        assert!(
            handles.is_empty(),
            "remote PG owner cannot start a local host"
        );
    }
    let effects = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(tokio::sync::Notify::new());
    let effect_probe =
        herdr_consent::boot_input_effect_probe_for_tests(effects.clone(), completed.clone());
    let pane_probe = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    let mut f = Fixture {
        shared,
        channel,
        message: ((chrono::Utc::now().timestamp_millis() as u64 - 1_420_070_400_000) << 22) | 1,
        global,
        local,
        release,
        handles,
        discord,
        effects,
        checkpoints: Arc::new(AtomicUsize::new(0)),
    };
    run(&mut f).await;
    f.release.add_permits(1);
    for handle in f.handles.drain(..) {
        handle.stop_and_join().await.unwrap();
    }
    server.abort();
    drop((
        pane_probe,
        effect_probe,
        selection,
        host_mapping,
        routing_mode,
        config_guard,
        release_on_panic,
        registry,
    ));
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_lease_lost_refuses_live_without_custody_or_checkpoint_pg() {
    scenario(
        "writer_boot_lease_lost_refuses_live_without_custody_or_checkpoint_pg",
        73_369_521,
        "mac-a",
        |f| {
            Box::pin(async move {
                f.global.lost();
                f.live(ProviderKind::Claude).await.unwrap();
                f.assert_refused();
                assert!(f.disk().is_empty());
            })
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_remote_pg_owner_refuses_live_without_custody_or_checkpoint_pg() {
    scenario(
        "writer_boot_remote_pg_owner_refuses_live_without_custody_or_checkpoint_pg",
        73_369_522,
        "mac-b",
        |f| {
            Box::pin(async move {
                assert!(
                    f.local.is_none(),
                    "actual PG owner read rejects remote execution"
                );
                f.live(ProviderKind::Claude).await.unwrap();
                f.assert_refused();
                assert!(f.disk().is_empty());
            })
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_provider_mismatch_refuses_live_without_custody_or_checkpoint_pg() {
    scenario(
        "writer_boot_provider_mismatch_refuses_live_without_custody_or_checkpoint_pg",
        73_369_523,
        "mac-a",
        |f| {
            Box::pin(async move {
                let other = crate::services::tui_o::ownership::gate("codex");
                other.acquired();
                f.live(ProviderKind::Codex).await.unwrap();
                f.assert_refused();
                assert!(f.disk().is_empty());
                other.lost();
            })
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_failed_custody_sends_refusal_and_keeps_checkpoint_pg() {
    scenario(
        "writer_boot_failed_custody_sends_refusal_and_keeps_checkpoint_pg",
        73_369_524,
        "mac-a",
        |f| {
            Box::pin(async move {
                let path =
                    crate::services::discord::input_queue_path_for_test(&f.shared, f.channel);
                std::fs::create_dir_all(&path).unwrap();
                assert!(f.live(ProviderKind::Claude).await.is_err());
                f.assert_refused();
                std::fs::remove_dir(path).unwrap();
                assert!(f.disk().is_empty());
            })
        },
    )
    .await;
}

#[path = "boot_input_gateway_tests.rs"]
mod boot_input_gateway_tests;
#[path = "boot_input_race_tests.rs"]
mod race_tests;

#[path = "boot_input_owner_ready_tests.rs"]
mod boot_input_owner_ready_tests;

#[path = "boot_input_placement_tests.rs"]
mod boot_input_placement_tests;
