use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;

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
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, NODE, PANE, SHELL};
use crate::services::tmux_common::session_temp_path;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};
use crate::services::tui_prompt_dedupe::binding_events::{self, CauseSource, Proposal, SourceId};
use crate::services::tui_prompt_dedupe::{self as dedupe, runtime_binding_for_tmux_session};

const CHANNEL: u64 = 82801;
const NONCE: &str = "0123456789abcdef0123456789abcdef";
const SUCCESSOR: &str = "fedcba9876543210fedcba9876543210";

struct Fixture {
    rig: HerdrRig,
    shared: Arc<SharedData>,
    record: HostedExecution,
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
            rig,
            shared,
            record,
            pool,
            row,
            source,
            context_path: context.path,
            db,
        }
    }

    fn logical(&self) -> &str {
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
        match std::env::var("ADK_HERDR_REATTACH_TEST_MODE").as_deref() {
            Ok("before") => match reconnect_source(&self.pool, row, None).await {
                RestartAttachment::Legacy(HerdrSourceAttach::Published { .. }) => {
                    VerifiedReattach::Live
                }
                _ => VerifiedReattach::Held,
            },
            Ok("always-live") => {
                verify_with(&self.shared, &self.pool, &lease, row, |_| true, stale_nonce).await
            }
            Ok("never-live") => {
                verify_with(
                    &self.shared,
                    &self.pool,
                    &lease,
                    row,
                    |_| false,
                    stale_nonce,
                )
                .await
            }
            Ok("ignore-nonce") => {
                verify_with(
                    &self.shared,
                    &self.pool,
                    &lease,
                    row,
                    HostReconcile::admits_reconnect,
                    |_, marker| matches!(marker, SpawnNonceMarker::Unreadable),
                )
                .await
            }
            Err(std::env::VarError::NotPresent) => match reconnect_source(
                &self.pool,
                row,
                Some(VerifiedTarget {
                    shared: &self.shared,
                    transition: &lease,
                }),
            )
            .await
            {
                RestartAttachment::Verified(result) => result,
                RestartAttachment::Legacy(_) => panic!("verified target did not select its path"),
            },
            mode => panic!("unknown reattach test mode: {mode:?}"),
        }
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

    async fn close(self) {
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

fn run(
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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(CHANNEL, NODE)]);
    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
    let rig = HerdrRig::start();
    let registry = rig.registry_as_on_this_thread(
        NODE,
        &endpoint.key,
        &endpoint.socket_path,
        &endpoint.herdr_session,
    );
    let codex = crate::services::turn_host::force_codex_switch_for_test(Some(true));
    let fixture = runtime.block_on(Fixture::new(rig, provider, tag, busy));
    verify(fixture, &runtime);
    drop((codex, registry, hosts, log_root, state, test_root));
}

fn live_case(provider: ProviderKind, busy: bool, boot_first: bool, tag: &str) {
    run(provider, tag, busy, |fx, rt| {
        if boot_first {
            fx.legacy_boot(rt);
            assert_eq!(
                fx.owner(),
                None,
                "the landed boot path keeps the old registry behavior"
            );
        }
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Live);
        if !boot_first {
            fx.legacy_boot(rt);
        }
        assert_eq!(fx.owner(), Some(ChannelId::new(CHANNEL)));
        let binding = runtime_binding_for_tmux_session(fx.logical()).unwrap();
        assert_eq!(
            binding.session_id.as_deref(),
            Some(fx.source.session_id.as_str())
        );
        assert_eq!(binding.output_path, fx.source.path.to_str().unwrap());
        let logs_before = binding_events::binding_events_since(CHANNEL, 0).unwrap();
        assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Live);
        let after = runtime_binding_for_tmux_session(fx.logical()).unwrap();
        assert_eq!(after.last_offset, binding.last_offset);
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
        wrong.state = HostedState::Pending;
        wrong.location = None;
        wrong.expected = None;
        let row = rt.block_on(fx.save(&wrong));
        assert_eq!(rt.block_on(fx.verify(&row)), VerifiedReattach::Held);
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
fn clear_during_the_probe_and_conflicting_registry_each_refuse_publication_pg() {
    run(
        ProviderKind::Claude,
        "publication-fence",
        false,
        |fx, rt| {
            let clear = Arc::new(std::sync::Mutex::new(None));
            let during_probe = clear.clone();
            let token = fx.record.owner.discord_token_hash.clone();
            fx.rig.on_reply(move |call| {
                use crate::services::session_host::herdr_socket_rig_tests::HerdrRequest;
                if matches!(call.request, HerdrRequest::PaneProcessInfo { .. }) {
                    let mut guard = during_probe.lock().unwrap();
                    guard
                        .get_or_insert_with(|| clear_epoch::begin_clear("claude", &token, CHANNEL));
                }
            });
            assert_eq!(rt.block_on(fx.verify(&fx.row)), VerifiedReattach::Held);
            assert_eq!(fx.owner(), None);
            assert!(runtime_binding_for_tmux_session(fx.logical()).is_none());
            fx.rig.on_reply(|_| {});
            drop(clear.lock().unwrap().take());

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
