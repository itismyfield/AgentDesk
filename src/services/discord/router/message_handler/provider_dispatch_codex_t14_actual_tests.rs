//! One real scratch Herdr execution, actual native client input and mock Discord FullEvent ingress.
//! Only the provider protocol is local and input-driven; hooks and output cross real sockets.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, load_hosted_execution_pg,
};
use crate::services::discord::outbound::o_writer_io::GatewayPort;
use crate::services::session_host::herdr_socket_rig_tests::t14_client::RealServer;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use crate::services::tui_o::writer::host::test_io::{Alarms, AnyLease};
use crate::services::tui_o::writer::host::{self, Custody, HostIo, HostParts, Readiness};
use crate::services::tui_prompt_dedupe::binding_context::HookBindingEnvelope;
use poise::serenity_prelude as serenity;

const CHANNEL: u64 = discord_mock::CHANNEL_ID;
const FALLBACK: &str = "t14-real-codex";
const FIRST_ACTUAL: &str = "actual Codex first Discord input";
const DIRECT_ACTUAL: &str = "actual Codex native client input";
const FOLLOWUP_ACTUAL: &str = "actual Codex later Discord input";
const ANSWERS_ACTUAL: [&str; 3] = [
    "CODEX_ACTUAL_FIRST",
    "CODEX_ACTUAL_DIRECT",
    "CODEX_ACTUAL_FOLLOWUP",
];
const ACTUAL_WAIT: Duration = Duration::from_secs(60);

#[test]
#[ignore = "requires approved pinned scratch Herdr, native client monitoring and throwaway PostgreSQL"]
fn codex_actual_client_and_executor_keep_one_live_execution_pg() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("agentdesk::services::discord::tui_prompt_relay=debug,agentdesk::services::discord::turn_bridge=debug")
        .with_test_writer()
        .with_ansi(false)
        .try_init();
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _endpoint_lock = crate::services::claude_tui::hook_server::tests::ENDPOINT_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _dedupe = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _rotations = dedupe::lock_claude_session_rotations_for_tests();
    dedupe::reset_state_for_tests();
    let mut server = RealServer::new("codex");
    let root = crate::config::runtime_root()
        .expect("the monitored runner supplies its owned runtime root");
    assert!(
        root.is_absolute() && root.starts_with(server.root()),
        "actual runtime stays inside the monitored scratch"
    );
    std::fs::create_dir_all(&root).unwrap();
    let _user_home = Guard::set_path_after_shared_test_env_lock("HOME", &server.dir().join("home"));
    let user_home = PathBuf::from(std::env::var_os("HOME").expect("owned scratch HOME"));
    assert!(
        user_home.is_absolute() && user_home.starts_with(server.root()),
        "auth profiles stay inside the monitored scratch HOME"
    );
    let home = crate::services::provider_auth_profile::extra_account_home(
        &ProviderKind::Codex,
        "t14-empty",
    )
    .unwrap();
    assert_eq!(home, user_home.join(".adk/profiles/codex/t14-empty"));
    let cwd = server.dir().join("work");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let config_path = crate::runtime_layout::config_file_path(&root);
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let _config = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CONFIG", &config_path);
    // A normal managed empty profile unsets credentials; it never imports an operator auth home.
    let org = serde_yaml::to_string(&json!({"version":1, "agents":{},
        "provider_auth_profiles":{"t14-empty":{"provider":"codex", "home":home, "env":{"OPENAI_API_KEY":""}}},
        "provider_auth_primary_profiles":{"codex":"t14-empty"}})).unwrap();
    crate::services::discord::org_schema::parse_org_schema(&org)
        .expect("normal empty managed auth profile");
    std::fs::write(crate::runtime_layout::org_schema_path(&root), org).unwrap();
    let _intake =
        Guard::set_value_after_shared_test_env_lock("ADK_INTAKE_ROUTING_MODE", "disabled".as_ref());
    let _binary =
        Guard::set_path_after_shared_test_env_lock("AGENTDESK_CODEX_PATH", server.fake_provider());
    let _home = Guard::set_path_after_shared_test_env_lock("CODEX_HOME", &home);
    let _hooks = Guard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
        "1".as_ref(),
    );
    server.start();
    let config: crate::config::Config = serde_json::from_value(json!({
        "server": {"port":9}, "cluster": {"instance_id": NODE},
        "runtime": {"herdr_codex_turn_enabled": true},
        "agents": [{"id":"t14", "name":"T14", "provider":"codex", "channels":{
            "codex":{"id":CHANNEL.to_string(), "name":FALLBACK, "workspace":cwd, "runtime":"tui"}}}],
        "tui_o":{"writer":{"channels":[CHANNEL]}},
        "session_hosts":{"herdr":{"endpoints":{"mini":{"execution_node":NODE,
            "socket_path":server.socket(), "herdr_home":server.home(), "herdr_session":server.session()}},
            "channels":{CHANNEL.to_string():KEY}}}
    })).unwrap();
    crate::config::validate_config(&config).expect("complete supported Legacy binding");
    std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
    crate::config::session_hosts::install(&config).unwrap();
    let boot = crate::config::session_hosts::BootSessionHosts::from_config(&config).unwrap();
    assert!(crate::config::session_hosts::with_boot(
        |installed| installed == Some(&boot)
    ));
    let (_store, _era) = crate::services::herdr_launch::o_store_for_test(&root, &[CHANNEL]);
    crate::services::tui_o::channel_policy::install(&config).unwrap();
    crate::config_live_reload::install(config);
    let events = server.dir().join("binding-events");
    std::fs::create_dir_all(&events).unwrap();
    let real_registry = server.installer(boot);
    let install = move || {
        real_registry();
        dedupe::binding_events::set_test_root(Some(&events));
    };
    install();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap();
    runtime.block_on(Box::pin(scenario(&mut server, &home)));
    assert!(server.stop(), "the owned server stops normally");
}

async fn scenario(server: &mut RealServer, home: &Path) {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let health = Arc::new(discord::health::HealthRegistry::new());
    let mut shared = discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let fresh = Arc::get_mut(&mut shared).unwrap();
    fresh.provider = ProviderKind::Codex;
    fresh.token_hash = TOKEN.into();
    fresh.api_port = 9;
    fresh.health_registry = Arc::downgrade(&health);
    let sample = discord_mock::user_message(1, "owner identity");
    {
        let mut settings = shared.settings.write().await;
        settings.provider = ProviderKind::Codex;
        settings.allow_all_users = true;
        settings.owner_user_id = Some(sample.author.id.get());
    }
    let mock = discord_mock::DiscordMockState::new();
    mock.park_first_placeholder.store(false, Ordering::SeqCst);
    let (proxy, gateway, http_server) = discord_mock::start(mock.clone()).await;
    let _http = AbortServer(http_server.abort_handle());
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    shared
        .http
        .cached_serenity_ctx
        .set(ctx.clone())
        .expect("cache owned mock context");
    shared
        .http
        .cached_bot_token
        .set("test-token".into())
        .expect("cache owned mock token");
    shared.restart.reconcile_done.store(true, Ordering::Release);
    health
        .set_utility_bot_http_for_tests(discord::bot_role::UtilityBotRole::Notify, ctx.http.clone())
        .await;
    let _rest = discord::shared_state::test_rest::install(ctx.http.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hook_server = tokio::spawn(async move {
        axum::serve(
            listener,
            crate::services::claude_tui::hook_server::hook_receiver_router(),
        )
        .await
        .unwrap();
    });
    let _hook_server = AbortServer(hook_server.abort_handle());
    let hook_url = format!("http://{address}");
    let _hook_endpoint =
        crate::services::claude_tui::hook_server::publish_hook_endpoint(hook_url.clone());
    let writer = ActualWriter::start(&ctx);
    writer.ready().await;
    assert!(
        crate::services::herdr_launch::o_writer_ready(
            CHANNEL,
            crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
        ),
        "actual O writer and its checkpoint admit input"
    );
    let logical = ProviderKind::Codex.build_tmux_session_name(FALLBACK);
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: TOKEN.into(),
        channel_id: CHANNEL.to_string(),
        logical_key: logical.clone(),
        owner_node: NODE.into(),
        runtime_root: crate::config::runtime_root().unwrap().display().to_string(),
    };
    assert!(
        current_record(&pool, &owner).await.is_none(),
        "no hosted execution is seeded"
    );
    let key =
        discord::adk_session::build_namespaced_session_key(TOKEN, &ProviderKind::Codex, &logical);
    sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id) VALUES ($1, 'codex', 'idle', 'discord_channel', $2, $3)")
        .bind(&key).bind(TOKEN).bind(CHANNEL.to_string()).execute(&pool).await.unwrap();
    assert!(
        current_record(&pool, &owner).await.is_none(),
        "the ordinary Legacy row has no hosted execution"
    );
    let first_id = recent_id();
    let mut completed = shared.inflight_signals.subscribe();
    let mut first = FullEventTask::spawn(&ctx, &shared, first_id, FIRST_ACTUAL);
    let provider = server.provider_ready();
    let auth =
        discord::org_schema::overlay_from_tmux_session(ProviderKind::Codex, &logical).unwrap();
    assert_eq!(auth.profile_id, "t14-empty");
    assert_eq!(auth.home.as_deref(), Some(home));
    assert!(auth.unset.contains("OPENAI_API_KEY") && !auth.env.contains_key("OPENAI_API_KEY"));
    let pid = u32::try_from(provider["pid"].as_u64().expect("actual provider PID")).unwrap();
    let pending = wait_record(&pool, &owner, HostedState::Pending).await;
    assert_eq!(pending.expected.as_ref().unwrap().provider_process.pid, pid);
    assert_eq!(
        pending.expected.as_ref().unwrap().binding_nonce,
        pending.execution_nonce
    );
    let context = PathBuf::from(
        provider["context"]
            .as_str()
            .expect("actual provider context path"),
    );
    let envelope = HookBindingEnvelope::capture_from_env("codex", |key| {
        (key == "AGENTDESK_BINDING_CONTEXT").then(|| context.as_os_str().to_owned())
    })
    .encode()
    .unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let path = home
        .join("sessions")
        .join(format!("rollout-t14-{session}.jsonl"));
    server.write_control(
        &json!({"provider":"codex", "execution_node":NODE, "session_id":session, "transcript_path":path,
        "hook_url":hook_url, "binding_envelope":envelope,
        "prompts":[FIRST_ACTUAL,DIRECT_ACTUAL,FOLLOWUP_ACTUAL], "answers":ANSWERS_ACTUAL}),
    );
    wait_input(server, 1, FIRST_ACTUAL).await;
    let bound = wait_record(&pool, &owner, HostedState::Bound).await;
    assert_eq!(bound.execution_nonce, pending.execution_nonce);
    assert_eq!(bound.expected.as_ref().unwrap().provider_process.pid, pid);
    server.release_answer(1);
    first.join().await;
    settled(&shared, &mock, &mut completed, first_id, ANSWERS_ACTUAL[0]).await;
    writer.drained().await;
    let original = hosted_source::capture("codex", &logical, CHANNEL, &path, Some(&session))
        .expect("actual HTTP SessionStart commits the real input-driven source");
    assert!(original.hosted_admission_required);
    assert_eq!(original.execution_nonce, bound.execution_nonce);
    let options =
        crate::services::codex_tui::session::read_codex_tui_launch_options_fingerprint(&logical);
    assert!(options.is_some());
    let pane = bound.location.as_ref().unwrap().pane_id.clone();
    server.assert_single_execution(&pane, pid);
    let adk_before_direct = server.input_requests();
    assert_eq!(
        adk_before_direct.len(),
        3,
        "T14 first Discord input uses one paste/Enter pair after bootstrap"
    );
    let mut client = server.attach(&pane);
    let mut observed = dedupe::subscribe_observed_prompts();
    assert!(
        hosted_source::current_execution_for_adoption(
            &shared,
            "codex",
            ChannelId::new(CHANNEL),
            &logical
        )
        .await
    );
    tui_prompt_relay::spawn_tui_prompt_relay(shared.clone(), ProviderKind::Codex);
    client.send_prompt(DIRECT_ACTUAL);
    wait_input(server, 2, DIRECT_ACTUAL).await;
    let observation = tokio::time::timeout(ACTUAL_WAIT, async {
        loop {
            let prompt = observed.recv().await.unwrap();
            if prompt.tmux_session_name == logical && prompt.prompt == DIRECT_ACTUAL {
                break prompt;
            }
        }
    })
    .await
    .expect("actual native client input reaches the production observer");
    assert!(
        observation
            .native_turn_id
            .as_ref()
            .is_some_and(|turn| !turn.is_empty())
    );
    assert_eq!(observation.hosted_source.as_ref(), Some(&original));
    let claim = tokio::time::timeout(ACTUAL_WAIT, async {
        loop {
            if let Some(row) =
                inflight::load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL)
            {
                break row;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await;
    assert!(
        claim.is_ok(),
        "t14_codex_actual_direct_live_relay_once_contract"
    );
    let claim = claim.unwrap();
    assert_eq!(
        claim.hosted_execution_nonce.as_deref(),
        Some(original.execution_nonce.as_str())
    );
    assert_eq!(claim.native_turn_id, observation.native_turn_id);
    assert_ne!(claim.user_msg_id, 0);
    assert_eq!(
        server.inputs().len(),
        2,
        "observation submits no duplicate input"
    );
    server.release_answer(2);
    settled(
        &shared,
        &mock,
        &mut completed,
        claim.user_msg_id,
        ANSWERS_ACTUAL[1],
    )
    .await;
    writer.drained().await;
    assert_eq!(
        announcement_count(&mock),
        1,
        "the real hook and its replay announce direct input once"
    );
    assert_eq!(current_record(&pool, &owner).await.unwrap(), bound);
    assert_eq!(
        server.input_requests(),
        adk_before_direct,
        "T14 native direct stage adds zero ADK API inputs"
    );
    assert!(client.detach(), "the native client detaches normally");
    server.assert_single_execution(&pane, pid);
    let later_id = first_id + 1;
    let mut later = FullEventTask::spawn(&ctx, &shared, later_id, FOLLOWUP_ACTUAL);
    wait_input(server, 3, FOLLOWUP_ACTUAL).await;
    server.release_answer(3);
    later.join().await;
    settled(&shared, &mock, &mut completed, later_id, ANSWERS_ACTUAL[2]).await;
    writer.drained().await;
    assert_eq!(current_record(&pool, &owner).await.unwrap(), bound);
    assert_eq!(
        hosted_source::capture("codex", &logical, CHANNEL, &path, Some(&session)),
        Some(original)
    );
    assert_eq!(
        crate::services::codex_tui::session::read_codex_tui_launch_options_fingerprint(&logical),
        options
    );
    server.assert_single_execution(&pane, pid);
    assert_eq!(
        server.input_requests().len(),
        5,
        "T14 exactly two ADK paste/Enter pairs plus one launch command"
    );
    let expected = [
        expected_input(1, FIRST_ACTUAL),
        DIRECT_ACTUAL.to_owned(),
        expected_input(3, FOLLOWUP_ACTUAL),
    ];
    server.assert_input_bytes(expected.each_ref().map(String::as_str));
    let inputs = server.inputs();
    assert_eq!(
        inputs.len(),
        3,
        "two Discord inputs and one native client input, with no duplicate submit"
    );
    for (index, expected) in [FIRST_ACTUAL, DIRECT_ACTUAL, FOLLOWUP_ACTUAL]
        .iter()
        .enumerate()
    {
        assert_eq!(inputs[index]["seq"].as_u64(), Some((index + 1) as u64));
        assert_eq!(
            inputs[index]["prompt"].as_str(),
            Some(expected_input(index + 1, expected).as_str()),
            "actual input retains its Discord body or native prompt"
        );
    }
    assert_eq!(
        inputs[1]["prompt"].as_str(),
        Some(DIRECT_ACTUAL),
        "native client input has no Discord author frame"
    );
    for answer in ANSWERS_ACTUAL {
        assert_eq!(
            answer_count(&mock, answer),
            1,
            "one actual REST answer: {answer}"
        );
    }
    assert_eq!(announcement_count(&mock), 1);
    assert!(mock.unhandled.lock().unwrap().is_empty());
    assert!(
        completed.try_recv().is_err(),
        "no late duplicate completion"
    );
    assert!(dedupe::prompt_anchor_for_response("codex", &logical, CHANNEL).is_none());
    assert!(!dedupe::external_input_relay_lease_present(
        "codex", &logical, CHANNEL
    ));
    eprintln!(
        "T14_ACTUAL_CODEX {}",
        json!({"mode":"legacy_dedicated", "ingress":"mock_full_event",
        "provider_pid":pid, "nonce":bound.execution_nonce, "pane":pane, "source":path,
        "input_receipts":server.input_receipts_path(), "native_client_screen":client.screen_path(),
        "native_client_receipt":client.receipt_path(), "provider":provider,
        "announcement_count":1, "answers":ANSWERS_ACTUAL, "actual_inputs":inputs})
    );
    writer.stop().await;
    pool.close().await;
    database.drop().await;
}

fn recent_id() -> u64 {
    let millis = chrono::Utc::now().timestamp_millis() - 1_420_070_400_000 - 1_000;
    (u64::try_from(millis).unwrap() << 22) | 1
}

#[inline(never)]
fn boxed_actual_intake<'a>(
    ctx: &'a serenity::Context,
    event: &'a serenity::FullEvent,
    data: &'a discord::Data,
) -> impl std::future::Future<Output = Result<(), discord::Error>> + 'a {
    Box::pin(discord::router::handle_event(ctx, event, data))
}
struct FullEventTask(Option<tokio::task::JoinHandle<Result<(), discord::Error>>>);
impl FullEventTask {
    fn spawn(
        ctx: &serenity::Context,
        shared: &Arc<discord::SharedData>,
        id: u64,
        text: &str,
    ) -> Self {
        let ctx = ctx.clone();
        let data = discord::Data {
            shared: shared.clone(),
            token: "test-token".into(),
            provider: ProviderKind::Codex,
        };
        let event = serenity::FullEvent::Message {
            new_message: discord_mock::user_message(id, text),
        };
        Self(Some(tokio::spawn(async move {
            boxed_actual_intake(&ctx, &event, &data).await
        })))
    }
    async fn join(&mut self) {
        let result = tokio::time::timeout(ACTUAL_WAIT, self.0.as_mut().unwrap())
            .await
            .expect("FullEvent intake returns");
        match result {
            Ok(result) => result.unwrap(),
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => panic!("FullEvent intake task: {error}"),
        }
        self.0.take();
    }
}
impl Drop for FullEventTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

async fn current_record(pool: &PgPool, owner: &HostedOwner) -> Option<HostedExecution> {
    let key = HostedLookupKey::Canonical {
        provider: "codex",
        identity: CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: &owner.discord_token_hash,
            channel_id: &owner.channel_id,
        },
    };
    match load_hosted_execution_pg(pool, key).await {
        HostedLookup::Found(found) => match found.record {
            HostedRecord::Known(record) => Some(record),
            _ => None,
        },
        _ => None,
    }
}
async fn wait_record(pool: &PgPool, owner: &HostedOwner, state: HostedState) -> HostedExecution {
    tokio::time::timeout(ACTUAL_WAIT, async {
        loop {
            if let Some(record) = current_record(pool, owner).await {
                if record.state == state && record.expected.is_some() {
                    break record;
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("production intake commits its actual process evidence")
}
async fn wait_input(server: &RealServer, sequence: usize, prompt: &str) {
    let expected = expected_input(sequence, prompt);
    tokio::time::timeout(ACTUAL_WAIT, async {
        loop {
            let inputs = server.inputs();
            assert!(
                inputs.len() <= sequence,
                "no provider input submitted twice"
            );
            if inputs.len() == sequence {
                assert_eq!(inputs[sequence - 1]["seq"].as_u64(), Some(sequence as u64));
                assert_eq!(
                    inputs[sequence - 1]["prompt"].as_str(),
                    Some(expected.as_str())
                );
                assert!(
                    !inputs[sequence - 1]["raw_bytes"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("the fake receives actual pane input bytes")
}
fn expected_input(sequence: usize, prompt: &str) -> String {
    if sequence == 2 {
        return prompt.into();
    }
    let message = discord_mock::user_message(1, prompt);
    format!(
        "[User: {} (ID: {})] {prompt}",
        message.author.name, message.author.id
    )
}
fn answer_count(mock: &discord_mock::DiscordMockState, answer: &str) -> usize {
    mock.message_snapshot()
        .iter()
        .filter(|(_, body)| body.contains(answer))
        .count()
}
fn announcement_count(mock: &discord_mock::DiscordMockState) -> usize {
    mock.post_snapshot()
        .iter()
        .filter(|(_, body)| body.contains(DIRECT_ACTUAL))
        .count()
}
async fn settled(
    shared: &Arc<discord::SharedData>,
    mock: &discord_mock::DiscordMockState,
    completed: &mut tokio::sync::broadcast::Receiver<inflight::InflightSignal>,
    id: u64,
    answer: &str,
) {
    let result = tokio::time::timeout(ACTUAL_WAIT, async {
        loop {
            let active = inflight::load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL);
            let mailbox = discord::mailbox_snapshot(shared, ChannelId::new(CHANNEL)).await;
            if answer_count(mock, answer) == 1
                && active.is_none()
                && mailbox.active_user_message_id.is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await;
    if result.is_err() {
        let active = inflight::load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL);
        let mailbox = discord::mailbox_snapshot(shared, ChannelId::new(CHANNEL)).await;
        eprintln!(
            "T14_SETTLEMENT {}",
            json!({"anchor":id,"answer":answer,
            "answer_count":answer_count(mock, answer),"messages":mock.message_snapshot(),
            "inflight":active,"mailbox_active":mailbox.active_user_message_id})
        );
    }
    result.expect("actual native answer REST post and natural lifecycle completion");
    let signal = tokio::time::timeout(ACTUAL_WAIT, completed.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(signal, inflight::InflightSignal::Completed { channel_id:CHANNEL, turn_id } if turn_id == id)
    );
}

struct ActualIo {
    port: Arc<GatewayPort>,
    alarms: Alarms,
}
impl HostIo for ActualIo {
    type Port = GatewayPort;
    type Lease = AnyLease;
    type Alarms = Alarms;
    type Bindings = ChannelBindingLog;
    async fn port(&self) -> Arc<GatewayPort> {
        self.port.clone()
    }
    fn lease(&self) -> AnyLease {
        AnyLease
    }
    fn alarms(&self) -> Alarms {
        self.alarms.clone()
    }
    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<ChannelBindingLog> {
        Arc::new(ChannelBindingLog::new(channel, provider))
    }
    async fn activation_facts(
        &self,
        _: u64,
        _: ShadowProvider,
    ) -> Result<crate::services::tui_o::writer::activation::ActivationFacts, String> {
        Ok(Default::default())
    }
    fn local_custody(&self, _: u64, _: ShadowProvider) -> Result<Custody, String> {
        Ok(Custody::Free)
    }
    fn legacy(&self) -> Arc<dyn crate::services::tui_o::writer::adoption::LegacyView> {
        Arc::new(crate::services::tui_o::writer::adoption::NoLegacy)
    }
    async fn legacy_busy(&self, _: u64) -> bool {
        false
    }
    fn relaying(&self, _: u64) -> bool {
        false
    }
}
struct ActualWriter {
    handles: Vec<host::ManagedWriterHandle>,
    ready: Arc<Readiness>,
    io: Arc<ActualIo>,
}
impl ActualWriter {
    fn start(ctx: &serenity::Context) -> Self {
        let io = Arc::new(ActualIo {
            port: Arc::new(GatewayPort::new(
                ctx.http.clone(),
                ctx.cache.current_user().id.get(),
            )),
            alarms: Alarms::default(),
        });
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let ready = host::process_readiness();
        let root = crate::config::runtime_root().unwrap();
        let handles = host::start_managed(
            ShadowProvider::Codex,
            true,
            crate::services::tui_o::cutover::boot_ownership(),
            || HostParts {
                io: io.clone(),
                runtime_root: Some(root),
                gate,
                readiness: ready.clone(),
            },
        );
        assert_eq!(
            handles.len(),
            1,
            "actual managed writer hosts the registered channel"
        );
        Self { handles, ready, io }
    }
    async fn ready(&self) {
        tokio::time::timeout(ACTUAL_WAIT, async {
            while !self.ready.is_ready(CHANNEL) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("actual writer readiness and baseline checkpoint");
    }
    async fn drained(&self) {
        self.ready().await;
        let view = self
            .ready
            .undelivered(CHANNEL)
            .expect("running actual O actor");
        let wanting = view.demand.want();
        let mut published = view.published.clone();
        published.borrow_and_update();
        tokio::time::timeout(ACTUAL_WAIT, async {
            loop {
                published.changed().await.expect("O actor remains running");
                if published.borrow_and_update().as_ref().is_some_and(|poll| {
                    poll.asked >= wanting.asked()
                        && poll.owed == 0
                        && poll.prepared == 0
                        && poll.unsealed == 0
                        && poll.uncaptured == 0
                        && poll.binding_pending == 0
                }) {
                    break;
                }
            }
        })
        .await
        .expect("actual O captures and delivers the real source");
        assert!(
            self.io.alarms.0.lock().unwrap().is_empty(),
            "no hidden writer alarm"
        );
    }
    async fn stop(self) {
        for handle in self.handles {
            tokio::time::timeout(ACTUAL_WAIT, handle.stop_and_join())
                .await
                .unwrap()
                .unwrap();
        }
    }
}
