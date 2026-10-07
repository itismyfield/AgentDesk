//! Real mailbox command admission, canonical PG rows, binding logs and Herdr socket effects.
use super::super::TmuxCleanupPolicy;
use super::super::judged_stop::{CommandStop, begin_command_stop};
use super::super::stop_host::tests::{Fixture, Mark, mark, run};
use super::super::stop_host::{StopSettlement, StopTarget};
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HOSTED_EXECUTION_SCHEMA, HostedExecution, HostedLocation, HostedOwner, SourceRef,
};
use crate::services::discord::SharedData;
use crate::services::provider::cancel_token_claude_interrupt::HERDR_CANCEL_OVERRIDE;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events as events};
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use serde_json::json;
use std::sync::Mutex;

pub(super) static AFTER_IDENTITY: Mutex<Option<Box<dyn FnOnce() + Send>>> = Mutex::new(None);

struct Switch;
impl Switch {
    fn on() -> Self {
        HERDR_CANCEL_OVERRIDE.set(Some(true));
        Self
    }
}
impl Drop for Switch {
    fn drop(&mut self) {
        HERDR_CANCEL_OVERRIDE.set(None);
        *AFTER_IDENTITY.lock().unwrap() = None;
    }
}

struct Case {
    shared: Arc<SharedData>,
    token: Arc<CancelToken>,
    provider: ProviderKind,
    owner: HostedOwner,
    channel: ChannelId,
    path: PathBuf,
    rig: HerdrRig,
    record: HostedExecution,
}
impl Case {
    async fn new(pool: &sqlx::PgPool, root: &Path, provider: ProviderKind, n: u64) -> Self {
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let channel = ChannelId::new(5_340_103_000 + n);
        let logical = format!("AgentDesk-{}-p10-stop-{n}", provider.as_str());
        let nonce = format!("{n:032x}");
        let owner = HostedOwner {
            provider: provider.as_str().into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: channel.to_string(),
            logical_key: logical.clone(),
            owner_node: NODE.into(),
            runtime_root: root.display().to_string(),
        };
        let rig = HerdrRig::start();
        use dedupe::binding_context::{BindingContext, PreparedIncarnation};
        let context = PreparedIncarnation::create(BindingContext {
            schema: 1,
            provider: provider.as_str().into(),
            created_at: chrono::Utc::now(),
            execution_nonce: nonce.clone(),
            tmux_session: logical.clone(),
            channel_id: Some(channel.get()),
            owner_runtime_root: owner.runtime_root.clone(),
            host: Some("herdr".into()),
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: None,
            first_prompt_digest: None,
            source_policy: None,
        })
        .unwrap()
        .path;
        rig.run_provider(&context, false);
        rig.show_panes(&[PANE]);
        let mut expected = rig.expected(&nonce);
        expected.binding_provider = provider.as_str().into();
        let record = HostedExecution {
            schema: HOSTED_EXECUTION_SCHEMA,
            state: HostedState::Bound,
            execution_nonce: nonce.clone(),
            owner: owner.clone(),
            location: Some(HostedLocation {
                host: "herdr".into(),
                execution_node: NODE.into(),
                endpoint_config_key: KEY.into(),
                socket_addr: rig.socket().display().to_string(),
                named_session: SESSION.into(),
                pane_id: PANE.into(),
            }),
            expected: Some(expected),
            source_ref: SourceRef {
                runtime_root: owner.runtime_root.clone(),
                channel: channel.to_string(),
                provider: provider.as_str().into(),
                logical_key: logical.clone(),
                execution_nonce: nonce.clone(),
                initial_source: None,
                baseline_event_seq: None,
            },
        };
        sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, $2, 'idle', 'discord_channel', $3, $4, $5)")
            .bind(format!("{}/{}/{NODE}:{logical}", provider.as_str(), shared.token_hash)).bind(provider.as_str()).bind(&shared.token_hash).bind(channel.to_string()).bind(json!(record)).execute(pool).await.unwrap();
        mark(&logical, Mark::Herdr);
        let path = root.join(format!("{logical}.jsonl"));
        let content = if provider == ProviderKind::Claude {
            format!(
                "{}\n{}\n",
                json!({"type":"user","uuid":"turn-a","message":{"role":"user","content":"go"}}),
                json!({"type":"assistant","message":{"content":[]}})
            )
        } else {
            format!(
                "{}\n",
                json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-a"}})
            )
        };
        std::fs::write(&path, content).unwrap();
        let (dev, ino) = crate::services::tui_o::shadow::capture::file_identity(
            &std::fs::metadata(&path).unwrap(),
        );
        let event = events::BindingEvent {
            seq: 1,
            channel_id: channel.get(),
            provider: provider.as_str().into(),
            tmux_session: logical.clone(),
            execution_nonce: Some(nonce),
            old: None,
            new: events::BindingTarget::Source(events::SourceId {
                session_id: "parent".into(),
                path: path.clone(),
                dev,
                ino,
            }),
            cause: events::BindingCause::Startup,
            parent_hint: None,
            evidence: events::BindingEvidence {
                hook_event: Some("SessionStart".into()),
                received_at: chrono::Utc::now(),
            },
            committed_at: chrono::Utc::now(),
        };
        let log = root.join(events::BINDING_EVENTS_DIR);
        std::fs::create_dir_all(&log).unwrap();
        std::fs::write(
            log.join(format!("{}.log", channel.get())),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        dedupe::register_tmux_runtime_binding(
            &logical,
            dedupe::TuiRuntimeBinding {
                runtime_kind: if provider == ProviderKind::Claude {
                    crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
                } else {
                    crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
                },
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("parent".into()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let token = Arc::new(CancelToken::new());
        let observation = token.prepare_herdr_interrupt(provider.clone(), &owner);
        *observation.submission.lock().unwrap() = HerdrSubmission::Submitted;
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel,
                token.clone(),
                UserId::new(7),
                MessageId::new(channel.get() + 1)
            )
            .await
        );
        let case = Self {
            shared,
            token,
            provider,
            owner,
            channel,
            path,
            rig,
            record,
        };
        case.screen(true);
        case
    }
    fn screen(&self, busy: bool) {
        let text = match (&self.provider, busy) {
            (ProviderKind::Claude, true) => "· Actioning… (4m 7s · esc to interrupt)",
            (ProviderKind::Codex, true) => "• Working (1s • esc to interrupt)",
            _ => "Ready for input (type message + Enter)",
        };
        self.rig.answer("pane.read", json!({"type":"pane_read","read":{"pane_id":PANE,"workspace_id":"w1","tab_id":"w1:1","source":"recent_unwrapped","format":"text","text":text,"revision":3,"truncated":false}}));
    }
    async fn stop(&self, reason: &str) -> super::super::stop_host::StopOutcome {
        let CommandStop::Stop(stop) =
            begin_command_stop(&self.shared, &self.provider, self.channel, true).await
        else {
            panic!("command did not admit token");
        };
        let result = stop.stop(TmuxCleanupPolicy::PreserveSession, reason).await;
        assert!(!result.may_clear_inflight());
        result
    }
    fn escapes(&self) -> usize {
        let writes = self.rig.sends();
        for write in &writes {
            assert_eq!(write, &json!({"pane_id":PANE,"keys":["esc"]}));
        }
        writes.len()
    }
}
impl Drop for Case {
    fn drop(&mut self) {
        dedupe::clear_tmux_runtime_binding(&self.owner.logical_key);
        crate::services::cluster::channel_home::unregister(&self.channel.to_string());
    }
}

#[test]
fn herdr_command_stop_preserves_host_once_for_both_providers() {
    let fx = Fixture::new();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _switch = Switch::on();
    let root = tempfile::tempdir().unwrap();
    let _log = TestBindingRoot::enter(Some(root.path()));
    run(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        for (n, provider) in [ProviderKind::Claude, ProviderKind::Codex]
            .into_iter()
            .enumerate()
        {
            let case = Case::new(&pool, root.path(), provider, n as u64 + 1).await;
            let _registry = case.rig.registry_on_this_thread();
            assert_eq!(
                case.stop("/stop").await.settlement,
                StopSettlement::HostOwned
            );
            assert_eq!(case.escapes(), 1, "{}", case.provider.as_str());
            assert!(matches!(
                begin_command_stop(&case.shared, &case.provider, case.channel, true).await,
                CommandStop::AlreadyStopping
            ));
            let _ = interrupt_herdr(&pool, &case.token, &case.provider).await;
            assert_eq!(case.escapes(), 1);
            assert!(!StopTarget::for_token(&case.token).reaches_legacy_host());
            assert!(fx.take_calls().is_empty());
        }
        pool.close().await;
        db.drop().await;
    });
}

#[test]
fn herdr_command_stop_refuses_off_idle_unsubmitted_and_nonuser_effects() {
    let fx = Fixture::new();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _switch = Switch::on();
    let root = tempfile::tempdir().unwrap();
    let _log = TestBindingRoot::enter(Some(root.path()));
    run(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        for (n, provider) in [ProviderKind::Claude, ProviderKind::Codex]
            .into_iter()
            .enumerate()
        {
            for variant in 0..5 {
                let case = Case::new(
                    &pool,
                    root.path(),
                    provider.clone(),
                    10 + n as u64 * 10 + variant,
                )
                .await;
                let _registry = case.rig.registry_on_this_thread();
                match variant {
                    0 => {
                        HERDR_CANCEL_OVERRIDE.set(Some(false));
                        assert!(matches!(
                            begin_command_stop(&case.shared, &case.provider, case.channel, true)
                                .await,
                            CommandStop::HostRefused
                        ));
                        assert!(case.rig.requests().is_empty());
                        HERDR_CANCEL_OVERRIDE.set(Some(true));
                    }
                    1 => {
                        case.screen(false);
                        let _ = case.stop("!stop").await;
                    }
                    2 => {
                        *case
                            .token
                            .herdr_interrupt_state()
                            .unwrap()
                            .submission
                            .lock()
                            .unwrap() = HerdrSubmission::Unsubmitted;
                        assert_eq!(
                            case.stop("/skill stop").await.settlement,
                            StopSettlement::LocalUnsubmitted
                        );
                    }
                    3 => {
                        let _ = case.stop("restart").await;
                    }
                    _ => {
                        let _home = crate::services::cluster::channel_home::register_for_test(
                            case.channel.get(),
                            None,
                        );
                        assert!(matches!(
                            begin_command_stop(&case.shared, &case.provider, case.channel, true)
                                .await,
                            CommandStop::HostRefused
                        ));
                    }
                }
                assert_eq!(
                    case.escapes(),
                    0,
                    "provider {provider:?}, variant {variant}"
                );
                assert!(fx.take_calls().is_empty());
            }
        }
        pool.close().await;
        db.drop().await;
    });
}

fn stale_fence(provider: ProviderKind, identity: bool) {
    let fx = Fixture::new();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _switch = Switch::on();
    let root = tempfile::tempdir().unwrap();
    let _log = TestBindingRoot::enter(Some(root.path()));
    run(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let case = Case::new(&pool, root.path(), provider.clone(), 90).await;
        let _registry = case.rig.registry_on_this_thread();
        let (path, owner) = (case.path.clone(), case.owner.clone());
        *AFTER_IDENTITY.lock().unwrap() = Some(Box::new(move || {
            if identity {
                use std::io::Write;
                let record = if provider == ProviderKind::Claude {
                    json!({"type":"user","uuid":"turn-b","message":{"content":"next"}})
                } else {
                    json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-b"}})
                };
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                writeln!(file, "{record}").unwrap();
                if provider == ProviderKind::Claude {
                    writeln!(
                        file,
                        "{}",
                        json!({"type":"assistant","message":{"content":[]}})
                    )
                    .unwrap();
                }
            } else {
                CancelToken::new().prepare_herdr_interrupt(provider, &owner);
            }
        }));
        let _ = case.stop("/stop").await;
        assert_eq!(case.escapes(), 0, "stale fence must prevent effects");
        assert!(fx.take_calls().is_empty());
        pool.close().await;
        db.drop().await;
    });
}
#[test]
fn herdr_http_preserve_keeps_conflict_and_cannot_cancel_token() {
    let fx = Fixture::new();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _switch = Switch::on();
    let root = tempfile::tempdir().unwrap();
    let _log = TestBindingRoot::enter(Some(root.path()));
    run(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        for (n, provider) in [ProviderKind::Claude, ProviderKind::Codex]
            .into_iter()
            .enumerate()
        {
            let case = Case::new(&pool, root.path(), provider, 200 + n as u64).await;
            let _registry = case.rig.registry_on_this_thread();
            let registry = crate::services::discord::health::HealthRegistry::new();
            registry
                .register(case.provider.as_str().into(), case.shared.clone())
                .await;
            let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
                provider: Some(case.provider.clone()),
                channel_id: Some(case.channel),
                tmux_name: case.owner.logical_key.clone(),
            };
            let result = crate::services::turn_lifecycle::stop_turn_preserving_queue(
                Some(&registry),
                &target,
                "http_cancel",
            )
            .await;
            assert!(result.host_guard_kept());
            assert!(!case.token.cancelled.load(Ordering::SeqCst));
            assert!(
                !case
                    .token
                    .herdr_interrupt_state()
                    .unwrap()
                    .user_stop
                    .load(Ordering::SeqCst)
            );
            assert!(case.rig.requests().is_empty());
            assert!(fx.take_calls().is_empty());
        }
        pool.close().await;
        db.drop().await;
    });
}

#[test]
fn herdr_uncertain_escape_spends_claim_and_gate_mismatch_writes_nothing() {
    let fx = Fixture::new();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _switch = Switch::on();
    let root = tempfile::tempdir().unwrap();
    let _log = TestBindingRoot::enter(Some(root.path()));
    run(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        for (n, provider) in [ProviderKind::Claude, ProviderKind::Codex]
            .into_iter()
            .enumerate()
        {
            for uncertain in [false, true] {
                let case = Case::new(
                    &pool,
                    root.path(),
                    provider.clone(),
                    300 + n as u64 * 10 + u64::from(uncertain),
                )
                .await;
                let _registry = case.rig.registry_on_this_thread();
                if uncertain {
                    case.rig.leave_sends_unanswered(true);
                } else {
                    case.rig.foreground(&[]);
                }
                assert_eq!(
                    case.stop("!cc stop").await.settlement,
                    StopSettlement::HostOwned
                );
                assert_eq!(case.escapes(), usize::from(uncertain));
                let _ = interrupt_herdr(&pool, &case.token, &case.provider).await;
                assert_eq!(case.escapes(), usize::from(uncertain));
                assert!(fx.take_calls().is_empty());
            }
        }
        pool.close().await;
        db.drop().await;
    });
}

#[test]
fn herdr_draining_holder_still_cancels_and_generation_adds_no_cleanup_authority() {
    let fx = Fixture::new();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _switch = Switch::on();
    let root = tempfile::tempdir().unwrap();
    let _log = TestBindingRoot::enter(Some(root.path()));
    run(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        for (n, provider) in [ProviderKind::Claude, ProviderKind::Codex]
            .into_iter()
            .enumerate()
        {
            let case = Case::new(&pool, root.path(), provider, 400 + n as u64).await;
            let _registry = case.rig.registry_on_this_thread();
            let home = crate::services::cluster::channel_home::register_for_test(
                case.channel.get(),
                Some(crate::db::o_channel_homes::HomeState::Reclaiming),
            );
            assert_eq!(
                home.refusal(),
                Some(crate::services::cluster::channel_home::HomeRefusal::Draining)
            );
            assert_eq!(
                case.stop("/cc stop").await.settlement,
                StopSettlement::HostOwned
            );
            assert_eq!(case.escapes(), 1);
            case.token.cancel_with_tmux_cleanup();
            assert!(fx.take_calls().is_empty());
            assert_eq!(case.escapes(), 1);
        }
        pool.close().await;
        db.drop().await;
    });
}

#[test]
fn herdr_claude_generation_only_stale_sends_no_escape() {
    stale_fence(ProviderKind::Claude, false);
}
#[test]
fn herdr_claude_identity_only_changed_sends_no_escape() {
    stale_fence(ProviderKind::Claude, true);
}
#[test]
fn herdr_codex_generation_only_stale_sends_no_escape() {
    stale_fence(ProviderKind::Codex, false);
}
#[test]
fn herdr_codex_identity_only_changed_sends_no_escape() {
    stale_fence(ProviderKind::Codex, true);
}
