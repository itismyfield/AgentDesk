//! Real mailbox command admission, canonical PG rows, binding logs and Herdr socket effects.
use super::super::judged_stop::{CommandStop, begin_command_stop};
use super::super::stop_host::StopTarget;
use super::super::stop_host::tests::{Fixture, Mark, mark, run};
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
static AFTER_SEND: Mutex<Option<Box<dyn FnOnce() + Send>>> = Mutex::new(None);

/// Takes the post-send action so the lock is released before it runs.
pub(super) fn take_after_send() -> Option<Box<dyn FnOnce() + Send>> {
    AFTER_SEND.lock().unwrap().take()
}

type Step = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
static BEFORE_LATE_STOP: Mutex<Option<Step>> = Mutex::new(None);

/// Runs the step a test placed between a stop's first Pending and its late arming.
pub(super) async fn before_late_stop() {
    let step = BEFORE_LATE_STOP.lock().unwrap().take();
    if let Some(step) = step {
        step.await;
    }
}

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
        *AFTER_SEND.lock().unwrap() = None;
        *BEFORE_LATE_STOP.lock().unwrap() = None;
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
        observation.submission.lock().unwrap().submission = HerdrSubmission::Submitted;
        if provider == ProviderKind::Codex {
            assert!(observation.record_turn_start(HerdrTurnStart {
                execution_nonce: record.execution_nonce.clone(),
                source: path.clone(),
                file: Some((dev, ino)),
                offset: 0,
                submitted_at: None,
            }));
        }
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
    async fn delivery(&self, reason: &str) -> HerdrDelivery {
        match admit_herdr_command(
            &self.token,
            Some(&self.token),
            &self.provider,
            self.channel.get(),
            &self.shared.token_hash,
            reason,
        ) {
            Ok(()) => {
                interrupt_herdr(
                    self.shared.pg_pool.as_ref().unwrap(),
                    &self.token,
                    &self.provider,
                )
                .await
            }
            Err(reason) => HerdrDelivery::NotSent(reason),
        }
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
fn herdr_delivery_sends_once_for_both_providers() {
    with_cases(|case, fx, runtime| {
        assert_eq!(
            runtime.block_on(case.delivery("/stop")),
            HerdrDelivery::Sent
        );
        let next = runtime.block_on(interrupt_herdr(
            case.shared.pg_pool.as_ref().unwrap(),
            &case.token,
            &case.provider,
        ));
        assert_eq!(
            case.escapes(),
            1,
            "consumed claim must prevent another Escape"
        );
        assert_eq!(next, HerdrDelivery::NotSent(HerdrNotSent::Duplicate));
        assert_eq!(case.escapes(), 1);
        assert!(!StopTarget::for_token(&case.token).reaches_legacy_host());
        assert!(!case.token.cancelled.load(Ordering::SeqCst));
        assert!(fx.take_calls().is_empty());
    });
}

fn with_cases(mut check: impl FnMut(&Case, &Fixture, &tokio::runtime::Runtime)) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fx = Fixture::new();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _switch = Switch::on();
    let root = tempfile::tempdir().unwrap();
    let _log = TestBindingRoot::enter(Some(root.path()));
    let db = runtime.block_on(crate::db::auto_queue::test_support::TestPostgresDb::create());
    let pool = runtime.block_on(db.connect_and_migrate());
    for (n, provider) in [ProviderKind::Claude, ProviderKind::Codex]
        .into_iter()
        .enumerate()
    {
        let case = runtime.block_on(Case::new(&pool, root.path(), provider, 10 + n as u64));
        let _registry = case.rig.registry_on_this_thread();
        check(&case, &fx, &runtime);
    }
    runtime.block_on(pool.close());
    runtime.block_on(db.drop());
}

#[test]
fn herdr_production_stop_unwired_switch_on_matches_off() {
    with_cases(|case, fx, runtime| {
        let mut observations = Vec::new();
        for enabled in [false, true] {
            HERDR_CANCEL_OVERRIDE.set(Some(enabled));
            let reply = match runtime.block_on(begin_command_stop(
                &case.shared,
                &case.provider,
                case.channel,
                true,
            )) {
                CommandStop::HostRefused => "host_refused",
                CommandStop::AlreadyStopping => "already_stopping",
                CommandStop::NoActiveTurn => "no_active_turn",
                _ => "admitted",
            };
            observations.push((
                reply,
                case.token.cancelled.load(Ordering::SeqCst),
                fx.take_calls(),
            ));
            assert_eq!(
                case.escapes(),
                0,
                "production /stop must not deliver Escape"
            );
            assert!(case.rig.requests().is_empty(), "stop must not probe Herdr");
        }
        assert_eq!(observations[0], observations[1]);
        assert_eq!(observations[1], ("host_refused", false, vec![]));
    });
}

#[test]
fn herdr_typed_refusals_have_no_escape_effects() {
    with_cases(|case, fx, runtime| {
        let pool = case.shared.pg_pool.as_ref().unwrap();
        use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;
        HERDR_SETTLEMENT_OVERRIDE.set(false);
        let unavailable = runtime.block_on(interrupt_herdr(pool, &case.token, &case.provider));
        HERDR_SETTLEMENT_OVERRIDE.set(true);
        assert_eq!(
            unavailable,
            HerdrDelivery::NotSent(HerdrNotSent::SettlementUnavailable)
        );
        assert_eq!(
            runtime.block_on(interrupt_herdr(pool, &case.token, &case.provider)),
            HerdrDelivery::NotSent(HerdrNotSent::NotAdmitted)
        );
        let other = Arc::new(CancelToken::new());
        assert_eq!(
            admit_herdr_command(
                &case.token,
                Some(&other),
                &case.provider,
                case.channel.get(),
                &case.shared.token_hash,
                "/stop"
            ),
            Err(HerdrNotSent::Generation)
        );
        assert_eq!(
            runtime.block_on(interrupt_herdr(pool, &case.token, &case.provider)),
            HerdrDelivery::NotSent(HerdrNotSent::NotAdmitted),
            "stale admission must not write intent"
        );
        assert_eq!(
            runtime.block_on(case.delivery("watchdog")),
            HerdrDelivery::NotSent(HerdrNotSent::NotAdmitted)
        );
        HERDR_CANCEL_OVERRIDE.set(Some(false));
        assert_eq!(
            runtime.block_on(case.delivery("/stop")),
            HerdrDelivery::NotSent(HerdrNotSent::SwitchOff)
        );
        HERDR_CANCEL_OVERRIDE.set(Some(true));
        let home =
            crate::services::cluster::channel_home::register_for_test(case.channel.get(), None);
        assert_eq!(
            runtime.block_on(case.delivery("/stop")),
            HerdrDelivery::NotSent(HerdrNotSent::Holder)
        );
        drop(home);
        crate::services::cluster::channel_home::unregister(&case.channel.to_string());
        case.screen(false);
        assert_eq!(
            runtime.block_on(case.delivery("/stop")),
            HerdrDelivery::NotSent(HerdrNotSent::Idle)
        );
        let state = case.token.herdr_interrupt_state().unwrap();
        state.submission.lock().unwrap().submission = HerdrSubmission::Unsubmitted;
        assert_eq!(
            runtime.block_on(interrupt_herdr(pool, &case.token, &case.provider)),
            HerdrDelivery::NotSent(HerdrNotSent::Pending)
        );
        state.submission.lock().unwrap().submission = HerdrSubmission::Submitted;
        dedupe::clear_tmux_runtime_binding(&case.owner.logical_key);
        assert_eq!(
            runtime.block_on(interrupt_herdr(pool, &case.token, &case.provider)),
            HerdrDelivery::NotSent(HerdrNotSent::Pending)
        );
        assert_eq!(case.escapes(), 0);
        assert!(fx.take_calls().is_empty());
    });
}

#[test]
fn herdr_prepare_preserves_consumed_claim_and_intent() {
    with_cases(|case, fx, runtime| {
        assert_eq!(
            runtime.block_on(case.delivery("/stop")),
            HerdrDelivery::Sent
        );
        let before = case.token.herdr_interrupt_state().unwrap();
        let after = case
            .token
            .prepare_herdr_interrupt(case.provider.clone(), &case.owner);
        assert!(Arc::ptr_eq(&before, &after));
        let next = runtime.block_on(interrupt_herdr(
            case.shared.pg_pool.as_ref().unwrap(),
            &case.token,
            &case.provider,
        ));
        assert_eq!(
            case.escapes(),
            1,
            "consumed claim must prevent another Escape"
        );
        assert_eq!(next, HerdrDelivery::NotSent(HerdrNotSent::Duplicate));
        assert_eq!(
            case.escapes(),
            1,
            "second prepare must not authorize a second write"
        );
        assert!(fx.take_calls().is_empty());
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
        let result = case.delivery("/stop").await;
        assert_eq!(case.escapes(), 0, "stale fence must prevent effects");
        assert_eq!(
            result,
            HerdrDelivery::NotSent(if identity {
                HerdrNotSent::Identity
            } else {
                HerdrNotSent::Generation
            })
        );
        assert!(fx.take_calls().is_empty());
        pool.close().await;
        db.drop().await;
    });
}
#[test]
fn herdr_http_preserve_keeps_conflict_and_cannot_cancel_token_pg() {
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
                let result = case.delivery("!cc stop").await;
                assert_eq!(
                    result,
                    if uncertain {
                        HerdrDelivery::Indeterminate
                    } else {
                        HerdrDelivery::NotSent(HerdrNotSent::Gate)
                    }
                );
                assert_eq!(case.escapes(), usize::from(uncertain));
                let next = interrupt_herdr(&pool, &case.token, &case.provider).await;
                assert_eq!(
                    case.escapes(),
                    usize::from(uncertain),
                    "Indeterminate consumes the write claim"
                );
                assert_eq!(
                    next,
                    HerdrDelivery::NotSent(if uncertain {
                        HerdrNotSent::Duplicate
                    } else {
                        HerdrNotSent::Gate
                    })
                );
                assert_eq!(case.escapes(), usize::from(uncertain));
                assert!(fx.take_calls().is_empty());
            }
        }
        pool.close().await;
        db.drop().await;
    });
}

#[test]
fn herdr_draining_holder_still_cancels_and_generation_adds_no_cleanup_authority_pg() {
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
            assert_eq!(case.delivery("/cc stop").await, HerdrDelivery::Sent);
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

fn user_stop(case: &Case, runtime: &tokio::runtime::Runtime, reason: &str) -> HerdrStop {
    use super::super::judged_stop::begin_user_stop;
    let stop = begin_user_stop(&case.shared, &case.provider, case.channel, true, reason);
    match runtime.block_on(stop) {
        CommandStop::Herdr(stop) => stop,
        CommandStop::HostRefused => panic!("host refused"),
        _ => panic!("a Herdr turn took another stop path"),
    }
}

fn current_token(case: &Case, runtime: &tokio::runtime::Runtime) -> Option<Arc<CancelToken>> {
    let snapshot = crate::services::discord::mailbox_snapshot(&case.shared, case.channel);
    runtime.block_on(snapshot).cancel_token
}

#[test]
fn herdr_user_stop_sends_one_escape_and_keeps_the_turn() {
    with_cases(|case, fx, runtime| {
        let intent = case.token.herdr_interrupt_state().unwrap();
        assert_eq!(
            user_stop(case, runtime, "!stop"),
            HerdrStop::Requested(HerdrDelivery::Sent)
        );
        assert_eq!(case.escapes(), 1);
        assert!(intent.user_stop.load(Ordering::SeqCst));
        assert_eq!(
            user_stop(case, runtime, "/stop"),
            HerdrStop::AlreadyRequested
        );
        assert_eq!(case.escapes(), 1, "a repeated stop sends nothing");
        assert!(!case.token.cancelled.load(Ordering::SeqCst));
        assert!(case.token.cancel_source().is_none());
        let current = current_token(case, runtime).expect("the turn keeps its slot");
        assert!(Arc::ptr_eq(&current, &case.token));
        assert!(fx.take_calls().is_empty(), "no tmux or cleanup call");
    });
}

#[test]
fn herdr_user_stop_with_the_switch_or_settlement_off_records_and_sends_nothing() {
    use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;
    with_cases(|case, fx, runtime| {
        let intent = case.token.herdr_interrupt_state().unwrap();
        HERDR_CANCEL_OVERRIDE.set(Some(false));
        assert_eq!(
            user_stop(case, runtime, "!stop"),
            HerdrStop::Refused(HerdrNotSent::SwitchOff)
        );
        HERDR_CANCEL_OVERRIDE.set(Some(true));
        HERDR_SETTLEMENT_OVERRIDE.set(false);
        let user = super::super::judged_stop::begin_user_stop(
            &case.shared,
            &case.provider,
            case.channel,
            true,
            "!stop",
        );
        let unsettled = matches!(runtime.block_on(user), CommandStop::HostRefused);
        let command = begin_command_stop(&case.shared, &case.provider, case.channel, true);
        let legacy = matches!(runtime.block_on(command), CommandStop::HostRefused);
        HERDR_SETTLEMENT_OVERRIDE.set(true);
        assert!(
            unsettled && legacy,
            "without settlement a user stop is the command stop"
        );
        assert_eq!(case.escapes(), 0);
        assert!(!intent.user_stop.load(Ordering::SeqCst));
        assert!(!case.token.cancelled.load(Ordering::SeqCst));
        assert!(fx.take_calls().is_empty());
    });
}

#[test]
fn herdr_user_stop_reads_a_long_turn_and_never_calls_an_unread_turn_idle() {
    with_cases(|case, _fx, runtime| {
        if case.provider == ProviderKind::Codex {
            let filler = json!({"type":"event_msg","payload":{"type":"agent_reasoning","text":"x".repeat(1024)}});
            let mut rollout = std::fs::OpenOptions::new()
                .append(true)
                .open(&case.path)
                .unwrap();
            for _ in 0..300 {
                use std::io::Write;
                writeln!(rollout, "{filler}").unwrap();
            }
            assert!(std::fs::metadata(&case.path).unwrap().len() > 256 * 1024);
            assert_eq!(
                user_stop(case, runtime, "!stop"),
                HerdrStop::Requested(HerdrDelivery::Sent),
                "a turn started before the last 256 KiB is still the running turn"
            );
            assert_eq!(case.escapes(), 1);
        } else {
            let reply = json!({"type":"assistant","message":{"content":[]}});
            std::fs::write(&case.path, format!("{reply}\n")).unwrap();
            assert_eq!(
                user_stop(case, runtime, "!stop"),
                HerdrStop::Requested(HerdrDelivery::NotSent(HerdrNotSent::Unobserved))
            );
            assert_eq!(case.escapes(), 0);
        }
    });
}

#[test]
fn herdr_codex_rollout_ending_mid_record_is_unobserved_not_idle() {
    with_cases(|case, _fx, runtime| {
        if case.provider != ProviderKind::Codex {
            return;
        }
        let mut rollout = std::fs::OpenOptions::new()
            .append(true)
            .open(&case.path)
            .unwrap();
        std::io::Write::write_all(&mut rollout, b"{\"type\":\"event_msg\"").unwrap();
        assert_eq!(
            user_stop(case, runtime, "!stop"),
            HerdrStop::Requested(HerdrDelivery::NotSent(HerdrNotSent::Unobserved))
        );
        assert_eq!(case.escapes(), 0);
    });
}

#[test]
fn herdr_writer_unwinding_after_its_escape_keeps_the_claim_spent() {
    with_cases(|case, _fx, runtime| {
        *AFTER_SEND.lock().unwrap() = Some(Box::new(|| panic!("writer unwinds after its Escape")));
        assert_eq!(
            user_stop(case, runtime, "!stop"),
            HerdrStop::Requested(HerdrDelivery::Indeterminate)
        );
        assert_eq!(case.escapes(), 1);
        let pool = case.shared.pg_pool.as_ref().unwrap();
        assert_eq!(
            runtime.block_on(interrupt_herdr(pool, &case.token, &case.provider)),
            HerdrDelivery::NotSent(HerdrNotSent::Duplicate),
            "an uncertain Escape is never sent again"
        );
        assert_eq!(case.escapes(), 1);
    });
}

/// A Codex stop whose own turn ended while the pane began another native turn sends nothing and
/// keeps its intent; it never takes the later turn for its own.
#[test]
fn a_codex_stop_never_takes_a_later_native_turn_for_its_own() {
    with_cases(|case, _fx, runtime| {
        if case.provider != ProviderKind::Codex {
            return;
        }
        let next = [
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-a"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-b"}}),
        ];
        let lines: String = next.iter().map(|line| format!("{line}\n")).collect();
        let mut rollout = std::fs::OpenOptions::new()
            .append(true)
            .open(&case.path)
            .unwrap();
        std::io::Write::write_all(&mut rollout, lines.as_bytes()).unwrap();
        assert_eq!(
            user_stop(case, runtime, "!stop"),
            HerdrStop::Requested(HerdrDelivery::NotSent(HerdrNotSent::Identity))
        );
        assert_eq!(case.escapes(), 0);
        let intent = case.token.herdr_interrupt_state().unwrap();
        assert!(intent.user_stop.load(Ordering::SeqCst), "the intent stays");
    });
}

/// A stop that found its turn unsubmitted after the reader had read that turn's start runs again
/// only while its token is still the channel's turn.
#[test]
fn a_stop_after_its_turns_start_was_read_runs_only_for_the_current_token() {
    with_cases(|case, _fx, runtime| {
        if case.provider != ProviderKind::Codex {
            return;
        }
        let state = case.token.herdr_interrupt_state().unwrap();
        state.submission.lock().unwrap().submission = HerdrSubmission::Unsubmitted;
        state.own_start_observed(0, "turn-a");
        let (shared, channel, provider) =
            (case.shared.clone(), case.channel, case.provider.clone());
        // The turn is submitted and the channel takes another token before the stop runs again.
        *BEFORE_LATE_STOP.lock().unwrap() = Some(Box::pin(async move {
            state.submission.lock().unwrap().submission = HerdrSubmission::Submitted;
            crate::services::discord::mailbox_finish_turn(&shared, &provider, channel).await;
            let (next, user) = (Arc::new(CancelToken::new()), UserId::new(7));
            let message = MessageId::new(channel.get() + 2);
            let start = crate::services::discord::mailbox_try_start_turn;
            assert!(start(&shared, channel, next, user, message).await);
        }));
        assert_eq!(
            user_stop(case, runtime, "!stop"),
            HerdrStop::Requested(HerdrDelivery::NotSent(HerdrNotSent::Generation))
        );
        assert_eq!(case.escapes(), 0);
    });
}

/// Once the reader saw its turn's own start, a rollout rewritten in place to begin with another
/// native turn is never taken for it: nothing is sent and the intent stays.
#[test]
fn a_rollout_rewritten_in_place_never_retargets_a_late_stop() {
    with_cases(|case, _fx, runtime| {
        if case.provider != ProviderKind::Codex {
            return;
        }
        let state = case.token.herdr_interrupt_state().unwrap();
        state.submission.lock().unwrap().submission = HerdrSubmission::Unsubmitted;
        state.own_start_observed(0, "turn-a");
        let (path, observation) = (case.path.clone(), state.clone());
        *BEFORE_LATE_STOP.lock().unwrap() = Some(Box::pin(async move {
            let next =
                json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-b"}});
            std::fs::write(&path, format!("{next}\n")).unwrap();
            observation.submission.lock().unwrap().submission = HerdrSubmission::Submitted;
        }));
        let inode = |path: &Path| {
            crate::services::tui_o::shadow::capture::file_identity(
                &std::fs::metadata(path).unwrap(),
            )
        };
        let before = inode(&case.path);
        assert_eq!(
            user_stop(case, runtime, "!stop"),
            HerdrStop::Requested(HerdrDelivery::NotSent(HerdrNotSent::Identity))
        );
        assert_eq!(inode(&case.path), before, "the rollout keeps its inode");
        assert_eq!(case.escapes(), 0);
        assert!(state.user_stop.load(Ordering::SeqCst), "the intent stays");
    });
}

/// Captures the command's structured events, excluding unrelated actor diagnostics.
fn capture_stop_events() -> (Arc<Mutex<Vec<String>>>, tracing::subscriber::DefaultGuard) {
    use tracing_subscriber::layer::SubscriberExt;
    struct Events(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Events {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Fields(Vec<String>);
            impl tracing::field::Visit for Fields {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push(format!("{}={value:?}", field.name()));
                }
            }
            if event.metadata().target().contains("judged_stop")
                || event.metadata().target().contains("codex_stop_delivery")
                || event.metadata().target().ends_with("commands::stop")
            {
                let mut fields = Fields(Vec::new());
                event.record(&mut fields);
                fields.0.sort();
                self.0.lock().unwrap().push(format!(
                    "{} {:?}",
                    event.metadata().target(),
                    fields.0
                ));
            }
        }
    }
    let events = Arc::new(Mutex::new(Vec::new()));
    crate::logging::test_capture::pin_callsite_interest();
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(Events(events.clone())),
    );
    (events, guard)
}

/// Slot and queue identities remain the same, not merely the response or Escape count.
fn assert_same_stop_mailbox(
    before: &crate::services::turn_orchestrator::ChannelMailboxSnapshot,
    after: &crate::services::turn_orchestrator::ChannelMailboxSnapshot,
) {
    assert!(Arc::ptr_eq(
        before.cancel_token.as_ref().unwrap(),
        after.cancel_token.as_ref().unwrap()
    ));
    assert_eq!(after.active_request_owner, before.active_request_owner);
    assert_eq!(after.active_user_message_id, before.active_user_message_id);
    assert_eq!(after.active_turn_nonce, before.active_turn_nonce);
    assert_eq!(after.active_turn_kind, before.active_turn_kind);
    assert_eq!(
        format!("{:?}", after.intervention_queue),
        format!("{:?}", before.intervention_queue)
    );
    assert_eq!(after.pending_user_dispatch, before.pending_user_dispatch);
    assert_eq!(
        after.pending_user_dispatch_source_ids,
        before.pending_user_dispatch_source_ids
    );
    assert_eq!(
        after.active_absorbed_source_ids,
        before.active_absorbed_source_ids
    );
    assert_eq!(after.turn_started_at, before.turn_started_at);
}

#[test]
fn act7_slash_stop_effect_keeps_host_owned_and_dormant_matches_base() {
    use crate::services::discord::commands::stop::run_slash_stop;
    use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;
    with_cases(|case, fx, runtime| {
        HERDR_SETTLEMENT_OVERRIDE.set(false);
        assert!(!herdr_stop_settlement_available());
        let row = crate::services::discord::inflight::InflightTurnState::new(
            case.provider.clone(),
            case.channel.get(),
            None,
            1,
            case.channel.get() + 1,
            case.channel.get() + 2,
            "dormant equality".into(),
            None,
            Some(case.owner.logical_key.clone()),
            None,
            None,
            0,
        );
        crate::services::discord::inflight::save_inflight_state(&row).unwrap();
        let row_path = crate::services::discord::inflight::inflight_state_path(
            &crate::services::discord::inflight::inflight_runtime_root().unwrap(),
            &case.provider,
            case.channel.get(),
        );
        let bytes = std::fs::read(&row_path).unwrap();
        let before = runtime.block_on(crate::services::discord::mailbox_snapshot(
            &case.shared,
            case.channel,
        ));
        let (logs, _capture) = capture_stop_events();
        let base = runtime.block_on(begin_command_stop(
            &case.shared,
            &case.provider,
            case.channel,
            false,
        ));
        assert!(matches!(base, CommandStop::HostRefused));
        let base_logs = std::mem::take(&mut *logs.lock().unwrap());
        assert_same_stop_mailbox(
            &before,
            &runtime.block_on(crate::services::discord::mailbox_snapshot(
                &case.shared,
                case.channel,
            )),
        );
        assert_eq!(std::fs::read(&row_path).unwrap(), bytes);
        assert!(!case.token.cancelled.load(Ordering::SeqCst));
        assert!(
            crate::services::discord::tmux::recent_turn_stop_for_channel(case.channel).is_none()
        );
        assert!(fx.take_calls().is_empty());
        let dormant = runtime.block_on(run_slash_stop(&case.shared, &case.provider, case.channel));
        assert_eq!(
            dormant.text(),
            crate::services::discord::commands::HOST_REFUSED_STOP_RESPONSE
        );
        runtime.block_on(dormant.finish(&case.shared, &case.provider, case.channel));
        assert_eq!(case.escapes(), 0);
        assert_eq!(
            *logs.lock().unwrap(),
            base_logs,
            "off and base log events must agree"
        );
        assert_same_stop_mailbox(
            &before,
            &runtime.block_on(crate::services::discord::mailbox_snapshot(
                &case.shared,
                case.channel,
            )),
        );
        assert_eq!(std::fs::read(&row_path).unwrap(), bytes);
        assert!(!case.token.cancelled.load(Ordering::SeqCst));
        assert!(
            !case
                .token
                .herdr_interrupt_state()
                .unwrap()
                .user_stop
                .load(Ordering::Acquire)
        );
        assert!(
            crate::services::discord::tmux::recent_turn_stop_for_channel(case.channel).is_none()
        );
        assert!(fx.take_calls().is_empty());
        assert_eq!(
            case.token
                .herdr_interrupt_state()
                .unwrap()
                .owner
                .logical_key,
            case.record.owner.logical_key
        );
        HERDR_SETTLEMENT_OVERRIDE.set(true);
        let reply = runtime.block_on(run_slash_stop(&case.shared, &case.provider, case.channel));
        assert_eq!(
            reply.text(),
            HerdrStop::Requested(HerdrDelivery::Sent).reply()
        );
        runtime.block_on(reply.finish(&case.shared, &case.provider, case.channel));
        assert_eq!(case.escapes(), 1);
        assert!(Arc::ptr_eq(
            &current_token(case, runtime).unwrap(),
            &case.token
        ));
        assert!(!case.token.cancelled.load(Ordering::SeqCst));
        assert!(
            crate::services::discord::tmux::recent_turn_stop_for_channel(case.channel).is_none()
        );
        assert!(fx.take_calls().is_empty());
    });
    let source = include_str!("../../commands/control.rs");
    let body = source
        .split("async fn cmd_stop")
        .nth(1)
        .unwrap()
        .split("pub(super) fn parse_queued_message_id")
        .next()
        .unwrap();
    assert_eq!(body.matches("run_slash_stop(").count(), 1);
    assert!(!body.contains("begin_command_stop(") && !body.contains("begin_user_stop("));
}

/// Formerly a known defect: a cold stop survived an unsubmitted failure with no terminal. The
/// stop alone is still no proof and holds; the executor's no-attempt exit makes the proof that
/// the bridge exit seals as a policy close, with no Escape and no provider call.
#[test]
fn cold_unsubmitted_stop_closes_only_after_a_no_attempt_exit() {
    use crate::services::provider::herdr_before_start::{
        ExitDecision, finish_execution, seal_exit,
    };
    with_cases(|case, fx, runtime| {
        let state = case.token.herdr_interrupt_state().unwrap();
        state.submission.lock().unwrap().submission = HerdrSubmission::Unsubmitted;
        mark(&case.owner.logical_key, Mark::Absent);
        let reply = runtime.block_on(crate::services::discord::commands::stop::run_slash_stop(
            &case.shared,
            &case.provider,
            case.channel,
        ));
        assert_eq!(
            reply.text(),
            HerdrStop::Requested(HerdrDelivery::NotSent(HerdrNotSent::Pending)).reply()
        );
        runtime.block_on(reply.finish(&case.shared, &case.provider, case.channel));
        assert!(state.user_stop.load(Ordering::Acquire));
        // No-proof negative control: the stop alone closes nothing and keeps the token.
        assert_eq!(state.closed_probe(), Some(false));
        assert!(Arc::ptr_eq(
            &current_token(case, runtime).unwrap(),
            &case.token
        ));
        finish_execution(Some(&case.token));
        assert_eq!(state.closed_probe(), Some(true));
        let ExitDecision::PolicyClose(proof) = seal_exit(&case.token, false, false) else {
            panic!("a no-attempt exit with a cold stop is a before-start proof");
        };
        assert_eq!(proof.owner, state.owner);
        assert_eq!(proof.turn_nonce.as_str(), case.token.turn_nonce().unwrap());
        assert_eq!(
            seal_exit(&case.token, false, false),
            ExitDecision::PolicyClose(proof),
            "the exit decision is sealed once"
        );
        assert_eq!(
            state.submission.lock().unwrap().submission,
            HerdrSubmission::Unsubmitted
        );
        assert!(!case.token.cancelled.load(Ordering::Acquire));
        assert_eq!(case.escapes(), 0);
        assert!(fx.take_calls().is_empty());
    });
}

/// A stop that lands after the exit sealed Normal stays an intent: the unsubmitted turn sends no
/// Escape, the sealed decision does not flip to a hold, and normal finalize owns the slot.
#[test]
fn cold_stop_after_a_normal_exit_seal_sends_nothing() {
    use crate::services::provider::herdr_before_start::{ExitDecision, seal_exit};
    with_cases(|case, fx, runtime| {
        let state = case.token.herdr_interrupt_state().unwrap();
        state.submission.lock().unwrap().submission = HerdrSubmission::Unsubmitted;
        assert_eq!(seal_exit(&case.token, false, false), ExitDecision::Normal);
        mark(&case.owner.logical_key, Mark::Herdr);
        let reply = runtime.block_on(crate::services::discord::commands::stop::run_slash_stop(
            &case.shared,
            &case.provider,
            case.channel,
        ));
        assert_eq!(
            reply.text(),
            HerdrStop::Requested(HerdrDelivery::NotSent(HerdrNotSent::Pending)).reply()
        );
        runtime.block_on(reply.finish(&case.shared, &case.provider, case.channel));
        assert!(state.user_stop.load(Ordering::Acquire));
        assert_eq!(seal_exit(&case.token, false, false), ExitDecision::Normal);
        assert!(!case.token.cancelled.load(Ordering::Acquire));
        assert_eq!(case.escapes(), 0);
        assert!(fx.take_calls().is_empty());
    });
}

#[test]
fn act7_cold_start_accepts_intent_without_marker_then_sends_once() {
    with_cases(|case, fx, runtime| {
        mark(&case.owner.logical_key, Mark::Absent);
        let pending = runtime.block_on(crate::services::discord::commands::stop::run_slash_stop(
            &case.shared,
            &case.provider,
            case.channel,
        ));
        assert!(
            case.token
                .herdr_interrupt_state()
                .unwrap()
                .user_stop
                .load(Ordering::Acquire)
        );
        assert!(!case.token.cancelled.load(Ordering::Acquire));
        assert!(
            crate::services::discord::tmux::recent_turn_stop_for_channel(case.channel).is_none()
        );
        assert_eq!(case.escapes(), 0);
        assert!(pending.text().contains("기다리는"));
        mark(&case.owner.logical_key, Mark::Herdr);
        let state = case.token.herdr_interrupt_state().unwrap();
        let binding_root = events::test_root();
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let _registry = case.rig.registry_on_this_thread();
                let _binding = TestBindingRoot::enter(binding_root.as_deref());
                crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE
                    .set(true);
                state.own_start_observed(1, "turn-a");
            });
            runtime.block_on(async {
                while !reader.is_finished() {
                    tokio::task::yield_now().await;
                }
            });
            reader.join().unwrap();
        });
        assert_eq!(case.escapes(), 1);
        assert_eq!(
            runtime.block_on(interrupt_herdr(
                case.shared.pg_pool.as_ref().unwrap(),
                &case.token,
                &case.provider
            )),
            HerdrDelivery::NotSent(HerdrNotSent::Duplicate)
        );
        assert_eq!(case.escapes(), 1);
        assert!(fx.take_calls().is_empty());
    });
}

#[test]
fn act7_each_stop_reason_preserves_delivery_and_settlement_axes() {
    use HerdrNotSent::*;
    let reasons = [
        Idle,
        Unobserved,
        Pending,
        Generation,
        Identity,
        Holder,
        Gate,
        SwitchOff,
        Duplicate,
        NotAdmitted,
        SettlementUnavailable,
    ];
    let mut replies = std::collections::HashSet::new();
    for reason in reasons {
        let refused = HerdrStop::Refused(reason);
        assert!(
            replies.insert(refused.reply()),
            "distinct reason {reason:?}"
        );
        assert_eq!(refused.observation().intent, "refused");
        assert_eq!(refused.observation().settlement, None);
        let requested = HerdrStop::Requested(HerdrDelivery::NotSent(reason));
        assert_eq!(requested.observation().delivery, "not_sent");
        assert_eq!(requested.observation().reason, Some(reason.reason()));
        assert_eq!(requested.observation().settlement, Some("host_owned"));
    }
    assert!(!HerdrStop::Refused(Pending).reply().contains("요청은 유지"));
    assert!(
        HerdrStop::Requested(HerdrDelivery::NotSent(Pending))
            .reply()
            .contains("요청은 유지")
    );
}

#[path = "codex_stop_delivery_home_stop_tests.rs"]
mod home_stop;
