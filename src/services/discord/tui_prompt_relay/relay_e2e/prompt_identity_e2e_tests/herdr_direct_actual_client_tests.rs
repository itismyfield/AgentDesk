//! Actual pinned bare client input; all provider, HTTP, PG and filesystem effects are isolated.
use super::*;
use crate::services::session_host::herdr_socket_rig_tests::t14_client::RealServer;

const DIRECT: &str = "ordinary bare client prompt [claude-t14]";
const ANSWERS: [&str; 3] = [
    "CLAUDE_T14_FIRST_ANSWER",
    "CLAUDE_T14_DIRECT_ANSWER",
    "CLAUDE_T14_FOLLOWUP_ANSWER",
];

#[test]
#[ignore = "needs ADK_TEST_HERDR_BIN: pinned sandboxed Herdr native client and owned PG"]
fn claude_bare_client_roundtrip_retains_execution_and_source_pg() {
    let mut server = RealServer::new("claude");
    server.start();
    let config: crate::config::Config = serde_json::from_value(json!({
        "server":{}, "cluster":{"instance_id":NODE,"gateway_preferred_instance_id":NODE},
        "agents":[{"id":"t14","name":"T14","channels":{"claude":{"id":CHANNEL_ID.to_string(),"runtime":"tui"}}}],
        "runtime":{"herdr_turn_enabled":true},
        "tui_o":{"writer":{"channels":[CHANNEL_ID]}},
        "session_hosts":{"herdr":{"endpoints":{"mini":{"execution_node":NODE,
            "socket_path":server.socket(),"herdr_home":server.home(),"herdr_session":server.session()}},
            "channels":{CHANNEL_ID.to_string():KEY}}}
    })).unwrap();
    crate::config::session_hosts::install(&config).unwrap();
    let boot = crate::config::session_hosts::BootSessionHosts::from_config(&config).unwrap();
    let registry = server.installer(boot);
    let events = server.dir().join("binding-events");
    std::fs::create_dir_all(&events).unwrap();
    let install = move || {
        registry();
        dedupe::binding_events::set_test_root(Some(&events));
    };
    install();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap()
        .block_on(Box::pin(scenario(&mut server, config)));
    assert!(server.stop(), "T14 owned Claude processes stop cleanly");
}

async fn input(server: &RealServer, seq: u64, text: &str) {
    assert!(
        wait_until(TIMEOUT, || {
            let found = server
                .inputs()
                .iter()
                .any(|v| v["seq"] == seq && v["prompt"].as_str().is_some_and(|s| s.contains(text)));
            Box::pin(async move { found })
        })
        .await,
        "T14 actual Claude input {seq} arrived through its terminal: {:?}",
        server.inputs()
    );
    let inputs = server.inputs();
    assert_eq!(
        inputs.len(),
        seq as usize,
        "T14 actual input has no duplicate"
    );
    assert_eq!(inputs.last().unwrap()["seq"], seq);
}

async fn scenario(server: &mut RealServer, mut config: crate::config::Config) {
    let mut database = None;
    let h = Box::pin(RelayE2eHarness::start_inner(
        ProviderStub::Success,
        true,
        async {
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            database = Some(db);
            Some(pool)
        },
        false,
    ))
    .await;
    let database = database.unwrap();
    assert!(h.root.path().starts_with(server.root()));
    let pool = h.shared.pg_pool.clone().unwrap();
    register_channel(&h);
    let config_path = h.root.path().join("config/agentdesk.yaml");
    let registered: crate::config::Config =
        serde_yaml::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config.agents = registered.agents;
    let crate::config::AgentChannel::Detailed(channel) =
        config.agents[0].channels.get("claude").unwrap()
    else {
        panic!("registered detailed channel")
    };
    let mut channel = channel.clone();
    channel.runtime = Some("tui".into());
    config.agents[0]
        .channels
        .insert("claude", crate::config::AgentChannel::Detailed(channel));
    std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
    crate::config_live_reload::install(config.clone());
    h.answer_placeholders_immediately();
    h.cache_relay_transport();
    h.use_mock_notify_bot(TIMEOUT).await;
    h.shared
        .restart
        .reconcile_done
        .store(true, Ordering::Release);
    let _provider = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CLAUDE_PATH",
        server.fake_provider(),
    );
    let _home = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "CLAUDE_CONFIG_DIR",
        &server.dir().join("claude-config"),
    );
    let _tmux = dedupe::binding_context::tests::fake_tmux(h.root.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let _endpoint = publish_hook_endpoint(url.clone());
    let hooks = tokio::spawn(async move {
        axum::serve(listener, hook_receiver_router()).await.unwrap();
    });
    let (store, era) =
        crate::services::herdr_launch::o_store_for_test(h.root.path(), &[CHANNEL_ID]);
    assert_eq!(
        store
            .open_channel(&era, CHANNEL_ID)
            .unwrap()
            .unwrap()
            .binding_checkpoint()
            .unwrap(),
        None
    );
    crate::services::tui_o::channel_policy::install(&config).unwrap();
    let writer = start_actual_o_consumer(&h);
    writer.drained().await;
    assert!(
        host::channel_accepts(CHANNEL_ID),
        "T14 actual writer accepts the first input"
    );
    let provider = crate::services::provider::ProviderKind::Claude;
    let logical =
        provider.build_tmux_session_name(super::super::super::registered_bootstrap_e2e::FALLBACK);
    let key = crate::services::discord::adk_session::build_namespaced_session_key(
        &h.shared.token_hash,
        &provider,
        &logical,
    );
    sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id) VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3)")
        .bind(key).bind(&h.shared.token_hash).bind(CHANNEL_ID.to_string())
        .execute(&pool).await.unwrap();
    let first = recent_message_id();
    h.seed_channel_history(&[(first, FIRST, false)]);
    h.run_catch_up().await;
    let _ = kick_off(&h).await;
    assert!(
        wait_until(TIMEOUT, || {
            let (pool, token) = (pool.clone(), h.shared.token_hash.clone());
            Box::pin(async move {
                execution(&pool, &token)
                    .await
                    .is_some_and(|r| r.state == HostedState::Pending && r.location.is_some())
            })
        })
        .await,
        "T14 actual Claude first intake persists Pending with a real pane"
    );
    let pending = execution(&pool, &h.shared.token_hash).await.unwrap();
    let context =
        dedupe::binding_context::execution_context(PROVIDER_KEY, &pending.execution_nonce).unwrap();
    let session = context.expected_native_session_id.unwrap();
    let context_path = crate::config::runtime_root().unwrap().join(format!(
        "runtime/binding_contexts/claude/{}.json",
        pending.execution_nonce
    ));
    let envelope = HookBindingEnvelope::capture_from_env(PROVIDER_KEY, |key| {
        (key == "AGENTDESK_BINDING_CONTEXT").then(|| context_path.clone().into_os_string())
    })
    .encode()
    .unwrap();
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        h.root.path(),
        &session,
        None,
    )
    .unwrap();
    assert!(path.starts_with(server.root()));
    let provider_identity = server.provider_ready();
    let provider_pid = u32::try_from(provider_identity["pid"].as_u64().unwrap()).unwrap();
    assert_eq!(
        provider_identity["context"],
        context_path.to_string_lossy().as_ref()
    );
    server.write_control(
        &json!({"provider":"claude","session_id":session,"transcript_path":path,
        "hook_url":url,"binding_envelope":envelope,
        "prompts":[FIRST,DIRECT,FOLLOWUP],"answers":ANSWERS}),
    );
    input(server, 1, FIRST).await;
    server.release_answer(1);
    settled(&h, &logical, "actual first Discord turn", ANSWERS[0]).await;
    writer.drained().await;
    let bound = execution(&pool, &h.shared.token_hash).await.unwrap();
    assert_eq!(bound.state, HostedState::Bound);
    assert_eq!(bound.execution_nonce, pending.execution_nonce);
    let pane = bound.location.as_ref().unwrap().pane_id.clone();
    let binding = dedupe::runtime_binding_for_tmux_session(&logical).unwrap();
    assert_eq!(binding.session_id.as_deref(), Some(session.as_str()));
    assert_eq!(Path::new(&binding.output_path), path);
    let pin =
        dedupe::hosted_source::capture(PROVIDER_KEY, &logical, CHANNEL_ID, &path, None).unwrap();
    let adk_before_direct = server.input_requests();
    assert_eq!(
        adk_before_direct.len(),
        3,
        "T14 first Discord input uses one paste/Enter pair after bootstrap"
    );
    let relayed = observe_hooks(&h, subscribe_hook_events(), None);
    let mut client = server.attach(&pane);
    client.send_prompt(DIRECT);
    input(server, 2, DIRECT).await;
    assert!(
        wait_until(TIMEOUT, || {
            let messages = h.mock.messages.clone();
            Box::pin(async move {
                messages
                    .lock()
                    .unwrap()
                    .values()
                    .any(|(_, text)| text.contains(DIRECT))
            })
        })
        .await,
        "t14_actual_claude_client_relay_once_contract"
    );
    wait_for_relays(&relayed, 1).await;
    let direct_claim =
        crate::services::discord::inflight::load_inflight_state_read_only(&provider, CHANNEL_ID)
            .expect("T14 actual client input creates a synthetic lifecycle before its response");
    assert_eq!(
        direct_claim.hosted_execution_nonce.as_deref(),
        Some(pin.execution_nonce.as_str()),
        "T14 direct synthetic lifecycle belongs to the same actual execution"
    );
    server.release_answer(2);
    settled(&h, &logical, "actual direct human turn", ANSWERS[1]).await;
    writer.drained().await;
    let _ = settled_counts(&h).await;
    assert_eq!(relayed.load(Ordering::SeqCst), 1);
    assert_eq!(
        dedupe::hosted_source::capture(PROVIDER_KEY, &logical, CHANNEL_ID, &path, None),
        Some(pin.clone())
    );
    assert_eq!(
        server.input_requests(),
        adk_before_direct,
        "T14 native direct stage adds zero ADK API inputs"
    );
    assert_eq!(
        h.mock
            .post_snapshot()
            .iter()
            .filter(|(_, body)| body.contains(DIRECT))
            .count(),
        1,
        "T14 actual direct input notice is posted once even after its replay"
    );
    h.deliver_user_message(first + 1, FOLLOWUP).await.unwrap();
    input(server, 3, FOLLOWUP).await;
    server.release_answer(3);
    settled(&h, &logical, "actual Discord followup", ANSWERS[2]).await;
    writer.drained().await;
    assert_eq!(server.inputs().len(), 3);
    assert_eq!(execution(&pool, &h.shared.token_hash).await.unwrap(), bound);
    assert_eq!(
        dedupe::hosted_source::capture(PROVIDER_KEY, &logical, CHANNEL_ID, &path, None),
        Some(pin)
    );
    for answer in ANSWERS {
        assert_eq!(answer_count(&h, answer), 1);
    }
    assert!(
        client.detach(),
        "T14 native detach exits without ending the provider"
    );
    server.assert_single_execution(&pane, provider_pid);
    assert_eq!(
        server.input_requests().len(),
        5,
        "T14 exactly two ADK paste/Enter pairs plus one launch command"
    );
    server.assert_input_bytes(DIRECT);
    assert_eq!(
        h.mock
            .post_snapshot()
            .iter()
            .filter(|(_, body)| body.contains(DIRECT))
            .count(),
        1,
        "T14 direct notice remains exactly one after the Discord followup"
    );
    assert_eq!(
        server.inputs().len(),
        3,
        "T14 native detach adds no provider input"
    );
    assert!(
        h.unhandled_requests().is_empty(),
        "{:?}",
        h.unhandled_requests()
    );
    assert_eq!(h.provider_starts(), 0, "the old harness stub never runs");
    eprintln!(
        "T14_ACTUAL {}",
        json!({"provider":"claude","nonce":bound.execution_nonce,
        "source":path,"session":session,"pane":pane,"client_inputs":1,"provider_inputs":3,
        "direct_relay":1,"answers":3,"execution_unchanged":true,
        "context_receipt":server.context_seen_path(),"input_receipt":server.input_receipts_path(),
        "client_screen":client.screen_path(),"client_receipt":client.receipt_path()})
    );
    writer.stop().await;
    hooks.abort();
    let _ = hooks.await;
    drop(_home);
    drop(_provider);
    drop(h);
    pool.close().await;
    database.drop().await;
}

// Production intake and this real writer must read the same process readiness registry.
fn start_actual_o_consumer(h: &RelayE2eHarness) -> OConsumer {
    let io = Arc::new(HttpWriterIo {
        port: Arc::new(GatewayPort::new(
            h.ctx.http.clone(),
            h.ctx.cache.current_user().id.get(),
        )),
        alarms: Alarms::default(),
    });
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let readiness = host::process_readiness();
    let root = crate::config::runtime_root().unwrap();
    let owned = crate::services::tui_o::cutover::boot_ownership();
    let handles = host::start_managed(ShadowProvider::Claude, true, owned, || HostParts {
        io: io.clone(),
        runtime_root: Some(root),
        gate,
        readiness: readiness.clone(),
    });
    assert_eq!(
        handles.len(),
        1,
        "T14 one real managed O writer hosts the channel"
    );
    assert!(readiness.is_hosted(CHANNEL_ID));
    OConsumer {
        handles,
        readiness,
        io,
    }
}
