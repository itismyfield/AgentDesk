use super::codex_submission::{Operation, Port as ActorPort, Prepared};
use super::*;
use crate::services::tui_o::store::codex_provenance::submit_floor::{
    PreparedLaunch, tests as native,
};
use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use crate::services::tui_o::writer::host::{
    Custody, HostIo, HostParts, ManagedWriterHandle, Readiness, start_managed,
};
use crate::services::tui_o::writer::owed::tests::{Alarms, Lease, Port};
use crate::services::tui_prompt_dedupe::{
    self as dedupe,
    binding_context::{BindingContext, PreparedIncarnation},
};
use dedupe::codex_verified::provenance::decisions::PersistenceReason as P;
use dedupe::codex_verified::{DeliveryPermission, VerifiedSourceSnapshot};
use std::{path::Path, sync::MutexGuard};

fn enabled(root: &Path) -> OStore {
    OStore::open_if_enabled(&StoreConfig { enabled: true }, root)
        .unwrap()
        .unwrap()
}
fn initialized(channel: u64, sources: Vec<InitSource>) -> Initialized {
    Initialized {
        channel,
        sources,
        initial_anchor: 1,
        build_digest: "test".into(),
        at: chrono::Utc::now(),
    }
}

struct Isolation {
    _state: MutexGuard<'static, ()>,
    _env: crate::config::test_env_lock::SharedTestEnvLockGuard,
}
struct Io {
    port: Arc<Port>,
    calls: std::sync::Mutex<Vec<&'static str>>,
}
impl HostIo for Io {
    type Port = Port;
    type Lease = Lease;
    type Alarms = Alarms;
    type Bindings = ChannelBindingLog;
    async fn port(&self) -> Arc<Port> {
        self.calls.lock().unwrap().push("port");
        self.port.clone()
    }
    fn lease(&self) -> Lease {
        self.calls.lock().unwrap().push("lease");
        Lease
    }
    fn alarms(&self) -> Alarms {
        Alarms::default()
    }
    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<ChannelBindingLog> {
        self.calls.lock().unwrap().push("bindings");
        Arc::new(ChannelBindingLog::new(channel, provider))
    }
    async fn activation_facts(
        &self,
        _: u64,
        _: ShadowProvider,
    ) -> Result<super::super::activation::ActivationFacts, String> {
        Ok(Default::default())
    }
    fn local_custody(&self, _: u64, _: ShadowProvider) -> Result<Custody, String> {
        Ok(Custody::Free)
    }
    fn legacy(&self) -> Arc<dyn super::super::adoption::LegacyView> {
        Arc::new(super::super::adoption::NoLegacy)
    }
    async fn legacy_busy(&self, _: u64) -> bool {
        false
    }
    fn relaying(&self, _: u64) -> bool {
        false
    }
}

pub(crate) struct Fixture {
    pub root: tempfile::TempDir,
    pub source: VerifiedSourceSnapshot,
    pub launch: PreparedLaunch,
    pub ready: Arc<Readiness>,
    pub handles: Vec<ManagedWriterHandle>,
    io: Arc<Io>,
    _selection: crate::services::tui_o::cutover::test_override::ChannelsGuard,
    _fake: crate::services::provider_teardown::tests::test_support::FakeTmux,
    _guards: [crate::config::TestEnvVarGuard; 2],
    _env_root: tempfile::TempDir,
    _isolation: Isolation,
}
impl Fixture {
    pub(crate) async fn new(allowed: bool) -> Self {
        use crate::services::claude_tui::hook_server::observation_ingress::observe_binding_hook;
        use dedupe::binding_context::{
            BINDING_HEADER, CapturedContext, HookBindingEnvelope, ObservedHookProcess,
        };
        let env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let state = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let isolation = Isolation {
            _state: state,
            _env: env,
        };
        let (env_root, guards) =
            dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
        dedupe::reset_state_for_tests();
        let root = tempfile::tempdir().unwrap();
        let channel = 584_502_101;
        let id = "019e660d-4859-7522-9cee-8ba7c4e7c743";
        let tmux = format!("p5a2-{}", uuid::Uuid::new_v4().simple());
        let path = root.path().join(format!("rollout-{id}.jsonl"));
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now() - chrono::Duration::seconds(60),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: tmux.clone(),
            channel_id: Some(channel),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(root.path().into()),
            first_prompt_digest: Some(format!("sha256:{}", "a".repeat(64))),
            source_policy: Some("verified".into()),
        };
        PreparedIncarnation::create(context.clone()).unwrap();
        let marker = crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce");
        std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, &context.execution_nonce).unwrap();
        dedupe::register_tmux_channel(&tmux, channel);
        dedupe::binding_events::set_test_root(Some(root.path()));
        dedupe::codex_verified::set_permission_for_tests(&context, DeliveryPermission::Allowed);
        let fake =
            crate::services::provider_teardown::tests::test_support::FakeTmux::install(&tmux);
        let at = chrono::Utc::now().to_rfc3339();
        let meta = serde_json::json!({"type":"session_meta","timestamp":at,"payload":{
            "id":id,"timestamp":at,"cwd":root.path(),"source":"cli","originator":"codex_cli_rs"}});
        std::fs::write(&path, format!("{meta}\n")).unwrap();
        let envelope = HookBindingEnvelope {
            context: CapturedContext::Captured(context.clone()),
            observed: ObservedHookProcess::default(),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
        let claim = serde_json::json!({"session_id":id,"transcript_path":path,"source":"startup","prompt":null});
        let _observed = observe_binding_hook(
            "codex",
            "session-start",
            Some(id),
            Some(id),
            &claim,
            &headers,
        );
        let source = crate::services::tmux_common::with_tmux_source_authority(
            &tmux,
            VerifiedSourceSnapshot::capture,
        )
        .unwrap();
        if !allowed {
            dedupe::codex_verified::set_permission_for_tests(&context, DeliveryPermission::Unknown);
        }
        let owner = enabled(root.path());
        let bytes = std::fs::read(&path).unwrap();
        use sha2::{Digest, Sha256};
        owner
            .begin_era(&[channel], chrono::Utc::now(), |c| {
                Ok(initialized(
                    c,
                    vec![crate::services::tui_o::store::InitSource {
                        source_id: source.execution().source.clone(),
                        delivery_start: bytes.len() as u64,
                        prefix_hash: hex::encode(Sha256::digest(&bytes)),
                    }],
                ))
            })
            .unwrap();
        let selection = crate::services::tui_o::cutover::test_override::force_candidates(&[(
            channel,
            crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
        )]);
        let gate = Arc::new(crate::services::tui_o::ownership::OwnershipGate::default());
        gate.acquired();
        let ready = Arc::new(Readiness::default());
        let io = Arc::new(Io {
            port: Arc::new(Port::default()),
            calls: Default::default(),
        });
        let handles = start_managed(
            ShadowProvider::Codex,
            true,
            crate::services::tui_o::cutover::boot_ownership(),
            || HostParts {
                io: io.clone(),
                runtime_root: Some(root.path().into()),
                gate: gate.clone(),
                readiness: ready.clone(),
            },
        );
        assert_eq!(handles.len(), 1);
        tokio::time::sleep(POLL_INTERVAL * 3).await;
        if allowed {
            assert!(ready.accepts(channel));
        } else {
            assert!(ready.is_hosted(channel));
            assert!(!ready.accepts(channel));
        }
        let launch = PreparedLaunch {
            context,
            episode: native::launch(channel, &source.execution().source).episode,
        };
        Self {
            root,
            source,
            launch,
            ready,
            handles,
            io,
            _selection: selection,
            _fake: fake,
            _guards: guards,
            _env_root: env_root,
            _isolation: isolation,
        }
    }
    pub(crate) fn port(&self) -> ActorPort {
        self.ready
            .submission(self.launch.episode.channel_id)
            .unwrap()
    }
    pub(crate) async fn prepare(&self) -> Prepared {
        let reply = self
            .port()
            .request(Operation::Capture(Box::new((
                self.launch.clone(),
                self.source.execution().clone(),
            ))))
            .await
            .unwrap();
        self.ready
            .prepared_reply(self.launch.episode.channel_id, reply)
            .unwrap()
    }
    pub(crate) fn append(&self, kind: &str, turn: &str) -> u64 {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.source.execution().source.path)
            .unwrap();
        let record = serde_json::json!({"type":"event_msg","payload":{"type":kind,"turn_id":turn}});
        writeln!(file, "{record}").unwrap();
        file.metadata().unwrap().len()
    }
    pub(crate) async fn stop(mut self) {
        for handle in self.handles.drain(..) {
            handle.stop_and_join().await.unwrap();
        }
        dedupe::binding_events::set_test_root(None);
        dedupe::reset_state_for_tests();
    }
}

#[tokio::test(start_paused = true)]
async fn p5a2_stale_actor_reply_never_issues_effect() {
    let mut f = Fixture::new(true).await;
    let prepared = f.prepare().await;
    let a = f.port();
    let delayed = a
        .request_evidence(Operation::ReadBack(Box::new(f.source.execution().clone())))
        .await
        .unwrap();
    assert!(
        a.take_closed(delayed).is_err(),
        "ReadBack cannot become Closed"
    );
    let wrong = a
        .request_evidence(Operation::ReadBack(Box::new(f.source.execution().clone())))
        .await
        .unwrap();
    assert!(a.take_prepared(wrong).is_err());
    let foreign = a
        .request_evidence(Operation::ReadBack(Box::new(f.source.execution().clone())))
        .await
        .unwrap();
    let (other_channel, _) = ActorPort::new(f.handles[0].generation() + 1);
    assert_eq!(
        other_channel.take_read_back(foreign).unwrap_err(),
        P::ActorStale
    );
    let mut inner = a
        .request_evidence(Operation::ReadBack(Box::new(f.source.execution().clone())))
        .await
        .unwrap();
    ActorPort::replace_read_back_run_for_tests(&mut inner);
    assert_eq!(a.take_read_back(inner).unwrap_err(), P::ActorStale);
    let delayed = a
        .request_evidence(Operation::ReadBack(Box::new(f.source.execution().clone())))
        .await
        .unwrap();
    let generation = f.handles[0].generation();
    let failed = crate::services::tui_o::store::fault::plant(
        f.root.path(),
        crate::services::tui_o::store::fault::Step::Write,
        std::io::ErrorKind::StorageFull,
        Some(1),
    );
    let mut next = f.launch.clone();
    next.episode.user_message_id += 1;
    next.episode.turn_nonce = "next".into();
    assert!(
        a.request(Operation::Capture(Box::new((
            next,
            f.source.execution().clone()
        ))))
        .await
        .is_err()
    );
    drop(failed);
    let mut replacement = None;
    for _ in 0..60 {
        tokio::time::sleep(POLL_INTERVAL).await;
        if let Some(port) = f.ready.submission(f.launch.episode.channel_id)
            && !port.same_run(&a)
            && f.ready.accepts(f.launch.episode.channel_id)
        {
            replacement = Some(port);
            break;
        }
    }
    let b = replacement.expect("same-claim actor recovery");
    assert!(!b.same_run(&a));
    assert_eq!(f.handles[0].generation(), generation);
    assert_eq!(b.take_read_back(delayed).unwrap_err(), P::ActorStale);
    assert!(!b.accepts_prepared(&prepared));
    assert_eq!(
        f.ready
            .with_verified_prepared(f.launch.episode.channel_id, &prepared, || 1),
        Err(P::ActorStale)
    );
    let fresh = f.prepare().await;
    assert_eq!(
        f.ready
            .with_verified_prepared(f.launch.episode.channel_id, &fresh, || 1),
        Ok(1)
    );
    let read = f
        .ready
        .read_back_original(&f.source, |read| read.projection().clone())
        .await
        .unwrap();
    assert_eq!(read.execution(), f.source.execution());
    assert!(f.io.port.posts().is_empty());
    f.handles.pop().unwrap().stop_and_join().await.unwrap();
    assert!(matches!(
        b.request_evidence(Operation::ReadBack(Box::new(f.source.execution().clone())))
            .await,
        Err(P::ActorUnavailable)
    ));
    f.stop().await;
}

#[tokio::test(start_paused = true)]
async fn p5a2_source_snapshot_is_rechecked_after_actor_await() {
    let f = Fixture::new(true).await;
    f.prepare().await;
    let mut pending = Box::pin(f.ready.read_back_original(&f.source, |_| 1));
    assert!(
        futures::poll!(&mut pending).is_pending(),
        "actor request really yields without source/Live locks"
    );
    // Re-entry succeeds while the actor reply is pending; it changes the original incarnation.
    crate::services::tmux_common::with_tmux_source_authority(
        &f.launch.context.tmux_session,
        |_| {
            let marker = crate::services::tmux_common::session_temp_path(
                &f.launch.context.tmux_session,
                "spawn_nonce",
            );
            std::fs::write(marker, "replacement-incarnation").unwrap();
            assert!(f.ready.submission(f.launch.episode.channel_id).is_some());
        },
    );
    assert_eq!(pending.await, Err(P::SourceChanged));
    f.stop().await;
}

#[tokio::test(start_paused = true)]
async fn p5a2_dormant_actor_entrypoint_keeps_legacy_trace() {
    let f = Fixture::new(false).await;
    assert!(
        !dedupe::codex_verified::provenance::admission::owed_delivery_ready(
            f.launch.episode.channel_id
        )
    );
    assert!(!dedupe::codex_verified_o_source_allowed(
        f.launch.episode.channel_id,
        &f.source.execution().source
    ));
    let before = f.io.calls.lock().unwrap().clone();
    let path = f
        .root
        .path()
        .join("o_store")
        .join(f.launch.episode.channel_id.to_string())
        .join("codex_provenance");
    assert!(!path.exists());
    let reader = f.source.execution().source.clone();
    let owner = enabled(f.root.path());
    let era = owner.read_era().unwrap().unwrap();
    let original = owner
        .open_channel(&era, f.launch.episode.channel_id)
        .unwrap()
        .unwrap()
        .cursor(&reader)
        .unwrap()
        .clone();
    crate::services::tui_o::store::codex_provenance::SIDECAR_IO.with(|io| io.set((0, 0)));
    f.append("task_started", "t1");
    f.append("agent_message", "t1");
    f.append("task_complete", "t1");
    tokio::time::sleep(POLL_INTERVAL * 4).await;
    assert_eq!(
        crate::services::tui_o::store::codex_provenance::SIDECAR_IO.with(std::cell::Cell::get),
        (0, 0)
    );
    let after = owner
        .open_channel(&era, f.launch.episode.channel_id)
        .unwrap()
        .unwrap()
        .cursor(&reader)
        .unwrap()
        .clone();
    assert_eq!(
        after, original,
        "commands.is_some() does not bypass Unknown source permission"
    );
    assert_eq!(*f.io.calls.lock().unwrap(), before);
    assert!(f.io.port.posts().is_empty());
    assert!(!path.exists());
    f.stop().await;
}

#[tokio::test(start_paused = true)]
async fn p5a2_actor_deny_records_exact_evidence_without_post_or_closed_permission() {
    let f = Fixture::new(true).await;
    let prepared = f.prepare().await;
    let token = crate::services::provider::CancelToken::new();
    token.publish_cancel("independent cancel");
    let deny = dedupe::codex_verified::provenance::CodexEpisodeDeny {
        execution: f.source.execution().clone(),
        episode: f.launch.episode.clone(),
    };
    let reply = f
        .port()
        .request_evidence(Operation::Deny(Box::new(deny.clone())))
        .await
        .unwrap();
    let recorded = f
        .ready
        .with_deny_reply(f.launch.episode.channel_id, reply, |recorded| {
            recorded.deny().clone()
        })
        .unwrap();
    assert_eq!(recorded, deny);
    let read = f
        .ready
        .read_back_original(&f.source, |read| read.projection().clone())
        .await
        .unwrap();
    assert_eq!(read.evidence().denies(), std::slice::from_ref(&deny));
    assert_eq!(
        read.evidence().floors(),
        &[
            crate::services::tui_o::store::codex_provenance::submit_floor::SubmitFloor::NativeFloor(
                Box::new(prepared.floor().clone())
            )
        ]
    );
    let reply = f
        .port()
        .request_evidence(Operation::Deny(Box::new(deny)))
        .await
        .unwrap();
    assert!(f.port().take_closed(reply).is_err());
    assert!(token.cancelled.load(std::sync::atomic::Ordering::Acquire));
    assert!(f.io.port.posts().is_empty());
    f.stop().await;
}
