use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLocation, HostedLookupKey, HostedOwner, SourceRef, load_hosted_execution_pg,
};
use crate::services::discord::{
    ChannelId, make_shared_data_for_tests_with_storage,
    recovery_engine::herdr_reader::reconnect_restarted_herdr_with_shared,
};
use crate::services::provider::ProviderKind;
use crate::services::session_host::herdr_socket_rig_tests::{
    HerdrRequest, HerdrRig, NODE, PANE, PROVIDER, SHELL,
};
use crate::services::tmux_common::session_temp_path;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};
use crate::services::tui_prompt_dedupe::binding_events::{
    self, CauseSource, HookSignal, Proposal, SourceId,
};
use crate::services::tui_prompt_dedupe::{self as dedupe, runtime_binding_for_tmux_session};

pub(in crate::services::discord) const CHANNEL: u64 = 82801;
const NONCE: &str = "0123456789abcdef0123456789abcdef";
const SUCCESSOR: &str = "fedcba9876543210fedcba9876543210";

pub(in crate::services::discord) struct Fixture {
    pub(in crate::services::discord) rig: Arc<HerdrRig>,
    pub(in crate::services::discord) shared: Arc<SharedData>,
    pub(in crate::services::discord) record: HostedExecution,
    pool: PgPool,
    row: HostedObservation,
    source: SourceId,
    context_path: PathBuf,
    db: TestPostgresDb,
}

impl Fixture {
    async fn new(rig: HerdrRig, provider: ProviderKind, tag: &str, busy: bool) -> Self {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let mut shared = make_shared_data_for_tests_with_storage(Some(pool.clone()));
        Arc::get_mut(&mut shared).unwrap().provider = provider.clone();
        let runtime = crate::config::runtime_root().unwrap();
        let owner = HostedOwner {
            provider: provider.as_str().into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: CHANNEL.to_string(),
            logical_key: format!("AgentDesk-{}-reattach-{tag}", provider.as_str()),
            owner_node: NODE.into(),
            runtime_root: runtime.display().to_string(),
        };
        let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
        let mut expected = rig.expected(NONCE);
        expected.binding_provider.clone_from(&owner.provider);
        let record = HostedExecution {
            schema: 1,
            state: HostedState::Bound,
            execution_nonce: NONCE.into(),
            location: Some(HostedLocation {
                host: "herdr".into(),
                execution_node: NODE.into(),
                endpoint_config_key: endpoint.key,
                socket_addr: endpoint.socket_path.display().to_string(),
                named_session: endpoint.herdr_session,
                pane_id: PANE.into(),
            }),
            expected: Some(expected),
            source_ref: SourceRef {
                runtime_root: owner.runtime_root.clone(),
                channel: owner.channel_id.clone(),
                provider: owner.provider.clone(),
                logical_key: owner.logical_key.clone(),
                execution_nonce: NONCE.into(),
                initial_source: None,
                baseline_event_seq: None,
            },
            owner,
        };
        let owner = &record.owner;
        sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, $2, 'idle', 'discord_channel', $3, $4, $5)")
            .bind(&owner.logical_key)
            .bind(&owner.provider)
            .bind(&owner.discord_token_hash)
            .bind(&owner.channel_id)
            .bind(serde_json::to_value(&record).unwrap())
            .execute(&pool).await.unwrap();
        let row = read_row(&pool, &owner.logical_key).await;

        let session = uuid::Uuid::new_v4().to_string();
        let path = runtime.join(format!("rollout-{tag}-{session}.jsonl"));
        let initial = match provider {
            ProviderKind::Codex => serde_json::json!({
                "type": "session_meta", "payload": {"id": session, "source": "cli"},
            }),
            _ => serde_json::json!({"type": "system", "sessionId": session}),
        };
        let tail = match (&provider, busy) {
            (ProviderKind::Codex, true) => serde_json::json!({
                "type": "event_msg", "payload": {"type": "task_started", "turn_id": "one"},
            }),
            (ProviderKind::Codex, false) => serde_json::json!({
                "type": "event_msg", "payload": {"type": "task_complete", "turn_id": "one"},
            }),
            (_, true) => serde_json::json!({
                "type": "user", "sessionId": session, "message": {"role": "user", "content": "질문"},
            }),
            (_, false) => serde_json::json!({
                "type": "assistant", "sessionId": session, "message": {"role": "assistant", "stop_reason": "end_turn", "content": [{"type": "text", "text": "답"}]},
            }),
        };
        std::fs::write(&path, format!("{initial}\n{tail}\n")).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let source = SourceId {
            session_id: session,
            path,
            dev: meta.dev(),
            ino: meta.ino(),
        };
        let context = PreparedIncarnation::create(BindingContext {
            schema: 1,
            provider: owner.provider.clone(),
            created_at: chrono::Utc::now(),
            execution_nonce: NONCE.into(),
            tmux_session: owner.logical_key.clone(),
            channel_id: Some(CHANNEL),
            owner_runtime_root: owner.runtime_root.clone(),
            host: Some("herdr".into()),
            expected_native_session_id: Some(source.session_id.clone()),
            launch_mode: "fresh".into(),
            provider_root: None,
            first_prompt_digest: None,
            source_policy: None,
            resume_start: None,
        })
        .unwrap();
        rig.run_provider(&context.path, false);
        rig.show_panes(&[PANE]);
        std::fs::write(session_temp_path(&owner.logical_key, "host_kind"), "herdr").unwrap();
        std::fs::write(session_temp_path(&owner.logical_key, "spawn_nonce"), NONCE).unwrap();
        let proposal = Proposal {
            channel_id: CHANNEL,
            provider: &owner.provider,
            tmux_session: &owner.logical_key,
            session_id: Some(&source.session_id),
            path: source.path.to_str().unwrap(),
            replaced: None,
            cause: CauseSource::Launch,
            hook: None,
        };
        assert_eq!(
            binding_events::record_verified(&proposal, &source).unwrap(),
            binding_events::Committed::Appended,
        );
        assert_eq!(
            nonce_baseline(CHANNEL, &owner.logical_key, NONCE),
            Some(source.clone())
        );
        Self {
            rig: Arc::new(rig),
            shared,
            record,
            pool,
            row,
            source,
            context_path: context.path,
            db,
        }
    }

    pub(in crate::services::discord) fn logical(&self) -> &str {
        &self.record.owner.logical_key
    }

    fn owner(&self) -> Option<ChannelId> {
        self.shared
            .tmux_watchers
            .owner_channel_for_tmux_session(self.logical())
    }

    fn legacy_boot(&self, runtime: &tokio::runtime::Runtime) {
        let runtime_scope = runtime.enter();
        reconnect_restarted_herdr_with_shared(&self.shared, &self.shared.provider);
        drop(runtime_scope);
    }

    async fn verify(&self, row: &HostedObservation) -> VerifiedReattach {
        let lease =
            SessionTransitionLease::try_acquire(&self.shared, ChannelId::new(CHANNEL)).unwrap();
        let mut checks = Verification::default();
        match std::env::var("ADK_HERDR_REATTACH_TEST_MODE").as_deref() {
            Ok("before") => {
                return match reconnect_source(&self.pool, row, None).await {
                    RestartAttachment::Legacy(HerdrSourceAttach::Published { .. }) => {
                        VerifiedReattach::Live
                    }
                    _ => VerifiedReattach::Held,
                };
            }
            Ok("always-live") => checks.live = |_| true,
            Ok("never-live") => checks.live = |_| false,
            Ok("ignore-nonce") => {
                checks.stale = |_, marker| matches!(marker, SpawnNonceMarker::Unreadable)
            }
            Ok("shell-is-ended") => checks.ended = HerdrPaneView::ended,
            Ok("omit-row") => checks.omit = Some("row"),
            Ok("omit-epoch") => checks.omit = Some("epoch"),
            Ok("omit-last-pane") => checks.omit = Some("last-pane"),
            Err(std::env::VarError::NotPresent) => {
                let RestartAttachment::Verified(result) = reconnect_source(
                    &self.pool,
                    row,
                    Some(VerifiedTarget {
                        shared: &self.shared,
                        transition: &lease,
                    }),
                )
                .await
                else {
                    panic!("verified target did not select its path")
                };
                return result;
            }
            mode => panic!("unknown reattach test mode: {mode:?}"),
        }
        verify_with(&self.shared, &self.pool, &lease, row, checks).await
    }

    fn assert_unpublished(&self) {
        assert_eq!(self.owner(), None);
        assert!(dedupe::owner_channel_for_tmux_session(self.logical()).is_none());
        assert!(runtime_binding_for_tmux_session(self.logical()).is_none());
        assert!(self.rig.sends().is_empty());
    }

    fn on_first_process_read(&self, action: impl Fn() + Send + Sync + 'static) {
        let done = AtomicBool::new(false);
        self.rig.on_reply(move |call| {
            if matches!(call.request, HerdrRequest::PaneProcessInfo { .. })
                && !done.swap(true, Ordering::SeqCst)
            {
                action();
            }
        });
    }

    async fn save(&self, record: &HostedExecution) -> HostedObservation {
        sqlx::query("UPDATE sessions SET hosted_execution = $1 WHERE id = $2")
            .bind(serde_json::to_value(record).unwrap())
            .bind(self.row.session_id())
            .execute(&self.pool)
            .await
            .unwrap();
        read_row(&self.pool, self.logical()).await
    }

    pub(in crate::services::discord) async fn close(self) {
        drop(self.shared);
        self.pool.close().await;
        self.db.drop().await;
    }
}

struct BindingLogRoot(Option<PathBuf>);

impl Drop for BindingLogRoot {
    fn drop(&mut self) {
        binding_events::set_test_root(self.0.as_deref());
    }
}

async fn read_row(pool: &PgPool, key: &str) -> HostedObservation {
    match load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await {
        HostedLookup::Found(row) => row,
        found => panic!("{found:?}"),
    }
}

pub(in crate::services::discord) fn run(
    provider: ProviderKind,
    tag: &str,
    busy: bool,
    verify: impl FnOnce(Fixture, &tokio::runtime::Runtime),
) {
    let test_root = crate::config::TestRuntimeRootGuard::new();
    let state = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let log_root = BindingLogRoot(binding_events::test_root());
    binding_events::set_test_root(Some(test_root.path()));
    let stop = test_root.path().join("unwritten-admission-stop");
    let admission = crate::services::herdr_admission::force_for_test(
        crate::services::herdr_admission::Admission::new(true, Some(stop.clone())),
    );
    let rig = HerdrRig::start();
    let mut config = crate::config::Config::default();
    config.cluster.instance_id = Some(NODE.into());
    config.agents.push(serde_json::from_value(serde_json::json!({
        "id": "reattach", "name": "Reattach", "provider": provider.as_str(),
        "channels": {(provider.as_str()): {"id": CHANNEL.to_string(), "name": "reattach", "runtime": "tui"}}
    })).unwrap());
    let herdr = &mut config.session_hosts.herdr;
    herdr.endpoints.insert(
        "fixture".to_owned(),
        crate::config::session_hosts::EndpointConfig {
            mode: crate::config::session_hosts::EndpointMode::LegacyDedicated,
            execution_node: NODE.into(),
            socket_path: rig.socket().to_owned(),
            herdr_home: "/adk/herdr".into(),
            herdr_session: "agentdesk".into(),
        },
    );
    herdr
        .channels
        .insert(CHANNEL.to_string(), "fixture".to_owned());
    let thread_config = config.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .on_thread_start(move || {
            std::mem::forget(crate::services::herdr_admission::force_for_test(
                crate::services::herdr_admission::Admission::new(true, Some(stop.clone())),
            ));
            std::mem::forget(crate::config::session_hosts::force_config_for_test(
                &thread_config,
            ));
        })
        .build()
        .unwrap();
    let hosts = crate::config::session_hosts::force_config_for_test(&config);
    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
    let registry = rig.registry_as_on_this_thread(
        NODE,
        &endpoint.key,
        &endpoint.socket_path,
        &endpoint.herdr_session,
    );
    let codex = crate::services::turn_host::force_codex_switch_for_test(Some(true));
    let fixture = runtime.block_on(Fixture::new(rig, provider, tag, busy));
    verify(fixture, &runtime);
    drop(runtime);
    drop((
        codex, registry, hosts, log_root, state, admission, test_root,
    ));
}

fn live_case(provider: ProviderKind, busy: bool, boot_first: bool, tag: &str) {
    run(provider, tag, busy, |fx, rt| {
        use crate::services::discord::recovery_engine::herdr_reader::ATTACHES;
        ATTACHES.set(0);
        if boot_first {
            fx.legacy_boot(rt);
            assert_eq!(
                fx.owner(),
                Some(ChannelId::new(CHANNEL)),
                "boot publishes the same owner before the first input"
            );
        }
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Live);
        assert_eq!(fx.owner(), Some(ChannelId::new(CHANNEL)));
        let binding = runtime_binding_for_tmux_session(fx.logical()).unwrap();
        assert_eq!(
            binding.session_id.as_deref(),
            Some(fx.source.session_id.as_str())
        );
        assert_eq!(binding.output_path, fx.source.path.to_str().unwrap());
        use std::io::Write;
        let mut transcript = std::fs::OpenOptions::new()
            .append(true)
            .open(&fx.source.path)
            .unwrap();
        writeln!(transcript, "{{}}").unwrap();
        let advanced = transcript.metadata().unwrap().len();
        assert!(advanced > binding.last_offset);
        assert!(dedupe::advance_tmux_runtime_binding_offset(
            fx.logical(),
            &binding.output_path,
            advanced
        ));
        let logs_before = binding_events::binding_events_since(CHANNEL, 0).unwrap();
        if !boot_first {
            fx.legacy_boot(rt);
        }
        assert_eq!(ATTACHES.get(), usize::from(boot_first));
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Live);
        let after = runtime_binding_for_tmux_session(fx.logical()).unwrap();
        assert_eq!(after.last_offset, advanced);
        assert_eq!(
            binding_events::binding_events_since(CHANNEL, 0).unwrap(),
            logs_before
        );
        assert_eq!(rt.block_on(read_row(&fx.pool, fx.logical())), fx.row);
        assert!(fx.rig.sends().is_empty());
        rt.block_on(fx.close());
    });
}

#[test]
fn claude_idle_boot_before_intake_pg() {
    live_case(ProviderKind::Claude, false, true, "claude-idle");
}

#[test]
fn claude_busy_intake_before_boot_pg() {
    live_case(ProviderKind::Claude, true, false, "claude-busy");
}

#[test]
fn codex_idle_intake_before_boot_pg() {
    live_case(ProviderKind::Codex, false, false, "codex-idle");
}

#[test]
fn codex_busy_boot_before_intake_pg() {
    live_case(ProviderKind::Codex, true, true, "codex-busy");
}

#[test]
fn missing_and_exited_are_ended_without_retirement_pg() {
    run(ProviderKind::Claude, "ended", false, |fx, rt| {
        fx.rig.show_panes(&[]);
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Ended);
        fx.rig.show_panes(&[PANE]);
        fx.rig.foreground(&[SHELL]);
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Held);
        fx.assert_unpublished();
        fx.rig.exists(PROVIDER, false);
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Ended);
        assert_eq!(rt.block_on(read_row(&fx.pool, fx.logical())), fx.row);
        assert_eq!(fx.owner(), None);
        assert!(runtime_binding_for_tmux_session(fx.logical()).is_none());
        assert!(fx.rig.sends().is_empty());
        rt.block_on(fx.close());
    });
}

#[test]
fn unknown_unplaced_and_replaced_execution_do_not_publish_pg() {
    run(ProviderKind::Claude, "withheld", false, |fx, rt| {
        fx.rig.serve_as(&[7, 8, 7]);
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Held);
        fx.rig.serve_as(&[7]);
        fx.rig.restart_shell(&fx.context_path);
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Held);

        let mut wrong = fx.record.clone();
        wrong
            .location
            .as_mut()
            .unwrap()
            .endpoint_config_key
            .push_str("-other");
        let row = rt.block_on(fx.save(&wrong));
        assert_eq!(rt.block_on(fx.verify(&row)), VerifiedReattach::Held);
        wrong = fx.record.clone();
        wrong.state = HostedState::Pending;
        let row = rt.block_on(fx.save(&wrong));
        assert_eq!(rt.block_on(fx.verify(&row)), VerifiedReattach::Held);
        wrong.location = None;
        wrong.expected = None;
        let row = rt.block_on(fx.save(&wrong));
        assert_eq!(rt.block_on(fx.verify(&row)), VerifiedReattach::Held);
        let unknown = rt.block_on(async {
            sqlx::query("UPDATE sessions SET hosted_execution = $1 WHERE id = $2")
                .bind(serde_json::json!({"pane_id":PANE, "launch_nonce":NONCE}))
                .bind(fx.row.session_id())
                .execute(&fx.pool)
                .await
                .unwrap();
            read_row(&fx.pool, fx.logical()).await
        });
        assert!(matches!(unknown.record, HostedRecord::Unknown(_)));
        assert_eq!(rt.block_on(fx.verify(&unknown)), VerifiedReattach::Held);
        assert_eq!(fx.owner(), None);
        assert!(runtime_binding_for_tmux_session(fx.logical()).is_none());
        assert!(fx.rig.sends().is_empty());
        rt.block_on(fx.close());
    });
}

#[test]
fn changed_nonce_after_probe_and_stale_row_do_not_publish_pg() {
    run(ProviderKind::Claude, "nonce-race", false, |fx, rt| {
        let marker = session_temp_path(fx.logical(), "spawn_nonce");
        let late_marker = marker.clone();
        fx.rig.on_reply(move |call| {
            if matches!(call.request, crate::services::session_host::herdr_socket_rig_tests::HerdrRequest::PaneProcessInfo { .. }) {
                std::fs::write(&late_marker, SUCCESSOR).unwrap();
            }
        });
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Held);
        assert_eq!(fx.owner(), None);
        assert!(dedupe::owner_channel_for_tmux_session(fx.logical()).is_none());
        assert!(runtime_binding_for_tmux_session(fx.logical()).is_none());

        // A stale Missing answer also cannot lend the predecessor's identity to a new launch.
        fx.rig.show_panes(&[]);
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Held);
        fx.rig.show_panes(&[PANE]);

        let mut successor = fx.record.clone();
        successor.execution_nonce = SUCCESSOR.into();
        successor.source_ref.execution_nonce = SUCCESSOR.into();
        successor.expected = Some(fx.rig.expected(SUCCESSOR));
        let current = rt.block_on(fx.save(&successor));
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Held);
        assert_eq!(rt.block_on(read_row(&fx.pool, fx.logical())), current);
        assert_eq!(fx.owner(), None);
        assert!(fx.rig.sends().is_empty());
        rt.block_on(fx.close());
    });
}

#[test]
fn database_only_successor_during_probe_refuses_publication_pg() {
    run(ProviderKind::Claude, "db-race", false, |fx, rt| {
        let marker = session_temp_path(fx.logical(), "spawn_nonce");
        let mut successor = fx.record.clone();
        successor.execution_nonce = SUCCESSOR.into();
        successor.source_ref.execution_nonce = SUCCESSOR.into();
        successor.expected = Some(fx.rig.expected(SUCCESSOR));
        let pool = fx.pool.clone();
        let row_id = fx.row.session_id();
        let handle = rt.handle().clone();
        fx.on_first_process_read(move || {
            handle
                .block_on(
                    sqlx::query("UPDATE sessions SET hosted_execution = $1 WHERE id = $2")
                        .bind(serde_json::to_value(&successor).unwrap())
                        .bind(row_id)
                        .execute(&pool),
                )
                .unwrap();
        });
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Held);
        assert_eq!(std::fs::read_to_string(marker).unwrap(), NONCE);
        assert_ne!(rt.block_on(read_row(&fx.pool, fx.logical())), fx.row);
        fx.assert_unpublished();
        rt.block_on(fx.close());
    });
}

#[test]
fn finished_clear_during_probe_refuses_publication_pg() {
    run(ProviderKind::Claude, "epoch-race", false, |fx, rt| {
        let token = fx.record.owner.discord_token_hash.clone();
        fx.on_first_process_read(move || {
            drop(clear_epoch::begin_clear("claude", &token, CHANNEL));
        });
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Held);
        fx.assert_unpublished();
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Live);
        rt.block_on(fx.close());
    });
}

#[test]
fn pane_disappearing_after_first_observation_does_not_publish_pg() {
    run(ProviderKind::Claude, "pane-race", false, |fx, rt| {
        let present = serde_json::json!({"type":"session_snapshot", "snapshot":{
            "version":"0.9.3", "protocol":22, "workspaces":[], "tabs":[], "layouts":[], "agents":[],
            "panes":[{"pane_id":PANE, "terminal_id":"t1", "workspace_id":"w1",
                "tab_id":"w1:1", "focused":false, "agent_status":"idle", "revision":7}]}});
        let mut missing = present.clone();
        missing["snapshot"]["panes"] = serde_json::json!([]);
        fx.rig
            .answer_sequence("session.snapshot", [present, missing]);
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Held);
        fx.assert_unpublished();
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Ended);
        rt.block_on(fx.close());
    });
}

#[test]
fn same_nonce_clear_preserves_input_admission_without_old_binding_pg() {
    run(ProviderKind::Claude, "awaited-clear", false, |fx, rt| {
        let new_session = uuid::Uuid::new_v4().to_string();
        let path = fx
            .source
            .path
            .with_file_name(format!("{new_session}.jsonl"));
        let hook =
            HookSignal::from_payload("session_start", &serde_json::json!({"source":"clear"}));
        let proposal = Proposal {
            channel_id: CHANNEL,
            provider: "claude",
            tmux_session: fx.logical(),
            session_id: Some(&new_session),
            path: path.to_str().unwrap(),
            replaced: None,
            cause: CauseSource::Hook(hook.cause()),
            hook: Some(&hook),
        };
        binding_events::record_pending(&proposal).unwrap();
        assert_eq!(
            rt.block_on(fx.verify(&fx.row)),
            VerifiedReattach::AwaitingClear
        );
        assert_eq!(fx.owner(), Some(ChannelId::new(CHANNEL)));
        assert!(runtime_binding_for_tmux_session(fx.logical()).is_none());
        assert!(dedupe::herdr_claim_admission(fx.logical(), true).is_ok());
        assert!(fx.rig.sends().is_empty());
        rt.block_on(fx.close());
    });
}

#[test]
fn cold_codex_scan_leaves_other_registry_writers_unblocked_pg() {
    use crate::services::discord::tui_prompt_relay::herdr_source::BEFORE_CODEX_RESTART_SCAN;
    run(ProviderKind::Codex, "codex-scan", false, |fx, rt| {
        let shared = fx.shared.clone();
        let channel = ChannelId::new(CHANNEL + 1);
        let (scanning, begin_write) = std::sync::mpsc::sync_channel(0);
        let (wrote, write_done) = std::sync::mpsc::sync_channel(1);
        let writer = std::thread::spawn(move || {
            begin_write.recv_timeout(Duration::from_secs(5)).unwrap();
            let published = shared
                .tmux_watchers
                .restore_owner_channel_for_tmux_session("codex-other-restart-scan", channel);
            wrote.send(published).unwrap();
        });
        BEFORE_CODEX_RESTART_SCAN.with(|hook| {
            hook.replace(Some(Box::new(move || {
                scanning.send(()).unwrap();
                assert!(write_done.recv_timeout(Duration::from_secs(5)).unwrap());
            })))
        });
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Live);
        crate::test_support::wait::join_within(writer, Duration::from_secs(5));
        assert_eq!(fx.owner(), Some(ChannelId::new(CHANNEL)));
        rt.block_on(fx.close());
    });
}

#[test]
fn conflicting_registry_refuses_publication_pg() {
    run(
        ProviderKind::Claude,
        "publication-fence",
        false,
        |fx, rt| {
            let foreign = ChannelId::new(CHANNEL + 1);
            assert!(
                fx.shared
                    .tmux_watchers
                    .restore_owner_channel_for_tmux_session(fx.logical(), foreign)
            );
            assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Held);
            assert_eq!(fx.owner(), Some(foreign));
            assert!(dedupe::owner_channel_for_tmux_session(fx.logical()).is_none());
            assert!(runtime_binding_for_tmux_session(fx.logical()).is_none());
            assert!(fx.rig.sends().is_empty());
            rt.block_on(fx.close());
        },
    );
}

fn snapshot_labels(fx: &Fixture, labels: &[String]) {
    let workspaces: Vec<_> = labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            serde_json::json!({"workspace_id": format!("w{}", index + 1), "label": label,
            "pane_count": u32::from(index == 0), "tab_count": 1})
        })
        .collect();
    fx.rig.answer(
        "session.snapshot",
        serde_json::json!({"type":"session_snapshot","snapshot":{
            "version":"0.9.3", "protocol":22, "workspaces":workspaces,
            "panes":[{"pane_id":PANE,"terminal_id":"t1","workspace_id":"w1","tab_id":"w1:1",
                      "focused":true,"agent_status":"idle","revision":7}]
        }}),
    );
}

fn named_registry(fx: &Fixture) -> impl Drop + use<> {
    let at = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
    let endpoint = crate::services::session_host::HerdrEndpoint::new(
        NODE,
        &at.key,
        fx.rig.socket(),
        &at.herdr_session,
    )
    .unwrap();
    fx.rig.policy_registry_on_this_thread(endpoint, |host| host)
}

#[test]
fn restarted_unique_name_updates_bound_pane_once_pg() {
    run(ProviderKind::Claude, "s2-name-unique", false, |fx, rt| {
        let _named = named_registry(&fx);
        snapshot_labels(&fx, &[format!("{}#{NONCE}", fx.logical())]);
        let mut stale = fx.record.clone();
        stale.location.as_mut().unwrap().pane_id = "w0:p1".into();
        rt.block_on(fx.save(&stale));
        fx.legacy_boot(rt);
        rt.block_on(async {
            let saved = read_row(&fx.pool, fx.logical()).await;
            assert_eq!(saved.record, HostedRecord::Known(fx.record.clone()));
            assert_eq!(fx.owner(), Some(ChannelId::new(CHANNEL)));
            assert!(fx.rig.sends().is_empty(), "boot only reattaches");
        });
        fx.legacy_boot(rt);
        rt.block_on(async {
            assert_eq!(
                read_row(&fx.pool, fx.logical()).await.record,
                HostedRecord::Known(fx.record.clone())
            );
            fx.close().await;
        });
    });
}

fn assert_name_conflict_uses_exact_path(duplicate: bool) {
    run(
        ProviderKind::Claude,
        if duplicate {
            "s2-name-duplicate"
        } else {
            "s2-name-other"
        },
        false,
        |fx, rt| {
            let _named = named_registry(&fx);
            let other = format!("{}#{SUCCESSOR}", fx.logical());
            let labels = if duplicate {
                vec![format!("{}#{NONCE}", fx.logical()), other]
            } else {
                vec![other]
            };
            snapshot_labels(&fx, &labels);
            rt.block_on(async {
                let mut stored = fx.record.clone();
                stored.expected.as_mut().unwrap().provider_process.start =
                    "old-process-start".into();
                let row = fx.save(&stored).await;
                let lease =
                    SessionTransitionLease::try_acquire(&fx.shared, ChannelId::new(CHANNEL))
                        .unwrap();
                let (result, observed) = continue_owner(&fx.shared, &fx.pool, &lease, &row).await;
                assert_eq!(result, VerifiedReattach::Held);
                assert_eq!(observed, row);
                fx.assert_unpublished();
                drop(lease);
                fx.close().await;
            });
        },
    );
}

#[test]
fn restarted_duplicate_workspace_names_use_exact_pane_evidence_pg() {
    assert_name_conflict_uses_exact_path(true);
}

#[test]
fn restarted_workspace_nonce_mismatch_uses_exact_pane_evidence_pg() {
    assert_name_conflict_uses_exact_path(false);
}
