//! A real PG-owned host and the live intake/queue drain, with only provider execution replaced.
use super::*;
use crate::services::discord::router::{IntakeRequest, TurnKind, herdr_consent};
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Holds the real runtime before its writer actor can publish its first ready level.
struct Paused<I: HostIo> {
    io: Arc<I>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
    once: AtomicBool,
}
impl<I: HostIo> HostIo for Paused<I> {
    type Port = I::Port;
    type Lease = I::Lease;
    type Alarms = I::Alarms;
    type Bindings = I::Bindings;
    fn port(&self) -> impl Future<Output = Arc<Self::Port>> + Send {
        self.io.port()
    }
    async fn port_for(&self, channel: u64) -> Result<Arc<Self::Port>, String> {
        if !self.once.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.clone().acquire_owned().await.unwrap().forget();
        }
        self.io.port_for(channel).await
    }
    fn execution_permit(&self, channel: u64) -> Option<Result<Arc<OwnerPermit>, String>> {
        self.io.execution_permit(channel)
    }
    fn input_ready(&self, channel: u64, provider: ShadowProvider) {
        self.io.input_ready(channel, provider);
    }
    fn producer_observation(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Option<crate::services::tui_o::writer::ProducerObservation>> + Send
    {
        self.io.producer_observation(channel, provider)
    }
    fn lease(&self) -> Self::Lease {
        self.io.lease()
    }
    fn writer_unavailable(&self, channel: u64) {
        self.io.writer_unavailable(channel);
    }
    fn alarms(&self) -> Self::Alarms {
        self.io.alarms()
    }
    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<Self::Bindings> {
        self.io.bindings(channel, provider)
    }
    fn late_adopt_enabled(&self) -> bool {
        self.io.late_adopt_enabled()
    }
    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        self.io.activation_facts(channel, provider)
    }
    fn intake_fence(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<host::FencedFacts, String>> + Send {
        self.io.intake_fence(channel, provider)
    }
    fn local_custody(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> Result<host::Custody, String> {
        self.io.local_custody(channel, provider)
    }
    fn legacy(&self) -> Arc<dyn LegacyView> {
        self.io.legacy()
    }
    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send {
        self.io.legacy_busy(channel)
    }
    fn relaying(&self, channel: u64) -> bool {
        self.io.relaying(channel)
    }
    fn adopted(&self, channel: u64, provider: ShadowProvider) {
        self.io.adopted(channel, provider);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn owner_boot_live_input_waits_in_custody_and_starts_once_after_distinct_gate_ready_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_boot_live_input_waits_in_custody_and_starts_once_after_distinct_gate_ready_pg"
    )) {
        return;
    }
    let channel = 73_369_510;
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
    let message = 73_369_510_001;
    let selection = select(&ProviderKind::Claude, channel, "mac-a", "mac-a");
    execution::tests::admission(&pool, "claude", channel, "mac-a").await;
    let permit = execution::read(&pool, "claude", channel, "mac-a", || Ok(false))
        .await
        .unwrap()
        .unwrap();
    let local_gate = permit.gate();
    let snapshot = cutover::boot_snapshot().unwrap();
    snapshot.install_execution(channel, Some(Ok(permit)));
    let global = crate::services::tui_o::ownership::gate("claude");
    global.acquired();
    assert!(
        !Arc::ptr_eq(&local_gate, &global),
        "the fixture must exercise PG's channel gate"
    );
    assert!(local_gate.current().epoch().is_some());
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
        .unwrap()
        .unwrap();
    store
        .begin_era(&[channel], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                policy_parent: None,
                channel,
                sources: vec![],
                initial_anchor: 100,
                build_digest: "boot-input-test".into(),
                at: chrono::Utc::now(),
            })
        })
        .unwrap();
    let (registry, shared) = runtime(&pool, ProviderKind::Claude, channel, "gateway").await;
    let server = gateway_context(&shared, channel).await;
    let discord = Discord::new().await;
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
    let readiness = host::process_readiness();
    let mut handles = host::start_managed(
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
            readiness: readiness.clone(),
        },
    );
    assert_eq!(handles.len(), 1);
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert!(readiness.is_hosted(channel));
    assert!(!readiness.accepts(channel));
    assert!(matches!(
        for_input_placement("claude", channel).await,
        IntakeRoute::Hold(_)
    ));
    let effects = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(tokio::sync::Notify::new());
    let effect_probe =
        herdr_consent::boot_input_effect_probe_for_tests(effects.clone(), completed.clone());
    let pane_probe = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let checkpoint = checkpoints.clone();
    let deps = crate::services::discord::router::IntakeDeps {
        http: &discord.http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &shared,
        token: "test-token",
    };
    herdr_consent::dispatch_boot_live_for_tests(
        &deps,
        IntakeRequest {
            intake_outbox_id: None,
            channel_id: serenity::ChannelId::new(channel),
            user_msg_id: serenity::MessageId::new(message),
            source_message_ids: Vec::new(),
            busy_followup_retry_user_msg_id: serenity::MessageId::new(message),
            request_owner: serenity::UserId::new(7),
            request_owner_name: "boot-user".into(),
            user_text: "keep this input through writer boot".into(),
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            merge_consecutive: false,
            reply_context: Some("quoted user context".into()),
            has_reply_boundary: true,
            dm_hint: Some(false),
            turn_kind: TurnKind::Foreground,
            preserve_on_cancel: true,
        },
        move || {
            checkpoint.fetch_add(1, Ordering::SeqCst);
        },
    )
    .await
    .unwrap();
    let (queued, _) = crate::services::turn_orchestrator::load_channel_pending_queue_checked(
        &ProviderKind::Claude,
        &shared.token_hash,
        serenity::ChannelId::new(channel),
    )
    .unwrap();
    assert_eq!(
        queued.len(),
        1,
        "one durable original before writer readiness"
    );
    assert_eq!(queued[0].message_id.get(), message);
    assert_eq!(queued[0].text, "keep this input through writer boot");
    assert_eq!(
        queued[0].source_message_ids,
        vec![serenity::MessageId::new(message)]
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(checkpoints.load(Ordering::SeqCst), 1);
    // No direct queue kick is injected: only the real writer readiness callback can start it.
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), completed.notified())
        .await
        .expect("writer readiness must drain the saved live original without catch-up or a timer");
    assert!(readiness.accepts(channel));
    assert!(readiness.hosted_under(channel, &local_gate));
    assert!(!readiness.hosted_under(channel, &global));
    assert_eq!(
        for_input_placement("claude", channel).await,
        IntakeRoute::Gateway,
        "a PG channel's actual gate must be used after resume"
    );
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(checkpoints.load(Ordering::SeqCst), 1);
    handles.pop().unwrap().stop_and_join().await.unwrap();
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

#[path = "boot_input_more_tests.rs"]
mod more_tests;
