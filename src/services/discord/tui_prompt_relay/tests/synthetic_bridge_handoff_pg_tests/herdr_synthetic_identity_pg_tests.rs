//! Hosted synthetic claims keep their observed execution across deferred and post-claim races.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_session_canonical_identity::{
    self as canonical, pg_tests as canonical_fixture,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedState, SourceRef,
};
use crate::services::discord::{inflight, tui_direct_pending_start as pending};
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events};
use std::io::Write;

const NODE: &str = "synthetic-recovery-test-node";
const N1: &str = "00112233445566778899aabbccddee11";
const N2: &str = "00112233445566778899aabbccddee12";

mod final_recheck_tests;
mod hosted_classification_tests;

struct Fixture {
    db: TestPostgresDb,
    pool: sqlx::PgPool,
    shared: Arc<SharedData>,
    channel: ChannelId,
    logical: String,
    key: String,
    execution: HostedExecution,
    source: binding_events::SourceId,
    _registry: Box<dyn std::any::Any>,
    rig: crate::services::session_host::herdr_socket_rig_tests::HerdrRig,
}

impl Fixture {
    async fn new(root: &Path, channel: ChannelId) -> Self {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let logical = format!("synthetic-recovery-{}", channel.get());
        dedupe::register_tmux_channel(&logical, channel.get());
        let key = crate::services::discord::adk_session::build_namespaced_session_key(
            &shared.token_hash,
            &ProviderKind::Claude,
            &logical,
        );
        let owner = HostedOwner {
            provider: "claude".into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: channel.get().to_string(),
            logical_key: logical.clone(),
            owner_node: NODE.into(),
            runtime_root: crate::config::runtime_root().unwrap().display().to_string(),
        };
        let endpoint = crate::config::session_hosts::herdr_endpoint(channel.get()).unwrap();
        let rig = crate::services::session_host::herdr_socket_rig_tests::HerdrRig::start();
        let registry = rig.registry_as_on_this_thread(
            NODE,
            &endpoint.key,
            &endpoint.socket_path,
            &endpoint.herdr_session,
        );
        let context = rig.context(N1);
        rig.run_provider(&context, false);
        let location = HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: endpoint.key,
            socket_addr: endpoint.socket_path.display().to_string(),
            named_session: endpoint.herdr_session,
            pane_id: crate::services::session_host::herdr_socket_rig_tests::PANE.into(),
        };
        let execution = HostedExecution {
            schema: 1,
            state: HostedState::Bound,
            execution_nonce: N1.into(),
            owner: owner.clone(),
            location: Some(location),
            expected: Some(rig.expected(N1)),
            source_ref: SourceRef {
                runtime_root: owner.runtime_root.clone(),
                channel: channel.get().to_string(),
                provider: "claude".into(),
                logical_key: logical.clone(),
                execution_nonce: N1.into(),
                initial_source: None,
                baseline_event_seq: None,
            },
        };
        canonical::upsert_hook_session_with_identity_pg(
            &pool,
            crate::db::dispatched_sessions::HookSessionUpsert {
                instance_id: None,
                ..canonical_fixture::params(&key, &owner.channel_id)
            },
            Some(canonical::CanonicalSessionIdentity {
                kind: canonical::SessionIdentityKind::DiscordChannel,
                discord_token_hash: &shared.token_hash,
                channel_id: &owner.channel_id,
            }),
        )
        .await
        .unwrap();
        canonical_fixture::set_raw(&pool, &key, Some(serde_json::to_value(&execution).unwrap()))
            .await;
        let path = root.join(format!("{}-native-n1.jsonl", channel.get()));
        std::fs::write(&path, b"").unwrap();
        let source =
            crate::services::tui_o::shadow::binding_reader::source_id_for("native-n1", &path)
                .unwrap();
        let fixture = Self {
            db,
            pool,
            shared,
            channel,
            logical,
            key,
            execution,
            source,
            _registry: Box::new(registry),
            rig,
        };
        fixture.publish_source(&fixture.source, N1, 1);
        let marker = crate::services::tmux_common::session_temp_path(&fixture.logical, "host_kind");
        std::fs::write(marker, b"herdr").unwrap();
        fixture
    }

    fn publish_source(&self, source: &binding_events::SourceId, nonce: &str, seq: u64) {
        // The fixture writes its execution history explicitly below; publish the cache without
        // also appending a second event for the same sequence through registration.
        let log_root = binding_events::test_root();
        binding_events::set_test_root(None);
        dedupe::register_tmux_runtime_binding(
            &self.logical,
            dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: source.path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(source.session_id.clone()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        binding_events::set_test_root(log_root.as_deref());
        self.shared.tmux_watchers.insert(
            self.channel,
            crate::services::discord::TmuxWatcherHandle {
                tmux_session_name: self.logical.clone(),
                output_path: source.path.display().to_string(),
                paused: Arc::new(false.into()),
                resume_offset: Arc::new(Mutex::new(None)),
                cancel: Arc::new(false.into()),
                pause_epoch: Arc::new(0.into()),
                turn_delivered: Arc::new(false.into()),
                last_heartbeat_ts_ms: Arc::new(chrono::Utc::now().timestamp_millis().into()),
            },
        );
        self.append_source_event(source, nonce, seq);
        dedupe::install_herdr_execution(&self.logical, nonce);
        dedupe::admit_herdr_execution(&self.logical, nonce);
    }

    fn append_source_event(&self, source: &binding_events::SourceId, nonce: &str, seq: u64) {
        let now = chrono::Utc::now();
        let event = binding_events::BindingEvent {
            seq,
            channel_id: self.channel.get(),
            provider: "claude".into(),
            tmux_session: self.logical.clone(),
            execution_nonce: Some(nonce.into()),
            old: None,
            new: binding_events::BindingTarget::Source(source.clone()),
            cause: binding_events::BindingCause::Startup,
            parent_hint: None,
            evidence: binding_events::BindingEvidence {
                hook_event: Some("SessionStart".into()),
                received_at: now,
            },
            committed_at: now,
        };
        let dir = binding_events::test_root()
            .unwrap()
            .join(binding_events::BINDING_EVENTS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{}.log", self.channel.get())))
            .unwrap();
        writeln!(log, "{}", serde_json::to_string(&event).unwrap()).unwrap();
        log.sync_all().unwrap();
        let history = binding_events::records_strict(self.channel.get())
            .unwrap()
            .unwrap();
        assert_eq!(
            history.len() as u64,
            seq,
            "fixture history must have one event per sequence"
        );
        assert_eq!(
            history.last().unwrap().execution_nonce.as_deref(),
            Some(nonce)
        );
    }

    fn pending(
        &self,
        nonce: &str,
        source: &binding_events::SourceId,
        anchor: u64,
    ) -> pending::TuiDirectPendingStart {
        // Main ignores this additive carrier; the implemented decoder preserves it unchanged.
        serde_json::from_value(serde_json::json!({
            "provider": "claude", "channel_id": self.channel.get(),
            "tmux_session_name": self.logical, "prompt_text": "already submitted input",
            "anchor_message_id": anchor, "lease_relay_owner": "tmux_watcher",
            "lease_runtime_kind": "claude_tui", "lease_turn_id": format!("{nonce}-native-turn"),
            "lease_session_key": self.key, "generation": self.shared.restart.current_generation,
            "created_at_ms": 0, "observed_at_ms": 0,
            "captured_source": [source.path.display().to_string(), 0],
            "hosted_source_checked": true,
            "hosted_source": {
                "provider": "claude", "channel_id": self.channel.get(), "logical_key": self.logical,
                "execution_nonce": nonce, "source": source
            }
        }))
        .unwrap()
    }

    async fn replace_execution(&self, state: HostedState, nonce: &str) -> HostedExecution {
        let mut next = self.execution.clone();
        next.state = state;
        next.execution_nonce = nonce.into();
        next.source_ref.execution_nonce = nonce.into();
        next.expected = Some(self.rig.expected(nonce));
        let context = self.rig.context(nonce);
        self.rig.run_provider(&context, false);
        canonical_fixture::set_raw(
            &self.pool,
            &self.key,
            Some(serde_json::to_value(&next).unwrap()),
        )
        .await;
        next
    }

    async fn hosted_json(&self) -> serde_json::Value {
        canonical_fixture::raw_of(&self.pool, &self.key)
            .await
            .expect("the fixture has a hosted execution")
    }

    async fn claim(&self, record: &pending::TuiDirectPendingStart) -> bool {
        let claim = synthetic_start::pending_start_claim_fn();
        claim(&self.shared, record).await
    }

    fn publish_pending_lease(
        &self,
        record: &pending::TuiDirectPendingStart,
    ) -> ExternalInputRelayLease {
        let mut lease = ExternalInputRelayLease::unassigned(Some(self.channel.get()));
        lease.turn_id = record.lease_turn_id.clone();
        lease.session_key = record.lease_session_key.clone();
        lease.relay_owner = ExternalInputRelayOwner::TmuxWatcher;
        lease.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        lease.hosted_source = record.hosted_source.clone();
        dedupe::record_external_input_turn_lease("claude", &self.logical, lease)
    }

    async fn assert_quiet(&self) {
        assert!(
            crate::services::discord::mailbox_snapshot(&self.shared, self.channel)
                .await
                .cancel_token
                .is_none(),
            "a refused hosted claim left an actor"
        );
        assert!(
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, self.channel.get())
                .is_none(),
            "a refused hosted claim saved inflight"
        );
    }

    async fn close(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn hook_observer_carries_the_original_hosted_source_through_pending_json_restore_pg() {
    use crate::services::claude_tui::hook_server::{HookEvent, HookEventKind};

    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(7_186_502);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    let (hooks, hook_rx) = tokio::sync::broadcast::channel(4);
    let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
    let expected_logical = fixture.logical.clone();
    super::super::super::hook_observer::spawn_tui_prompt_relay_observer(
        "claude".into(),
        hook_rx,
        move |prompt| {
            let observed_tx = observed_tx.clone();
            let expected_logical = expected_logical.clone();
            Box::pin(async move {
                if prompt.tmux_session_name == expected_logical {
                    observed_tx.send(prompt).expect("live observation receiver");
                }
            })
        },
    );
    hooks
        .send(HookEvent {
            provider: "claude".into(),
            session_id: fixture.logical.clone(),
            kind: HookEventKind::UserPromptSubmit,
            received_at: chrono::Utc::now(),
            payload: serde_json::json!({
                "session_id": fixture.source.session_id,
                "transcript_path": fixture.source.path.display().to_string(),
                "prompt": "retain this hook's original hosted source",
                "prompt_id": "original-hosted-source-hook"
            }),
            fanout: None,
            original_context: None,
        })
        .expect("the production observer subscribed synchronously");
    let observed = tokio::time::timeout(Duration::from_secs(5), observed_rx.recv())
        .await
        .expect("the real hook observer produced a relay observation")
        .expect("observed prompt");
    assert_eq!(observed.tmux_session_name, fixture.logical);
    assert_eq!(observed.prompt, "retain this hook's original hosted source");
    let lease = record_observed_external_turn_lease(&fixture.shared, &observed, channel);
    assert_eq!(lease.session_key.as_deref(), Some(fixture.key.as_str()));
    assert!(
        lease.turn_id.is_some(),
        "the hook assigned an original turn identity"
    );
    let worker_lock = pending::channel_lock("claude", channel.get())
        .lock_owned()
        .await;
    synthetic_start::defer_synthetic_turn_start(
        &fixture.shared,
        &ProviderKind::Claude,
        channel,
        &observed,
        MessageId::new(7_186_503),
        &lease,
        Some((fixture.source.path.display().to_string(), 0)),
    );
    let saved = pending::load_all()
        .into_iter()
        .find(|record| record.channel_id == channel.get())
        .expect("defer persisted its original obligation before spawning the blocked worker");
    let serialized = serde_json::to_value(&saved).unwrap();
    assert_eq!(
        serialized["hosted_source"]["execution_nonce"],
        serde_json::json!(N1),
        "the real hook, observed lease and pending writer lost the original execution carrier"
    );
    assert_eq!(
        serialized["hosted_source"]["source"],
        serde_json::to_value(&fixture.source).unwrap()
    );
    let restored: pending::TuiDirectPendingStart =
        serde_json::from_value(serialized.clone()).unwrap();
    assert_eq!(
        restored, saved,
        "the durable pending decoder preserves the original source pin"
    );
    assert_eq!(
        serde_json::to_value(restored).unwrap()["hosted_source"],
        serialized["hosted_source"]
    );
    fixture.assert_quiet().await;
    drop(worker_lock);
    let claimed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let inflight =
                inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get());
            let still_pending = pending::load_all().iter().any(|record| {
                record.channel_id == channel.get()
                    && record.anchor_message_id == saved.anchor_message_id
            });
            if let Some(inflight) = inflight.filter(|_| !still_pending) {
                break inflight;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the real deferred worker claimed, saved inflight and completed its pending record");
    assert_eq!(claimed.hosted_execution_nonce.as_deref(), Some(N1));
    assert_eq!(
        claimed.session_id.as_deref(),
        Some(fixture.source.session_id.as_str())
    );
    assert_eq!(claimed.output_path.as_deref(), fixture.source.path.to_str());
    assert_eq!(claimed.external_turn_id, lease.turn_id);
    assert_eq!(claimed.user_msg_id, saved.anchor_message_id);
    drop(hooks);
    fixture.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn modern_unattributed_hook_pending_cannot_adopt_a_source_discovered_after_inline_refusal_pg()
{
    use crate::services::claude_tui::hook_server::{HookEvent, HookEventKind};

    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(7_186_652);
    let anchor = MessageId::new(7_186_653);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    let (hooks, hook_rx) = tokio::sync::broadcast::channel(4);
    let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
    let expected_logical = fixture.logical.clone();
    super::super::super::hook_observer::spawn_tui_prompt_relay_observer(
        "claude".into(),
        hook_rx,
        move |prompt| {
            let observed_tx = observed_tx.clone();
            let expected_logical = expected_logical.clone();
            Box::pin(async move {
                if prompt.tmux_session_name == expected_logical {
                    observed_tx.send(prompt).expect("live observation receiver");
                }
            })
        },
    );
    hooks
        .send(HookEvent {
            provider: "claude".into(),
            session_id: fixture.logical.clone(),
            kind: HookEventKind::UserPromptSubmit,
            received_at: chrono::Utc::now(),
            payload: serde_json::json!({
                "prompt": "input observed before the native source could be proved",
                "prompt_id": "unattributed-before-new-execution"
            }),
            fanout: None,
            original_context: None,
        })
        .expect("the production hook observer is subscribed");
    let observed = tokio::time::timeout(Duration::from_secs(5), observed_rx.recv())
        .await
        .expect("the actual hook producer published the unattributed original input")
        .expect("observed original input");
    assert!(observed.hosted_source.is_none());
    let lease = record_observed_external_turn_lease(&fixture.shared, &observed, channel);
    assert!(lease.hosted_source.is_none());
    assert!(lease.turn_id.is_some());

    fixture.replace_execution(HostedState::Bound, N2).await;
    let path = root
        .path()
        .join("new-execution-after-unattributed-hook.jsonl");
    std::fs::write(&path, b"").unwrap();
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for("native-n2", &path).unwrap();
    fixture.publish_source(&source, N2, 2);
    let (_, watcher) = fixture
        .shared
        .tmux_watchers
        .remove(&channel)
        .expect("the fixture's watcher metadata exists before inline fallback");
    let (inline, captured_source) =
        synthetic_start::claim_tui_direct_synthetic_turn_inner::<false>(
            &fixture.shared,
            &ProviderKind::Claude,
            channel,
            &fixture.logical,
            (&observed.prompt, anchor),
            &lease,
            (None, observed.native_turn_id.as_deref()),
        )
        .await;
    assert!(
        !inline.claimed,
        "the inline fence refused the unproved original input"
    );
    assert_eq!(inline.relay_owner, ExternalInputRelayOwner::BridgeAdapter);
    assert_eq!(
        captured_source.as_ref().map(|(path, _)| path.as_str()),
        source.path.to_str(),
        "inline fallback observed N2 only after the original hook had no source pin"
    );
    fixture.assert_quiet().await;
    let worker_lock = pending::channel_lock("claude", channel.get())
        .lock_owned()
        .await;
    synthetic_start::defer_synthetic_turn_start(
        &fixture.shared,
        &ProviderKind::Claude,
        channel,
        &observed,
        anchor,
        &lease,
        captured_source.clone(),
    );
    let deferred = pending::load_all()
        .into_iter()
        .find(|record| record.channel_id == channel.get())
        .expect("the real defer constructor persisted the original input");
    assert_eq!(deferred.captured_source, captured_source);
    assert_eq!(deferred.lease_turn_id, lease.turn_id);
    assert!(serde_json::to_value(&deferred).unwrap()["hosted_source"].is_null());
    fixture.shared.tmux_watchers.insert(channel, watcher);
    drop(worker_lock);

    let (saved, retained) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let saved =
                inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get());
            let retained = pending::load_all()
                .into_iter()
                .find(|record| record.channel_id == channel.get());
            if saved.is_some()
                || retained
                    .as_ref()
                    .is_some_and(|record| record.attempt_count >= 1)
            {
                break (saved, retained);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the actual deferred worker either claimed or completed a refused claim attempt");
    assert!(
        saved.is_none(),
        "the modern unattributed hook was restamped from N2's later captured path and saved"
    );
    fixture.assert_quiet().await;
    let retained = retained.expect("the unresolved original input remains pending");
    assert!(retained.attempt_count >= 1);
    assert!(serde_json::to_value(&retained).unwrap()["hosted_source"].is_null());
    assert_eq!(retained.lease_turn_id, lease.turn_id);

    let worker_lock = tokio::time::timeout(
        Duration::from_secs(5),
        pending::channel_lock("claude", channel.get()).lock_owned(),
    )
    .await
    .expect("the refused old worker exhausted its bounded retry budget");
    fixture.assert_quiet().await;
    let current = fixture.pending(N2, &source, 7_186_654);
    fixture.publish_pending_lease(&current);
    assert!(
        fixture.claim(&current).await,
        "a separately proved N2 input remains claimable while the unknown original stays pending"
    );
    let saved =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
    assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N2));
    assert_eq!(saved.user_msg_id, current.anchor_message_id);
    assert!(pending::load_all().iter().any(|record| {
        record.channel_id == channel.get() && record.anchor_message_id == anchor.get()
    }));
    drop(hooks);
    fixture.close().await;
    drop(worker_lock);
}

#[tokio::test(flavor = "current_thread")]
async fn admitted_herdr_pending_worker_waits_for_recovery_transition_then_claims_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(7_186_212);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    let record = fixture.pending(N1, &fixture.source, 7_186_213);
    pending::persist(&record).unwrap();
    assert!(
        dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
        "the original Herdr execution is admitted before contention"
    );
    let transition = fixture
        .shared
        .session_transition_lock(channel)
        .lock_owned()
        .await;
    let production_claim = Arc::new(synthetic_start::pending_start_claim_fn());
    let first_return = Arc::new(tokio::sync::Notify::new());
    let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
    let claim_fn: pending::ClaimFn = {
        let first_return = first_return.clone();
        Box::new(move |shared, record| {
            let production_claim = production_claim.clone();
            let first_return = first_return.clone();
            let result_tx = result_tx.clone();
            Box::pin(async move {
                let claimed = production_claim(shared, record).await;
                result_tx.send(claimed).expect("live test receiver");
                if !claimed {
                    first_return.notified().await;
                }
                claimed
            })
        })
    };
    pending::spawn_worker(
        fixture.shared.clone(),
        record.clone(),
        synthetic_start::pending_start_view_fn(),
        claim_fn,
        synthetic_start::pending_start_abort_cleanup_fn(),
        synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn(),
    );
    let first = tokio::time::timeout(Duration::from_secs(5), result_rx.recv())
        .await
        .expect("actual admitted Herdr worker attempted its production claim")
        .expect("claim result");
    assert!(
        !first,
        "the admitted Herdr worker claimed through recovery's transition"
    );
    fixture.assert_quiet().await;
    assert!(
        pending::load_all().iter().any(|saved| saved == &record),
        "the original pending source remains durable while excluded"
    );
    assert!(
        dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
        "contention did not turn admission off to manufacture the rejection"
    );
    drop(transition);
    first_return.notify_one();
    let retry = tokio::time::timeout(Duration::from_secs(5), result_rx.recv())
        .await
        .expect("the same admitted worker retried after exclusion")
        .expect("retry result");
    assert!(
        retry,
        "the valid original pin must pass once the transition is released"
    );
    let saved =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
    assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N1));
    assert_eq!(saved.user_msg_id, record.anchor_message_id);
    fixture.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_pinned_known_tmux_pending_worker_keeps_claiming_after_source_rotation_pg() {
    use dedupe::hosted_source::tests::{
        CHANNEL as SOURCE_CHANNEL, Fixture as SourceFixture, LOGICAL as SOURCE_LOGICAL,
        N1 as SOURCE_N1, N2 as SOURCE_N2,
    };

    let source_fixture = SourceFixture::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    crate::services::tmux_common::host_marker::record_tmux_host_marker(SOURCE_LOGICAL);
    let original = source_fixture.source(SOURCE_N1, "original-tmux-transcript");
    let pin = dedupe::hosted_source::capture(
        "claude",
        SOURCE_LOGICAL,
        SOURCE_CHANNEL,
        &original.path,
        Some(&original.session_id),
    )
    .unwrap();
    assert!(!pin.hosted_admission_required);
    assert!(crate::config::session_hosts::herdr_endpoint(SOURCE_CHANNEL).is_none());
    assert!(!dedupe::herdr_execution_listed(SOURCE_LOGICAL));
    dedupe::register_tmux_runtime_binding(
        SOURCE_LOGICAL,
        dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: original.path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(original.session_id.clone()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let channel = ChannelId::new(SOURCE_CHANNEL);
    let channel_key = SOURCE_CHANNEL.to_string();
    let key = crate::services::discord::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        &ProviderKind::Claude,
        SOURCE_LOGICAL,
    );
    canonical::upsert_hook_session_with_identity_pg(
        &pool,
        crate::db::dispatched_sessions::HookSessionUpsert {
            instance_id: None,
            ..canonical_fixture::params(&key, &channel_key)
        },
        Some(canonical::CanonicalSessionIdentity {
            kind: canonical::SessionIdentityKind::DiscordChannel,
            discord_token_hash: &shared.token_hash,
            channel_id: &channel_key,
        }),
    )
    .await
    .unwrap();
    assert!(canonical_fixture::raw_of(&pool, &key).await.is_none());
    let record: pending::TuiDirectPendingStart = serde_json::from_value(serde_json::json!({
        "provider": "claude", "channel_id": SOURCE_CHANNEL,
        "tmux_session_name": SOURCE_LOGICAL, "prompt_text": "an input waiting across rotation",
        "anchor_message_id": SOURCE_CHANNEL + 1, "lease_relay_owner": "tmux_watcher",
        "lease_runtime_kind": "claude_tui", "lease_turn_id": "tmux-original-observed-turn",
        "lease_session_key": key, "generation": shared.restart.current_generation,
        "created_at_ms": 0, "observed_at_ms": 0, "captured_source": null,
        "hosted_source_checked": true, "hosted_source": pin.clone()
    }))
    .unwrap();
    pending::persist(&record).unwrap();
    let next = source_fixture.source(SOURCE_N2, "rotated-tmux-transcript");
    dedupe::register_tmux_runtime_binding(
        SOURCE_LOGICAL,
        dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: next.path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(next.session_id.clone()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    shared.tmux_watchers.insert(
        channel,
        crate::services::discord::session_relay_sink::tests::idle_relay_harness::live_watcher(
            SOURCE_LOGICAL,
            next.path.to_str().unwrap(),
        ),
    );
    assert_ne!(original.path, next.path);
    assert_eq!(record.hosted_source.as_ref(), Some(&pin));
    pending::spawn_worker(
        shared.clone(),
        record.clone(),
        synthetic_start::pending_start_view_fn(),
        synthetic_start::pending_start_claim_fn(),
        synthetic_start::pending_start_abort_cleanup_fn(),
        synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn(),
    );
    let (saved, retained) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let saved =
                inflight::load_inflight_state_read_only(&ProviderKind::Claude, SOURCE_CHANNEL);
            let retained = pending::load_all()
                .into_iter()
                .find(|saved| saved.channel_id == SOURCE_CHANNEL);
            if (saved.is_some() && retained.is_none())
                || retained
                    .as_ref()
                    .is_some_and(|saved| saved.attempt_count >= 1)
            {
                break (saved, retained);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the actual deferred worker completed a production claim attempt");
    assert!(
        saved.is_some() && retained.is_none(),
        "a known non-Herdr pinned input must keep the base deferred claim after source rotation"
    );
    let saved = saved.unwrap();
    assert_eq!(saved.user_msg_id, record.anchor_message_id);
    assert_eq!(saved.output_path.as_deref(), next.path.to_str());
    assert!(saved.hosted_record_id.is_none());
    assert!(saved.hosted_execution_nonce.is_none());
    assert_eq!(
        dedupe::external_input_relay_lease("claude", SOURCE_LOGICAL, SOURCE_CHANNEL)
            .unwrap()
            .hosted_source,
        Some(pin)
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_pinned_herdr_pending_refuses_a_rotated_current_source_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(7_186_782);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    let mut record = fixture.pending(N1, &fixture.source, 7_186_783);
    record.captured_source = None;
    assert!(
        record
            .hosted_source
            .as_ref()
            .unwrap()
            .hosted_admission_required
    );
    let original_pin = record.hosted_source.clone();
    let hosted_before = fixture.hosted_json().await;
    let path = root
        .path()
        .join("rotated-source-in-the-same-hosted-execution.jsonl");
    std::fs::write(&path, b"").unwrap();
    let next = crate::services::tui_o::shadow::binding_reader::source_id_for(
        "rotated-native-session",
        &path,
    )
    .unwrap();
    fixture.publish_source(&next, N1, 2);
    assert!(dedupe::herdr_claim_admission(&fixture.logical, true).is_ok());
    assert_eq!(fixture.hosted_json().await, hosted_before);
    assert!(
        !fixture.claim(&record).await,
        "Herdr must still refuse the original pin when its current transcript rotates"
    );
    fixture.assert_quiet().await;
    assert_eq!(record.hosted_source, original_pin);
    assert_eq!(fixture.hosted_json().await, hosted_before);
    fixture.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_pending_claim_from_n1_refuses_retired_n1_and_bound_n2_but_accepts_n2_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(7_186_302);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    let original = fixture.pending(N1, &fixture.source, 7_186_303);
    let retired = fixture.replace_execution(HostedState::Retired, N1).await;
    assert!(
        !fixture.claim(&original).await,
        "the production pending claim admitted Retired N1"
    );
    fixture.assert_quiet().await;
    assert_eq!(
        fixture.hosted_json().await,
        serde_json::to_value(retired).unwrap()
    );

    let n2 = fixture.replace_execution(HostedState::Bound, N2).await;
    dedupe::install_herdr_execution(&fixture.logical, N2);
    dedupe::admit_herdr_execution(&fixture.logical, N2);
    assert!(
        dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
        "N2 is admitted, rather than pre-blocked by a false admission flag"
    );
    assert!(
        !fixture.claim(&original).await,
        "old source N1 adopted the new admitted N2 execution"
    );
    fixture.assert_quiet().await;
    assert_eq!(
        fixture.hosted_json().await,
        serde_json::to_value(n2).unwrap()
    );

    let path = root.path().join("native-n2.jsonl");
    std::fs::write(&path, b"").unwrap();
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for("native-n2", &path).unwrap();
    fixture.publish_source(&source, N2, 2);
    let current = fixture.pending(N2, &source, 7_186_304);
    fixture.publish_pending_lease(&original);
    assert!(
        fixture.claim(&current).await,
        "the valid N2 source must remain claimable"
    );
    let saved =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
    assert_eq!(saved.user_msg_id, current.anchor_message_id);
    assert_eq!(saved.output_path.as_deref(), source.path.to_str());
    assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N2));
    fixture.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_n2_pending_resumes_a_shared_native_source_without_adopting_n1_or_guessing_old_origin_pg()
 {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(7_186_632);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    let original = fixture.pending(N1, &fixture.source, 7_186_633);
    let mut old_encoded = serde_json::to_value(&original).unwrap();
    old_encoded.as_object_mut().unwrap().remove("hosted_source");
    old_encoded
        .as_object_mut()
        .unwrap()
        .remove("hosted_source_checked");
    let old: pending::TuiDirectPendingStart = serde_json::from_value(old_encoded).unwrap();
    fixture.replace_execution(HostedState::Bound, N2).await;
    fixture.publish_source(&fixture.source, N2, 2);
    assert!(
        dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
        "the resumed execution is admitted throughout"
    );
    assert!(
        !fixture.claim(&original).await,
        "an explicit original N1 pin cannot claim current N2 even when its native file is unchanged"
    );
    fixture.assert_quiet().await;
    assert!(
        !fixture.claim(&old).await,
        "an old record with the shared native file cannot guess which execution observed it"
    );
    fixture.assert_quiet().await;

    let resumed = fixture.pending(N2, &fixture.source, 7_186_634);
    fixture.publish_pending_lease(&resumed);
    assert!(
        fixture.claim(&resumed).await,
        "an explicit current N2 pin must validate within its own durable event history"
    );
    let saved =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
    assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N2));
    assert_eq!(saved.output_path.as_deref(), fixture.source.path.to_str());
    assert_eq!(
        saved.session_id.as_deref(),
        Some(fixture.source.session_id.as_str())
    );
    fixture.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_unpinned_pending_holds_a_later_n2_source_then_releases_the_input_gate_pg() {
    let root = tempfile::tempdir().unwrap();
    let log_path = root.path().join("legacy-origin-hold.log");
    crate::logging::test_capture::pin_callsite_interest();
    let _capture = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(std::fs::File::create(&log_path).unwrap())
            .finish(),
    );
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(7_186_672);
    let anchor = 7_186_673;
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    fixture.replace_execution(HostedState::Bound, N2).await;
    let path = root.path().join("later-n2-source-in-legacy-pending.jsonl");
    std::fs::write(&path, b"").unwrap();
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for("native-n2", &path).unwrap();
    fixture.publish_source(&source, N2, 2);
    let history = binding_events::records_strict(channel.get())
        .unwrap()
        .unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|event| matches!(&event.new,
                binding_events::BindingTarget::Source(found) if found == &source))
            .count(),
        1,
        "the later captured path has unique N2 history, which cannot prove the old input"
    );
    let hosted_before = fixture.hosted_json().await;
    let mut encoded = serde_json::to_value(fixture.pending(N1, &source, anchor)).unwrap();
    let fields = encoded.as_object_mut().unwrap();
    fields.remove("hosted_source");
    fields.remove("hosted_source_checked");
    assert!(!fields.contains_key("hosted_source"));
    assert!(!fields.contains_key("hosted_source_checked"));
    let old: pending::TuiDirectPendingStart = serde_json::from_value(encoded.clone()).unwrap();
    assert!(old.hosted_source.is_none());
    assert!(!old.hosted_source_checked);
    assert_eq!(
        old.captured_source.as_ref().map(|source| source.0.as_str()),
        path.to_str()
    );
    pending::persist(&old).unwrap();
    let old_path = crate::services::discord::runtime_store::tui_direct_pending_start_root()
        .unwrap()
        .join(format!("claude_{}_{anchor}.json", channel.get()));
    std::fs::write(&old_path, serde_json::to_vec(&encoded).unwrap()).unwrap();
    let restored = pending::load_all()
        .into_iter()
        .find(|record| record.channel_id == channel.get())
        .expect("the production reader restored JSON predating both source fields");
    assert_eq!(restored, old);
    assert_eq!(restored.attempt_count, 0);
    assert!(pending::pending_synthetic_start_blocks_idle_kickoff(
        "claude",
        channel.get()
    ));
    let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let production_claim = Arc::new(synthetic_start::pending_start_claim_fn());
    let claim_fn: pending::ClaimFn = {
        let entered = entered.clone();
        let completed = completed.clone();
        Box::new(move |shared, record| {
            let entered = entered.clone();
            let completed = completed.clone();
            let production_claim = production_claim.clone();
            Box::pin(async move {
                entered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let claimed = production_claim(shared, record).await;
                completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                claimed
            })
        })
    };
    pending::spawn_worker(
        fixture.shared.clone(),
        restored,
        synthetic_start::pending_start_view_fn(),
        claim_fn,
        synthetic_start::pending_start_abort_cleanup_fn(),
        synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn(),
    );
    let (saved, retained) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let saved =
                inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get());
            let retained = pending::load_all()
                .into_iter()
                .find(|record| record.channel_id == channel.get());
            if saved.is_some()
                || retained.as_ref().is_some_and(|record| {
                    record.attempt_count >= pending::PENDING_START_MAX_CLAIM_ATTEMPTS
                })
            {
                break (saved, retained);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the actual worker either claimed or exhausted all five production claim attempts");
    assert!(
        saved.is_none(),
        "legacy input adopted the later N2 source from path history alone"
    );
    fixture.assert_quiet().await;
    assert_eq!(
        entered.load(std::sync::atomic::Ordering::SeqCst),
        pending::PENDING_START_MAX_CLAIM_ATTEMPTS as usize
    );
    assert_eq!(
        completed.load(std::sync::atomic::Ordering::SeqCst),
        pending::PENDING_START_MAX_CLAIM_ATTEMPTS as usize
    );
    let logs = std::fs::read_to_string(&log_path).unwrap();
    let holds: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("legacy_herdr_pending_origin_unproven"))
        .collect();
    assert_eq!(
        holds.len(),
        1,
        "one worker reports its bounded Hold retries once"
    );
    for line in holds {
        assert!(line.contains("INFO"));
        assert!(line.contains(&format!("channel_id={}", channel.get())));
        assert!(line.contains(&format!("pending_id={anchor}")));
        assert!(line.contains("captured_path_present=true"));
    }
    let retained = retained.expect("the original unproved input remains durable");
    assert_eq!(
        retained.attempt_count,
        pending::PENDING_START_MAX_CLAIM_ATTEMPTS
    );
    assert!(retained.hosted_source.is_none());
    assert!(!retained.hosted_source_checked);
    assert_eq!(retained.captured_source, old.captured_source);
    assert_eq!(retained.lease_turn_id, old.lease_turn_id);
    assert_eq!(fixture.hosted_json().await, hosted_before);
    assert!(pending::pending_synthetic_start_present(
        "claude",
        channel.get()
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while pending::pending_synthetic_start_blocks_idle_kickoff("claude", channel.get()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the existing gate releases PRESENT after the actual worker has stopped");
    assert!(!pending::pending_synthetic_start_present(
        "claude",
        channel.get()
    ));
    assert!(pending::pending_synthetic_start_abandoned(
        "claude",
        channel.get()
    ));
    assert_eq!(
        pending::load_all()
            .into_iter()
            .find(|record| record.channel_id == channel.get()),
        Some(retained),
        "releasing the next-input gate does not discard or restamp the held old input"
    );
    let current = fixture.pending(N2, &source, anchor + 1);
    assert!(current.hosted_source_checked);
    fixture.publish_pending_lease(&current);
    assert!(
        fixture.claim(&current).await,
        "a normally proved N2 input still claims after gate release"
    );
    let saved =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
    assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N2));
    assert_eq!(saved.user_msg_id, current.anchor_message_id);
    assert!(
        old_path.exists(),
        "the unresolved old record remains available for inspection"
    );
    fixture.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn old_pending_with_an_exact_original_source_still_claims_current_n1_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(7_186_602);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    let mut original = fixture.pending(N1, &fixture.source, 7_186_603);
    original.hosted_source_checked = false;
    assert!(original.hosted_source.is_some());
    assert!(fixture.claim(&original).await);
    let before =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
    let actor = crate::services::discord::mailbox_snapshot(&fixture.shared, channel)
        .await
        .cancel_token
        .expect("the original source established its own actor");
    let active = fixture
        .shared
        .restart
        .global_active
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(before.hosted_record_id.is_some_and(|id| id > 0));
    assert_eq!(before.turn_source, inflight::TurnSource::ExternalInput);
    assert_eq!(before.external_turn_id, original.lease_turn_id);
    assert!(
        before
            .external_turn_id
            .as_ref()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(before.session_key.as_deref(), Some(fixture.key.as_str()));
    assert_eq!(before.output_path.as_deref(), fixture.source.path.to_str());
    assert_eq!(before.hosted_execution_nonce.as_deref(), Some(N1));
    assert_eq!(
        before.session_id.as_deref(),
        Some(fixture.source.session_id.as_str())
    );
    let mut encoded = serde_json::to_value(&original).unwrap();
    let encoded_fields = encoded.as_object_mut().unwrap();
    encoded_fields.remove("hosted_source");
    encoded_fields.remove("hosted_source_checked");
    let old: pending::TuiDirectPendingStart = serde_json::from_value(encoded).unwrap();
    assert!(old.hosted_source.is_none());
    assert!(!old.hosted_source_checked);
    assert!(
        fixture.claim(&old).await,
        "the original inflight episode independently proves this old record's execution"
    );
    let saved =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
    assert_eq!(saved.user_msg_id, old.anchor_message_id);
    assert_eq!(saved.output_path.as_deref(), fixture.source.path.to_str());
    assert_eq!(saved.provider, before.provider);
    assert_eq!(saved.channel_id, before.channel_id);
    assert_eq!(saved.tmux_session_name, before.tmux_session_name);
    assert_eq!(saved.turn_nonce, before.turn_nonce);
    assert_eq!(saved.external_turn_id, before.external_turn_id);
    assert_eq!(saved.session_key, before.session_key);
    assert_eq!(saved.hosted_record_id, before.hosted_record_id);
    assert_eq!(saved.hosted_execution_nonce, before.hosted_execution_nonce);
    assert_eq!(saved.session_id, before.session_id);
    assert_eq!(saved.turn_start_offset, before.turn_start_offset);
    assert_eq!(saved.last_offset, before.last_offset);
    assert_eq!(saved.response_sent_offset, before.response_sent_offset);
    let readopted = crate::services::discord::mailbox_snapshot(&fixture.shared, channel)
        .await
        .cancel_token
        .expect("re-adoption retained the original actor");
    assert!(Arc::ptr_eq(&actor, &readopted));
    assert_eq!(
        fixture
            .shared
            .restart
            .global_active
            .load(std::sync::atomic::Ordering::Relaxed),
        active
    );
    fixture.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn old_pending_without_original_source_holds_without_blocking_valid_n2_pg() {
    old_pending_does_not_block_n2(false, 7_186_612).await;
}

#[tokio::test(flavor = "current_thread")]
async fn old_pending_with_ambiguous_original_source_holds_without_blocking_valid_n2_pg() {
    old_pending_does_not_block_n2(true, 7_186_622).await;
}

#[tokio::test(flavor = "current_thread")]
async fn stale_pending_cannot_replace_a_live_n2_lease_before_its_claim_is_refused_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(7_186_642);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    let stale = fixture.pending(N1, &fixture.source, 7_186_643);
    let mut unknown_encoded = serde_json::to_value(&stale).unwrap();
    unknown_encoded
        .as_object_mut()
        .unwrap()
        .remove("hosted_source");
    unknown_encoded
        .as_object_mut()
        .unwrap()
        .remove("hosted_source_checked");
    unknown_encoded["captured_source"] = serde_json::Value::Null;
    let unknown: pending::TuiDirectPendingStart = serde_json::from_value(unknown_encoded).unwrap();

    fixture.replace_execution(HostedState::Bound, N2).await;
    let path = root.path().join("live-n2-before-stale-pending.jsonl");
    std::fs::write(&path, b"").unwrap();
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for("native-n2", &path).unwrap();
    fixture.publish_source(&source, N2, 2);
    let current = fixture.pending(N2, &source, 7_186_644);
    let current_lease = fixture.publish_pending_lease(&current);
    assert_eq!(
        current_lease
            .hosted_source
            .as_ref()
            .map(|pin| pin.execution_nonce.as_str()),
        Some(N2)
    );
    assert!(
        dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
        "N2 is admitted before either stale pending attempts its claim"
    );

    for rejected in [&stale, &unknown] {
        assert!(
            !fixture.claim(rejected).await,
            "a stale N1 or unknown old origin cannot claim the current N2 execution"
        );
        fixture.assert_quiet().await;
        assert_eq!(
            dedupe::external_input_relay_lease("claude", &fixture.logical, channel.get()),
            Some(current_lease.clone()),
            "a refused pending replaced the live N2 lease identity or original source pin"
        );
    }

    assert!(
        fixture.claim(&current).await,
        "the existing N2 lease remains claimable without being republished after stale attempts"
    );
    let saved =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
    assert_eq!(saved.user_msg_id, current.anchor_message_id);
    assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N2));
    assert_eq!(saved.output_path.as_deref(), source.path.to_str());
    fixture.close().await;
}

async fn old_pending_does_not_block_n2(ambiguous: bool, id: u64) {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(id);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    let mut encoded = serde_json::to_value(fixture.pending(N1, &fixture.source, id + 1)).unwrap();
    encoded.as_object_mut().unwrap().remove("hosted_source");
    encoded
        .as_object_mut()
        .unwrap()
        .remove("hosted_source_checked");
    if ambiguous {
        fixture.append_source_event(&fixture.source, "ambiguous-original-execution", 2);
    } else {
        encoded["captured_source"] = serde_json::Value::Null;
    }
    let old: pending::TuiDirectPendingStart = serde_json::from_value(encoded).unwrap();
    pending::persist(&old).unwrap();
    assert!(
        dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
        "the old execution is admitted before the source-origin check"
    );
    assert!(
        !fixture.claim(&old).await,
        "old Herdr pending with no unique original execution must hold"
    );
    fixture.assert_quiet().await;
    assert!(
        pending::load_all().iter().any(|record| record == &old),
        "the original unresolved input was retained"
    );

    fixture.replace_execution(HostedState::Bound, N2).await;
    let path = root.path().join(format!("{id}-native-n2.jsonl"));
    std::fs::write(&path, b"").unwrap();
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for("native-n2", &path).unwrap();
    fixture.publish_source(&source, N2, if ambiguous { 3 } else { 2 });
    let current = fixture.pending(N2, &source, id + 2);
    fixture.publish_pending_lease(&current);
    assert!(
        fixture.claim(&current).await,
        "an unresolved old pending must not globally block a valid new execution"
    );
    let saved = inflight::load_inflight_state_read_only(&ProviderKind::Claude, id).unwrap();
    assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N2));
    assert_eq!(saved.user_msg_id, current.anchor_message_id);
    assert!(
        pending::load_all().iter().any(|record| record == &old),
        "the valid N2 claim did not need to delete the unresolved old pending"
    );
    fixture.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn hosted_row_change_after_actor_admission_refuses_the_production_save_pg() {
    post_admission_change(false, 7_186_402).await;
}

#[tokio::test(flavor = "current_thread")]
async fn admitted_nonce_change_after_actor_admission_refuses_the_production_save_pg() {
    post_admission_change(true, 7_186_412).await;
}

async fn post_admission_change(change_admitted_nonce: bool, id: u64) {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(id);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    let original = fixture.pending(N1, &fixture.source, id + 1);
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    *synthetic_start::bridge_handoff::ADMISSION_PAUSE
        .lock()
        .unwrap() = Some((id, entered.clone(), resume.clone()));
    let change = async {
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("the valid original hosted claim actually admitted its actor");
        let actor = crate::services::discord::mailbox_snapshot(&fixture.shared, channel).await;
        assert_eq!(
            actor.active_user_message_id.map(MessageId::get),
            Some(original.anchor_message_id)
        );
        assert!(actor.cancel_token.is_some());
        assert!(inflight::load_inflight_state_read_only(&ProviderKind::Claude, id).is_none());
        let expected = if change_admitted_nonce {
            dedupe::install_herdr_execution(&fixture.logical, N2);
            dedupe::admit_herdr_execution(&fixture.logical, N2);
            fixture.hosted_json().await
        } else {
            serde_json::to_value(fixture.replace_execution(HostedState::Bound, N2).await).unwrap()
        };
        assert!(
            dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
            "the changed execution's admission stays true"
        );
        resume.notify_one();
        expected
    };
    let (claimed, expected) = tokio::join!(fixture.claim(&original), change);
    assert!(
        !claimed,
        "the original hosted identity changed after actor claim but before save"
    );
    fixture.assert_quiet().await;
    assert_eq!(
        fixture.hosted_json().await,
        expected,
        "the foreign execution was not rewritten by rollback"
    );
    fixture.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn restored_provider_same_transcript_human_prompt_is_not_relayed_or_synthetic_claimed_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(7_263_001);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    let source_before = fixture.source.clone();
    let hosted_before = fixture.hosted_json().await;
    assert!(
        crate::services::tui_prompt_dedupe::hosted_source::current_execution_for_adoption(
            &fixture.shared,
            "claude",
            channel,
            &fixture.logical
        )
        .await,
        "matched_live_execution_adoption_positive_contract"
    );
    let context = fixture.rig.context(N1);
    fixture.rig.restart_shell(&context);
    use std::io::Write as _;
    writeln!(std::fs::OpenOptions::new().append(true).open(&fixture.source.path).unwrap(), "{}", serde_json::json!({"type":"user", "sessionId":fixture.source.session_id, "message":{"role":"user","content":"human on restored provider"}})).unwrap();
    let record = fixture.pending(N1, &fixture.source, 7_263_002);
    let (rest, _http) =
        crate::services::discord::shared_state::test_rest::recording_mock(7_263_010, channel.get())
            .await;
    let observed = ObservedTuiPrompt {
        provider: "claude".into(),
        tmux_session_name: fixture.logical.clone(),
        prompt: "human on restored provider".into(),
        source_event_id: Some("restored-same-transcript".into()),
        observed_at: chrono::Utc::now(),
        external_input_lease_generation: 0,
        ssh_direct_observation_generation: 0,
        hook_prompt_id: None,
        row_prompt_id: None,
        native_turn_id: Some("restored-native-turn".into()),
        steer_echo: false,
        hosted_source: record.hosted_source.clone(),
    };
    super::super::super::relay_observed_prompt(&fixture.shared, observed).await;
    assert!(
        rest.lock().unwrap().is_empty(),
        "restored_same_transcript_relay_zero_contract"
    );
    assert!(
        !fixture.claim(&record).await,
        "restored_same_transcript_synthetic_zero_contract"
    );
    fixture.assert_quiet().await;
    assert_eq!(
        fixture.hosted_json().await,
        hosted_before,
        "read-only admission must not retire or adopt the restored provider"
    );
    assert_eq!(fixture.source, source_before);
    assert!(
        fixture.rig.sends().is_empty(),
        "restored provider receives no input or cleanup"
    );
    fixture.close().await;
}
