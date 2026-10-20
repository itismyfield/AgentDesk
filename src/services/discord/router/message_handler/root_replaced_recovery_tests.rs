use super::super::Fixture;
use super::*;
use crate::db::dispatched_session_canonical_identity::pg_tests as canonical_fixture;
use crate::services::discord::SharedData;
use crate::services::discord::herdr_terminate::root_replaced as recovery;
use crate::services::discord::recovery_engine::herdr_reader::{
    ATTACHES, reconnect_restarted_herdr_with_shared,
};
use crate::services::session_host::herdr_socket_rig_tests::{PROVIDER, SHELL};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};
use poise::serenity_prelude::{ChannelId, MessageId, UserId};

struct IntakeGate {
    previous: crate::config::Config,
    trace: crate::services::platform::tmux::session_inventory::tests::ChannelProbeTrace,
    _namespace: crate::services::platform::tmux::session_inventory::tests::ChannelNamespace,
    _confirmed: crate::services::tui_o::turn_mode::TestConfirmation,
}

impl IntakeGate {
    fn configure(case: &Case) -> Self {
        crate::config::test_env_lock::assert_shared_test_env_lock_held();
        let root = crate::config::runtime_root().unwrap();
        let previous = crate::config_live_reload::current()
            .map(|config| (*config).clone())
            .unwrap_or_default();
        let gate = Self {
            previous,
            trace:
                crate::services::platform::tmux::session_inventory::tests::ChannelProbeTrace::new(
                    CHANNEL,
                ),
            _namespace:
                crate::services::platform::tmux::session_inventory::tests::ChannelNamespace::new(
                    CHANNEL,
                    root.join("rr-private-tmux.sock"),
                ),
            _confirmed: crate::services::tui_o::turn_mode::TestConfirmation::new(CHANNEL),
        };
        let name = case.fx.logical().strip_prefix("AgentDesk-claude-").unwrap();
        let mut config: crate::config::Config = serde_yaml::from_str(&format!(
            "server: {{}}\ncluster: {{instance_id: {NODE}}}\nagents:\n  - id: rr-fixture\n    name: rr-fixture\n    provider: claude\n    channels:\n      claude: {{id: '{CHANNEL}', name: {name}, runtime: tui}}\n"
        )).unwrap();
        config.session_hosts =
            crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
        let path = crate::runtime_layout::config_file_path(&root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_yaml::to_string(&config).unwrap()).unwrap();
        crate::config_live_reload::install(config);
        case.fx.rt.block_on(async {
            use crate::db::dispatched_session_canonical_identity::{
                CanonicalSessionIdentity, SessionIdentityKind, upsert_hook_session_with_identity_pg,
            };
            use crate::db::dispatched_sessions::hosted_execution::{
                HostedLookupKey, load_hosted_execution_pg,
            };
            let primary = super::super::session_key(&case.fx.owner);
            let local = crate::services::discord::adk_session::build_namespaced_session_key(
                &case.shared.token_hash,
                &ProviderKind::Claude,
                case.fx.logical(),
            );
            let before =
                load_hosted_execution_pg(&case.fx.pool, HostedLookupKey::SessionKey(&primary))
                    .await;
            let channel = CHANNEL.to_string();
            let mut params = canonical_fixture::params(&local, &channel);
            params.instance_id = Some(NODE);
            let outcome = upsert_hook_session_with_identity_pg(
                &case.fx.pool,
                params,
                Some(CanonicalSessionIdentity {
                    kind: SessionIdentityKind::DiscordChannel,
                    discord_token_hash: TOKEN,
                    channel_id: &channel,
                }),
            )
            .await
            .unwrap();
            assert!(!outcome.inserted);
            assert_eq!(outcome.session_key, primary);
            assert_eq!(
                load_hosted_execution_pg(&case.fx.pool, HostedLookupKey::SessionKey(&local)).await,
                before,
                "the intake hostname alias must preserve the full original hosted row"
            );
            assert_eq!(
                load_hosted_execution_pg(&case.fx.pool, HostedLookupKey::SessionKey(&primary))
                    .await,
                before,
                "the fixture primary key and hosted row remain unchanged"
            );
            crate::services::discord::host_defer_gate::tests::map_channel(
                &case.shared,
                ChannelId::new(CHANNEL),
                name,
            )
            .await;
        });
        gate
    }
}

impl Drop for IntakeGate {
    fn drop(&mut self) {
        crate::config_live_reload::install(self.previous.clone());
    }
}

fn queued_input() -> Intervention {
    let message_id = MessageId::new(7_186_911);
    Intervention {
        author_id: UserId::new(7_186_910),
        author_is_bot: false,
        message_id,
        queued_generation: 1,
        source_message_ids: vec![message_id],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "복구 뒤 대기 입력".into(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

struct Case {
    fx: Fixture,
    nonce: String,
    shared: Arc<SharedData>,
    _hosts: crate::config::session_hosts::ForcedSessionHosts,
}

impl Case {
    fn launched(tag: &str) -> Self {
        let mut fx = Fixture::new(tag, None);
        let _: &crate::db::auto_queue::test_support::TestPostgresDb = fx
            .db
            .as_ref()
            .expect("the recovery fixture owns a PostgreSQL database");
        fx.owner.runtime_root = crate::config::runtime_root().unwrap().display().to_string();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let launcher = Arc::new(Launcher::default());
        let (nonce, started) = launch(&fx, &launcher);
        assert!(started.path.exists());
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
        assert_eq!(
            fx.rig.sends().len(),
            2,
            "the real initial attach admitted one prompt"
        );
        let hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(CHANNEL, NODE)]);
        let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
        let HostedRecord::Known(mut record) = fx.record() else {
            panic!("launched N1")
        };
        // The fixture's initial launch uses its own rig address; recovery uses the forced boot alias.
        let location = record.location.as_mut().unwrap();
        location.endpoint_config_key = endpoint.key;
        location.socket_addr = endpoint.socket_path.display().to_string();
        location.named_session = endpoint.herdr_session;
        fx.store(&record);
        let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
            fx.pool.clone(),
        ));
        Arc::get_mut(&mut shared).unwrap().token_hash = TOKEN.into();
        shared.restart.reconcile_done.store(true, Ordering::Release);
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        crate::services::tui_prompt_dedupe::binding_events::forget_channel_for_tests(CHANNEL);
        fx.rig.restore_as_new_shell(&context_of(&nonce));
        fx.rig.exists(SHELL, false);
        fx.rig.exists(PROVIDER, false);
        fx.rig.show_panes(&[PANE]);
        crate::services::claude::herdr_turn::hold(&nonce).unwrap();
        recovery::prune(&[]);
        recovery::CLEANUPS.with(|count| count.set(0));
        recovery::BEFORE_CAS.with(|slot| slot.borrow_mut().take());
        recovery::AFTER_CAS.with(|slot| slot.borrow_mut().take());
        recovery::AFTER_FINAL_PROOF.with(|slot| slot.borrow_mut().take());
        recovery::CAS_ATTEMPTS.with(|count| count.set(0));
        ATTACHES.with(|count| count.set(0));
        Self {
            fx,
            nonce,
            shared,
            _hosts: hosts,
        }
    }

    fn pass(&self) {
        let _runtime = self.fx.rt.enter();
        let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
        let _registry = self.fx.rig.registry_as_on_this_thread(
            &endpoint.execution_node,
            &endpoint.key,
            &endpoint.socket_path,
            &endpoint.herdr_session,
        );
        reconnect_restarted_herdr_with_shared(&self.shared, &ProviderKind::Claude);
    }

    fn cleanups(&self) -> usize {
        recovery::CLEANUPS.with(std::cell::Cell::get)
    }

    fn target(&self) -> crate::services::session_host::HerdrTarget {
        let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
        let _registry = self.fx.rig.registry_as_on_this_thread(
            &endpoint.execution_node,
            &endpoint.key,
            &endpoint.socket_path,
            &endpoint.herdr_session,
        );
        let HostedRecord::Known(record) = self.fx.record() else {
            panic!("captured N1")
        };
        herdr_endpoints().target(&record).unwrap()
    }

    fn hold_path(&self) -> PathBuf {
        crate::services::claude::herdr_turn::hold_path(&self.nonce).unwrap()
    }

    fn effects(&self) -> Vec<Value> {
        self.fx
            .rig
            .requests()
            .into_iter()
            .filter(|request| {
                request["method"].as_str().is_some_and(|method| {
                    (method.starts_with("pane.")
                        && !matches!(method, "pane.get" | "pane.read" | "pane.process_info"))
                        || method == "workspace.create"
                })
            })
            .collect()
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        recovery::BEFORE_CAS.with(|slot| slot.borrow_mut().take());
        recovery::AFTER_CAS.with(|slot| slot.borrow_mut().take());
        recovery::AFTER_FINAL_PROOF.with(|slot| slot.borrow_mut().take());
        recovery::CAS_ATTEMPTS.with(|count| count.set(0));
    }
}

// Same production executor and hook receiver as the shared turn fixture, using the boot endpoint
// alias for N2 as well as N1 recovery. The first launch's two sends remain in the rig's history.
fn fresh_turn(case: &Case, prompt: &str) -> (Arc<Launcher>, Started) {
    let fx = &case.fx;
    fx.rig.exists(SHELL, true);
    fx.rig.exists(PROVIDER, true);
    fx.rig.foreground(&[PROVIDER]);
    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
    let endpoint = HerdrLaunchEndpoint {
        execution_node: endpoint.execution_node,
        config_key: endpoint.key,
        socket_addr: endpoint.socket_path.display().to_string(),
        herdr_session: endpoint.herdr_session,
    };
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let row = fx.record();
    let log_root = crate::services::tui_prompt_dedupe::binding_events::test_root();
    let (runtime, rig, pool, finished) = (&fx.rt, &fx.rig, &fx.pool, &fx.finished);
    let owner = fx.owner.clone();
    let working_dir = fx.cwd.path();
    let started = Mutex::new(None);
    let cancel = Arc::new(CancelToken::new());
    fx.finished.store(false, Ordering::SeqCst);
    *fx.cancel.lock().unwrap() = cancel.clone();
    std::thread::scope(|scope| {
        let executor = scope.spawn(|| {
            let _runtime = runtime.enter();
            crate::services::tui_prompt_dedupe::binding_events::set_test_root(log_root.as_deref());
            let _registry = rig.registry_as_on_this_thread(
                &endpoint.execution_node,
                &endpoint.config_key,
                Path::new(&endpoint.socket_addr),
                &endpoint.herdr_session,
            );
            let _admission = open_admission();
            let (sender, receiver) = std::sync::mpsc::channel();
            let result = crate::services::tui_o::exact_submission::dispatch(|| {
                herdr_turn::execute(
                    HerdrTurn {
                        pool,
                        owner: owner.clone(),
                        channel_id: CHANNEL,
                        endpoint: endpoint.clone(),
                        row: Some(&row),
                        prompt,
                        working_dir: working_dir.to_str().unwrap(),
                        system_prompt: None,
                        model: None,
                        hook_endpoint: Some("http://127.0.0.1:1".into()),
                        cancel: Some(cancel.clone()),
                    },
                    &ports,
                    sender,
                )
            });
            let _: Vec<_> = receiver.try_iter().collect();
            finished.store(true, Ordering::SeqCst);
            result
        });
        let played = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(next) = fx.start_provider(&launcher, true) {
                if wait_for(&fx.finished, "the fresh N2 prompt", || {
                    fx.rig.sends().len() == 4
                }) {
                    let session = &next.session;
                    append(
                        &next.path,
                        &[
                            json!({"type":"user", "sessionId":session, "message":{"role":"user", "content":prompt}}),
                            json!({"type":"assistant", "sessionId":session, "message":{"role":"assistant", "stop_reason":"end_turn", "content":[{"type":"text", "text":"답"}]}}),
                            json!({"type":"system", "subtype":"turn_duration", "sessionId":session}),
                        ],
                    );
                    *started.lock().unwrap() = Some(next);
                }
            }
        }));
        let deadline = Instant::now() + LIMIT;
        while played.is_ok() && !fx.finished.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if played.is_err() || !fx.finished.load(Ordering::SeqCst) {
            cancel.cancelled.store(true, Ordering::SeqCst);
        }
        let result = executor.join().unwrap();
        if let Err(panic) = played {
            std::panic::resume_unwind(panic);
        }
        assert_eq!(result, Ok(()));
    });
    assert_eq!(
        ports.attaches.lock().unwrap().len(),
        1,
        "fresh N2 actually attached"
    );
    (launcher, started.into_inner().unwrap().unwrap())
}

#[test]
fn rr_01_restart_retires_the_proven_old_execution_and_fresh_turn_binds_n2_once_pg() {
    let case = Case::launched("rr01");
    let intake = IntakeGate::configure(&case);
    let queued = queued_input();
    let channel = ChannelId::new(CHANNEL);
    let provider = ProviderKind::Claude;
    case.fx.rt.block_on(async {
        case.shared
            .mailbox(channel)
            .replace_queue(
                vec![queued.clone()],
                crate::services::discord::queue_persistence_context(
                    &case.shared,
                    &provider,
                    channel,
                ),
            )
            .await;
    });
    let before = case.effects();
    let old = case.fx.record();
    let after_cas = Arc::new(AtomicBool::new(false));
    let shared = case.shared.clone();
    let pool = case.fx.pool.clone();
    let key = super::super::session_key(&case.fx.owner);
    let target = case.target();
    recovery::BEFORE_CAS.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            Box::pin(async move {
                assert!(
                    shared
                        .session_transition_lock(ChannelId::new(CHANNEL))
                        .try_lock_owned()
                        .is_err(),
                    "recovery holds the common session transition"
                );
                assert!(
                    matches!(
                        target.termination_fence(),
                        Err(crate::services::session_host::HerdrGateRefusal::MutationBusy)
                    ),
                    "recovery holds the captured N1 mutation fence"
                );
                assert!(
                    ATTACHES.with(|count| count.get() > 0),
                    "the real restart attach refused RootReplaced before the fenced stage"
                );
                let raw = canonical_fixture::raw_of(&pool, &key).await;
                assert!(matches!(HostedRecord::decode(raw.as_ref()), HostedRecord::Known(record) if record.state == HostedState::Bound));
                use crate::services::discord::turn_presence::activity::{Activity, activity_now};
                let before = tokio::time::timeout(
                    Duration::from_secs(15),
                    activity_now(&shared, &ProviderKind::Claude, channel),
                )
                .await
                .unwrap();
                assert_eq!(before.activity, Activity::Unknown, "Bound RootReplaced starts held");
                assert_eq!(before.reason, "session_identity_unresolved");
                let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel).await;
                assert!(snapshot.pending_user_dispatch.is_none());
                assert_eq!(snapshot.intervention_queue.len(), 1);
            })
        }))
    });
    let reached = after_cas.clone();
    recovery::AFTER_CAS.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            Box::pin(async move {
                reached.store(true, Ordering::SeqCst);
            })
        }))
    });
    case.pass();
    assert!(
        ATTACHES.with(|count| count.get() > 0),
        "actual restart source attach attempted"
    );
    let HostedRecord::Known(retired) = case.fx.record() else {
        panic!("retired N1")
    };
    let HostedRecord::Known(original) = old else {
        unreachable!()
    };
    assert_eq!(retired.state, HostedState::Retired);
    assert!(
        after_cas.load(Ordering::SeqCst),
        "the production recovery CAS completed"
    );
    assert_eq!(retired.execution_nonce, case.nonce);
    assert_eq!(
        retired.expected, original.expected,
        "RootReplaced never restamps expected"
    );
    assert_eq!(case.cleanups(), 1);
    assert!(!case.hold_path().exists());
    assert_eq!(case.effects(), before, "recovery has no pane effects");
    let taken = case.fx.rt.block_on(async {
        use crate::services::discord::turn_presence::activity::{Activity, activity_now};
        let observed = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let observed = activity_now(&case.shared, &provider, channel).await;
                if observed.reason != "catching_up" {
                    break observed;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Retired activity did not finish catching_up within 15 seconds");
        assert_eq!(
            observed.activity,
            Activity::Idle,
            "Retired must pass existing owner and absence checks: reason={}",
            observed.reason
        );
        assert_eq!(observed.reason, "no_session");
        let taken = tokio::time::timeout(
            Duration::from_secs(15),
            crate::services::discord::mailbox_take_next_automatic_intervention(
                &case.shared,
                &provider,
                channel,
            ),
        )
        .await
        .unwrap();
        assert!(taken.persistence_error.is_none());
        assert!(!taken.has_more);
        assert_eq!(
            taken.intervention.as_ref().map(|item| item.message_id),
            Some(queued.message_id)
        );
        assert!(
            taken.dispatch_lease.is_some(),
            "actual actor take reserves the dequeued input"
        );
        let snapshot = crate::services::discord::mailbox_snapshot(&case.shared, channel).await;
        assert_eq!(snapshot.pending_user_dispatch, Some(queued.message_id));
        assert!(snapshot.intervention_queue.is_empty());
        taken
    });
    assert!(
        intake.trace.calls() > 0,
        "the real absence inventory gate ran"
    );
    let (on_disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &provider,
        &case.shared.token_hash,
        channel,
    );
    assert!(
        on_disk.is_empty(),
        "the dequeued input is absent from the durable queue"
    );
    let dequeued = taken.intervention.as_ref().unwrap();
    assert_eq!(dequeued.text, queued.text);
    let (launcher, started) = fresh_turn(&case, &dequeued.text);
    let HostedRecord::Known(next) = case.fx.record() else {
        panic!("Bound N2")
    };
    assert_eq!(next.state, HostedState::Bound);
    assert_ne!(next.execution_nonce, case.nonce);
    assert_eq!(next.owner, case.fx.owner);
    assert_eq!(next.source_ref.execution_nonce, next.execution_nonce);
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
    let location = next.location.unwrap();
    assert_eq!(location.endpoint_config_key, endpoint.key);
    assert_eq!(Path::new(&location.socket_addr), endpoint.socket_path);
    assert_eq!(location.named_session, endpoint.herdr_session);
    let source =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(case.fx.logical())
            .unwrap();
    assert_eq!(source.session_id.as_deref(), Some(started.session.as_str()));
    assert_eq!(Path::new(&source.output_path), started.path);
    let exact = crate::services::tui_o::shadow::binding_reader::source_id_for(
        &started.session,
        &started.path,
    )
    .unwrap();
    assert!(crate::services::tui_prompt_dedupe::binding_events::binding_events_since(CHANNEL, 0)
        .unwrap().iter().any(|event| {
            use crate::services::tui_prompt_dedupe::binding_events::BindingTarget;
            event.execution_nonce.as_deref() == Some(next.execution_nonce.as_str())
                && matches!(&event.new, BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } if source == &exact)
        }), "Bound N2 published its exact physical native source under its own nonce");
    assert_eq!(
        case.fx.rig.sends().len(),
        4,
        "N2 takes one paste and one Enter"
    );
    assert_eq!(
        case.fx.rig.sends()[2]["text"],
        format!("\u{1b}[200~{}\u{1b}[201~", dequeued.text)
    );
}

#[derive(Clone, Copy)]
enum Change {
    RootAlive,
    ProviderAlive,
    NewProvider,
    ReadFailed,
    Nonce,
    Active,
    Inflight,
}

fn refuses_change(change: Change, tag: &str) {
    refuses_change_at(change, tag, false);
}
fn refuses_change_at(change: Change, tag: &str, after_final_proof: bool) {
    let case = Case::launched(tag);
    let before = case.effects();
    let old = case.fx.record();
    let reached = Arc::new(AtomicBool::new(false));
    let after_cas = Arc::new(AtomicBool::new(false));
    let (stage_tx, stage_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let probe_shared = case.shared.clone();
    let target = case.target();
    let probe = if after_final_proof {
        &recovery::AFTER_FINAL_PROOF
    } else {
        &recovery::BEFORE_CAS
    };
    probe.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            Box::pin(async move {
                stage_tx.send(()).unwrap();
                assert!(
                    probe_shared
                        .session_transition_lock(ChannelId::new(CHANNEL))
                        .try_lock_owned()
                        .is_err(),
                    "recovery holds the common session transition before CAS"
                );
                assert!(
                    matches!(
                        target.termination_fence(),
                        Err(crate::services::session_host::HerdrGateRefusal::MutationBusy)
                    ),
                    "recovery holds the captured N1 mutation fence before CAS"
                );
                tokio::time::timeout(Duration::from_secs(5), resume_rx)
                    .await
                    .unwrap()
                    .unwrap();
            })
        }))
    });
    let after = after_cas.clone();
    recovery::AFTER_CAS.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            Box::pin(async move {
                after.store(true, Ordering::SeqCst);
            })
        }))
    });
    let rig = &case.fx.rig;
    let pool = case.fx.pool.clone();
    let key = super::super::session_key(&case.fx.owner);
    let runtime = case.fx.rt.handle().clone();
    let shared = case.shared.clone();
    let logical = case.fx.logical().to_string();
    let HostedRecord::Known(mut changed) = old.clone() else {
        unreachable!()
    };
    let mutation_reached = reached.clone();
    let changed = std::thread::scope(|scope| {
        let mutation = scope.spawn(move || {
            stage_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("strict first proof reached the fenced production stage");
            mutation_reached.store(true, Ordering::SeqCst);
            match change {
                Change::RootAlive => rig.exists(SHELL, true),
                Change::ProviderAlive => rig.exists(PROVIDER, true),
                Change::NewProvider => {
                    const OTHER_NONCE: &str = "fedcba9876543210fedcba9876543210";
                    let nonce = if changed.execution_nonce == OTHER_NONCE {
                        NONCE
                    } else {
                        OTHER_NONCE
                    };
                    rig.run_provider(&rig.context(nonce), true);
                    rig.exists(PROVIDER, true);
                    rig.foreground(&[PROVIDER]);
                }
                Change::ReadFailed => rig.answer(
                    "pane.process_info",
                    json!({"type":"pane_process_info", "process_info":{"pane_id":PANE}}),
                ),
                Change::Nonce => {
                    changed.execution_nonce = "rr-concurrent-n2".into();
                    changed.source_ref.execution_nonce = changed.execution_nonce.clone();
                    changed.expected.as_mut().unwrap().binding_nonce =
                        changed.execution_nonce.clone();
                    runtime.block_on(async {
                        canonical_fixture::set_raw(
                            &pool,
                            &key,
                            Some(serde_json::to_value(&changed).unwrap()),
                        )
                        .await;
                    });
                }
                Change::Active => runtime.block_on(async {
                    assert!(
                        crate::services::discord::mailbox_try_start_turn(
                            &shared,
                            ChannelId::new(CHANNEL),
                            Arc::new(CancelToken::new()),
                            UserId::new(7_186_901),
                            MessageId::new(7_186_902)
                        )
                        .await
                    );
                    crate::services::discord::increment_global_active(
                        &shared,
                        "root_replaced_negative_fixture",
                    );
                }),
                Change::Inflight => {
                    let row = crate::services::discord::inflight::InflightTurnState::new(
                        ProviderKind::Claude,
                        CHANNEL,
                        None,
                        7_186_901,
                        7_186_902,
                        7_186_903,
                        "original active execution".into(),
                        Some("native-original".into()),
                        Some(logical.clone()),
                        None,
                        None,
                        0,
                    );
                    crate::services::discord::inflight::save_inflight_state(&row).unwrap();
                }
            }
            let _ = resume_tx.send(());
            changed
        });
        case.pass();
        mutation.join().unwrap()
    });
    assert!(reached.load(Ordering::SeqCst));
    if after_final_proof {
        assert_eq!(
            recovery::CAS_ATTEMPTS.with(std::cell::Cell::get),
            0,
            "final current and quiet checks must reject the changed execution before CAS"
        );
    }
    if matches!(change, Change::Nonce) {
        assert_eq!(case.fx.record(), HostedRecord::Known(changed));
    } else {
        assert_eq!(
            case.fx.record(),
            old,
            "the failed final proof must leave N1 Bound"
        );
    }
    assert!(
        !after_cas.load(Ordering::SeqCst),
        "a rejected final proof must not reach successful CAS"
    );
    assert_eq!(case.cleanups(), 0);
    assert!(case.hold_path().exists());
    assert_eq!(case.effects(), before);
    if matches!(change, Change::Active) {
        assert!(
            case.fx
                .rt
                .block_on(crate::services::discord::mailbox_snapshot(
                    &case.shared,
                    ChannelId::new(CHANNEL)
                ))
                .cancel_token
                .is_some()
        );
    }
    if matches!(change, Change::Inflight) {
        assert!(
            crate::services::discord::inflight::load_inflight_state_read_only(
                &ProviderKind::Claude,
                CHANNEL
            )
            .is_some()
        );
    }
}

#[test]
fn rr_recorded_root_alive_after_first_proof_holds_without_cleanup_pg() {
    refuses_change(Change::RootAlive, "rr-root-alive");
}
#[test]
fn rr_provider_alive_after_first_proof_holds_without_cleanup_pg() {
    refuses_change(Change::ProviderAlive, "rr-provider-alive");
}
#[test]
fn rr_new_provider_after_first_proof_holds_without_cleanup_pg() {
    refuses_change(Change::NewProvider, "rr-new-provider");
}
#[test]
fn rr_observation_read_failure_after_first_proof_holds_without_cleanup_pg() {
    refuses_change(Change::ReadFailed, "rr-read-failure");
}
#[test]
fn rr_nonce_replacement_before_cas_preserves_n2_and_old_hold_pg() {
    refuses_change(Change::Nonce, "rr-nonce-race");
}
#[test]
fn rr_active_or_inflight_appearing_after_first_proof_is_preserved_pg() {
    for (change, tag) in [
        (Change::Active, "rr-active-race"),
        (Change::Inflight, "rr-inflight-race"),
    ] {
        std::thread::spawn(move || refuses_change(change, tag))
            .join()
            .unwrap();
    }
}

#[test]
fn rr_post_cas_successor_or_activity_keeps_n1_hold_without_cleanup_pg() {
    for (change, tag) in [
        (Change::Nonce, "rr-post-cas-nonce"),
        (Change::Active, "rr-post-cas-active"),
        (Change::Inflight, "rr-post-cas-inflight"),
    ] {
        // Each case models a separate restart thread, including the reader's listed flag.
        std::thread::spawn(move || {
            let case = Case::launched(tag);
            let effects = case.effects();
            let HostedRecord::Known(original) = case.fx.record() else {
                panic!("launched Bound N1")
            };
            let mut retired = original.clone();
            retired.state = HostedState::Retired;
            let mut successor = original;
            successor.execution_nonce = "rr-post-cas-n2".into();
            successor.source_ref.execution_nonce = successor.execution_nonce.clone();
            successor.expected.as_mut().unwrap().binding_nonce = successor.execution_nonce.clone();
            let replacement = successor.clone();
            let expected_retired = retired.clone();
            let shared = case.shared.clone();
            let pool = case.fx.pool.clone();
            let key = super::super::session_key(&case.fx.owner);
            let logical = case.fx.logical().to_string();
            let reached = Arc::new(AtomicBool::new(false));
            let after_cas = reached.clone();
            let active = Arc::new(CancelToken::new());
            let active_at_hook = active.clone();
            let inflight_path = crate::services::discord::inflight::inflight_state_path(
                &crate::services::discord::inflight::inflight_runtime_root().unwrap(),
                &ProviderKind::Claude,
                CHANNEL,
            );
            let hook_path = inflight_path.clone();
            let inflight_bytes = Arc::new(Mutex::new(None));
            let saved_bytes = inflight_bytes.clone();
            recovery::AFTER_CAS.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    Box::pin(async move {
                        let raw = canonical_fixture::raw_of(&pool, &key).await;
                        assert_eq!(
                            HostedRecord::decode(raw.as_ref()),
                            HostedRecord::Known(expected_retired),
                            "the production CAS must commit Retired before the injected change"
                        );
                        after_cas.store(true, Ordering::SeqCst);
                        match change {
                            Change::Nonce => {
                                canonical_fixture::set_raw(
                                    &pool,
                                    &key,
                                    Some(serde_json::to_value(&replacement).unwrap()),
                                )
                                .await;
                            }
                            Change::Active => {
                                assert!(
                                    crate::services::discord::mailbox_try_start_turn(
                                        &shared,
                                        ChannelId::new(CHANNEL),
                                        active_at_hook,
                                        UserId::new(7_186_921),
                                        MessageId::new(7_186_922),
                                    )
                                    .await
                                );
                                crate::services::discord::increment_global_active(
                                    &shared,
                                    "root_replaced_post_cas_fixture",
                                );
                            }
                            Change::Inflight => {
                                let row =
                                    crate::services::discord::inflight::InflightTurnState::new(
                                        ProviderKind::Claude,
                                        CHANNEL,
                                        None,
                                        7_186_921,
                                        7_186_922,
                                        7_186_923,
                                        "post-CAS active execution".into(),
                                        Some("post-cas-native".into()),
                                        Some(logical),
                                        None,
                                        None,
                                        0,
                                    );
                                crate::services::discord::inflight::save_inflight_state(&row)
                                    .unwrap();
                                *saved_bytes.lock().unwrap() =
                                    Some(std::fs::read(hook_path).unwrap());
                            }
                            _ => unreachable!("only post-CAS identity and activity changes"),
                        }
                    })
                }));
            });
            case.pass();
            assert!(
                reached.load(Ordering::SeqCst),
                "actual successful CAS hook reached"
            );
            let expected = if matches!(change, Change::Nonce) {
                successor
            } else {
                retired
            };
            assert_eq!(case.fx.record(), HostedRecord::Known(expected));
            assert_eq!(
                case.cleanups(),
                0,
                "post-CAS identity or activity change must defer N1 cleanup"
            );
            assert!(
                case.hold_path().exists(),
                "post-CAS change preserves the original N1 hold"
            );
            assert_eq!(
                case.effects(),
                effects,
                "post-CAS deferral has no pane effects"
            );
            if matches!(change, Change::Active) {
                let snapshot = case
                    .fx
                    .rt
                    .block_on(crate::services::discord::mailbox_snapshot(
                        &case.shared,
                        ChannelId::new(CHANNEL),
                    ));
                assert!(
                    snapshot
                        .cancel_token
                        .as_ref()
                        .is_some_and(|token| Arc::ptr_eq(token, &active))
                );
                assert_eq!(
                    snapshot.active_user_message_id,
                    Some(MessageId::new(7_186_922))
                );
            }
            if matches!(change, Change::Inflight) {
                assert_eq!(
                    std::fs::read(inflight_path).unwrap(),
                    inflight_bytes.lock().unwrap().take().unwrap(),
                    "the new inflight execution is byte-identical after deferral"
                );
            }
        })
        .join()
        .unwrap();
    }
}

#[path = "root_replaced_recovery_tests/clear_tests.rs"]
mod clear_tests;
#[path = "root_replaced_recovery_tests/recheck_tests.rs"]
mod recheck_tests;

#[test]
fn rr_retired_cache_skips_completed_cleanup_and_unknown_or_failed_fsync_retries_pg() {
    self::recheck_tests::retired_cache_skips_completed_cleanup_and_unknown_or_failed_fsync_retries();
}

#[test]
fn rr_missing_pane_requires_a_prior_root_replaced_candidate_pg() {
    std::thread::spawn(|| {
        let case = Case::launched("rr-initial-missing");
        let original = case.fx.record();
        let effects = case.effects();
        case.fx.rig.show_panes(&[]);
        case.pass();
        assert!(ATTACHES.with(|count| count.get() > 0));
        assert_eq!(
            case.fx.record(),
            original,
            "initial Missing cannot authorize retirement"
        );
        assert_eq!(case.cleanups(), 0);
        assert!(case.hold_path().exists());
        assert_eq!(case.effects(), effects);
    })
    .join()
    .unwrap();
    std::thread::spawn(|| {
        let case = Case::launched("rr-cached-root-missing");
        case.fx.rig.exists(PROVIDER, true);
        case.pass();
        assert_eq!(case.fx.row(), Some(HostedState::Bound));
        assert!(ATTACHES.with(|count| count.get() > 0));
        let attached = ATTACHES.with(std::cell::Cell::get);
        case.fx.rig.exists(PROVIDER, false);
        case.fx.rig.show_panes(&[]);
        let effects = case.effects();
        case.pass();
        assert_eq!(
            case.fx.row(),
            Some(HostedState::Retired),
            "cached RootReplaced can converge after pane disappearance"
        );
        assert_eq!(ATTACHES.with(std::cell::Cell::get), attached);
        assert_eq!(case.cleanups(), 1);
        assert!(!case.hold_path().exists());
        assert_eq!(case.effects(), effects);
    })
    .join()
    .unwrap();
}
