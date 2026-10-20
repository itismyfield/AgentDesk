//! Hosted synthetic claims keep their observed execution across deferred and post-claim races.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_sessions::hosted_execution::{
    ExpectedExecution, HostedExecution, HostedLocation, HostedOwner, HostedState, ProcessStamp,
    SourceRef,
};
use crate::services::discord::{inflight, tui_direct_pending_start as pending};
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events};
use std::io::Write;

const NODE: &str = "synthetic-recovery-test-node";
const N1: &str = "synthetic-execution-n1";
const N2: &str = "synthetic-execution-n2";

struct Fixture {
    db: TestPostgresDb,
    pool: sqlx::PgPool,
    shared: Arc<SharedData>,
    channel: ChannelId,
    logical: String,
    key: String,
    execution: HostedExecution,
    source: binding_events::SourceId,
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
        let location = HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: endpoint.key,
            socket_addr: endpoint.socket_path.display().to_string(),
            named_session: endpoint.herdr_session,
            pane_id: "test-window:test-pane".into(),
        };
        let execution = HostedExecution {
            schema: 1,
            state: HostedState::Bound,
            execution_nonce: N1.into(),
            owner: owner.clone(),
            location: Some(location),
            expected: Some(ExpectedExecution {
                binding_provider: "claude".into(),
                binding_nonce: N1.into(),
                root: ProcessStamp {
                    pid: 100,
                    start: "linux-ticks:100".into(),
                },
                provider_process: ProcessStamp {
                    pid: 101,
                    start: "linux-ticks:101".into(),
                },
                provenance: "isolated recorded launch".into(),
            }),
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
        sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)")
            .bind(&key).bind(&shared.token_hash).bind(channel.get().to_string())
            .bind(serde_json::to_value(&execution).unwrap()).execute(&pool).await.unwrap();
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
        next.expected.as_mut().unwrap().binding_nonce = nonce.into();
        sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE session_key = $1")
            .bind(&self.key)
            .bind(serde_json::to_value(&next).unwrap())
            .execute(&self.pool)
            .await
            .unwrap();
        next
    }

    async fn hosted_json(&self) -> serde_json::Value {
        sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE session_key = $1")
            .bind(&self.key)
            .fetch_one(&self.pool)
            .await
            .unwrap()
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
    super::super::hook_observer::spawn_tui_prompt_relay_observer(
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
async fn old_pending_with_an_exact_original_source_still_claims_current_n1_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(7_186_602);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    let mut encoded =
        serde_json::to_value(fixture.pending(N1, &fixture.source, 7_186_603)).unwrap();
    encoded.as_object_mut().unwrap().remove("hosted_source");
    let old: pending::TuiDirectPendingStart = serde_json::from_value(encoded).unwrap();
    assert!(
        fixture.claim(&old).await,
        "an old record's exact original source proves its original execution"
    );
    let saved =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
    assert_eq!(saved.user_msg_id, old.anchor_message_id);
    assert_eq!(saved.output_path.as_deref(), fixture.source.path.to_str());
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
