//! Shared LegacyDedicated protocol composition; no real Herdr/Moshi client or UserDefault policy admission.
use super::super::registered_bootstrap_e2e::{kick_off, recent_message_id, register_channel};
use super::super::{CHANNEL_ID, ProviderStub};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLookup, HostedLookupKey, HostedRecord, HostedState,
    load_hosted_execution_pg,
};
use crate::services::claude_tui::hook_server::{
    hook_receiver_router, publish_hook_endpoint, subscribe_hook_events,
};
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::tui_prompt_dedupe::binding_context::{BINDING_HEADER, HookBindingEnvelope};
use serde_json::{Value, json};
use std::path::Path;

use crate::services::discord::outbound::o_writer_io::GatewayPort;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use crate::services::tui_o::writer::host::test_io::{Alarms, AnyLease};
use crate::services::tui_o::writer::host::{self, Custody, HostIo, HostParts, Readiness};

const FIRST: &str = "T14 first Discord input";
const FOLLOWUP: &str = "T14 Discord followup after the direct turn";
const TIMEOUT: Duration = Duration::from_secs(45);

#[path = "herdr_direct_actual_client_tests.rs"]
mod real_tests;

#[test]
fn discord_direct_prompt_and_followup_keep_one_live_execution_source_pg() {
    let exact = concat!(
        module_path!(),
        "::discord_direct_prompt_and_followup_keep_one_live_execution_source_pg"
    );
    if !composed_child(exact, CHANNEL_ID) {
        return;
    }
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish(),
    )
    .unwrap();
    let rig = Arc::new(HerdrRig::start());
    let mut config: crate::config::Config = serde_json::from_value(json!({
        "server":{}, "cluster":{"instance_id":NODE},
        "agents":[{"id":"t14", "name":"T14", "channels":{"claude":{"id":CHANNEL_ID.to_string(), "runtime":"tui"}}}],
        "runtime":{"herdr_turn_enabled":true},
        "session_hosts":{"herdr":{"endpoints":{"mini":{"execution_node":NODE,
            "socket_path":rig.socket(), "herdr_home":"/adk/herdr", "herdr_session":SESSION}},
            "channels":{CHANNEL_ID.to_string():KEY}}}
    })).unwrap();
    config.runtime.herdr_turn_enabled = Some(true);
    crate::config::session_hosts::install(&config).unwrap();
    let events = tempfile::tempdir().unwrap();
    let registry = rig.thread_installer_with_launch(NODE, KEY, SESSION);
    let root = events.path().to_owned();
    let install = move || {
        registry();
        dedupe::binding_events::set_test_root(Some(&root));
        std::mem::forget(crate::services::herdr_launch::force_writer_accepts(Some(
            true,
        )));
        std::mem::forget(
            crate::services::tui_o::cutover::intake_route::test_probe::answer_with(|_| true),
        );
    };
    install();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap()
        .block_on(Box::pin(scenario(rig, config)));
}

// Reuse the same executable and P11 environment; nested summaries remain inside one raw JSON line.
fn composed_child(path: &str, channel: u64) -> bool {
    use crate::services::session_host::herdr_termination_rig as herdr;
    if herdr::is_child() {
        assert!(herdr::in_child(path, channel));
        return true;
    }
    let home = tempfile::tempdir().unwrap();
    let executable = std::env::current_exe().unwrap();
    let mut command = herdr::child_command(path, channel, home.path());
    assert_eq!(
        command.get_program(),
        executable.as_os_str(),
        "T14 child reuses the current test executable"
    );
    let binary = command
        .get_program()
        .to_str()
        .expect("test binary path UTF-8")
        .to_owned();
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_str().expect("test argument UTF-8").to_owned())
        .collect();
    let output = command.output().unwrap();
    let exact = path.split_once("::").unwrap().1;
    eprintln!(
        "T14_CHILD_RAW {}",
        json!({
            "exact":exact, "binary":binary, "args":args,
            "status":output.status.to_string(), "success":output.status.success(),
            "stdout_bytes":output.stdout, "stderr_bytes":output.stderr,
        })
    );
    let stdout = std::str::from_utf8(&output.stdout).expect("child stdout UTF-8");
    let stderr = std::str::from_utf8(&output.stderr).expect("child stderr UTF-8");
    let summaries: Vec<_> = stdout
        .lines()
        .filter(|line| line.starts_with("test result:"))
        .collect();
    let selected = stdout
        .lines()
        .filter(|line| *line == "running 1 test")
        .count()
        == 1;
    let skip = regex::Regex::new(r"(?i)(?:\bsoft[-_ ]?skip\b|\bPG_NOT_RUN\b|\bPG_THROWAWAY_UNAVAILABLE\b|\bNOT_RUN\b|\bskipping\b.*(?:\btest\b|postgres unavailable|database unavailable)|postgres.*not configured)").unwrap();
    let skipped = stdout.lines().chain(stderr.lines()).any(|line| {
        skip.is_match(line)
            && !line.contains("skipping postgres test database cleanup without an ownership token")
    });
    let verified = output.status.success()
        && selected
        && summaries.len() == 1
        && summaries[0].starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; ")
        && !skipped;
    assert!(
        verified,
        "T14 isolated scenario child did not complete its single selected test; inspect T14_CHILD_RAW"
    );
    eprintln!("isolated child verified: {exact}; selected=1");
    false
}

async fn execution(pool: &sqlx::PgPool, token: &str) -> Option<HostedExecution> {
    let channel = CHANNEL_ID.to_string();
    let key = HostedLookupKey::Canonical {
        provider: "claude",
        identity: CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: token,
            channel_id: &channel,
        },
    };
    match load_hosted_execution_pg(pool, key).await {
        HostedLookup::Found(row) => match row.record {
            HostedRecord::Known(record) => Some(record),
            _ => None,
        },
        _ => None,
    }
}

fn append(path: &Path, records: &[Value]) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for record in records {
        writeln!(file, "{record}").unwrap();
    }
    file.sync_all().unwrap();
}

fn records(session: &str, prompt: &str, answer: &str) -> [Value; 4] {
    let native_id = uuid::Uuid::new_v4().to_string();
    let stamp = |mut record: Value| {
        record["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
        record["sessionId"] = json!(session);
        record
    };
    [
        stamp(json!({"type":"user", "message":{"role":"user", "content":prompt}})),
        stamp(
            json!({"type":"assistant", "uuid":format!("t14-{native_id}"), "apiBlockIndex":0,
         "message":{"id":native_id, "role":"assistant", "stop_reason":"end_turn",
         "content":[{"type":"text", "text":answer}]}}),
        ),
        stamp(
            json!({"type":"system", "subtype":"stop_hook_summary", "hookCount":1, "hasOutput":true}),
        ),
        stamp(json!({"type":"system", "subtype":"turn_duration"})),
    ]
}

async fn hook(session: &str, event: &str, payload: &Value, envelope: &str) {
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("/hooks/claude/{event}?session_id={session}"))
        .header("content-type", "application/json")
        .header(BINDING_HEADER, envelope)
        .body(Body::from(payload.to_string()))
        .unwrap();
    let response = hook_receiver_router().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
}

// Same managed host/binding log as herdr_terminate_o_e2e, with the idle O fixture's real HTTP port.
struct HttpWriterIo {
    port: Arc<GatewayPort>,
    alarms: Alarms,
}
impl HostIo for HttpWriterIo {
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

struct OConsumer {
    handles: Vec<host::ManagedWriterHandle>,
    readiness: Arc<Readiness>,
    io: Arc<HttpWriterIo>,
}
impl OConsumer {
    fn start(h: &RelayE2eHarness) -> Self {
        let io = Arc::new(HttpWriterIo {
            port: Arc::new(GatewayPort::new(
                h.ctx.http.clone(),
                super::super::discord_mock::BOT_ID,
            )),
            alarms: Alarms::default(),
        });
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let readiness = Arc::new(Readiness::default());
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
        Self {
            handles,
            readiness,
            io,
        }
    }

    async fn drained(&self) {
        assert!(
            wait_until(TIMEOUT, || {
                let ready = self.readiness.is_ready(CHANNEL_ID);
                Box::pin(async move { ready })
            })
            .await,
            "T14 actual O writer becomes ready: {:?}",
            self.io.alarms.0.lock().unwrap()
        );
        let view = self
            .readiness
            .undelivered(CHANNEL_ID)
            .expect("T14 running O actor");
        let wanting = view.demand.want();
        let mut published = view.published.clone();
        published.borrow_and_update();
        let done = tokio::time::timeout(TIMEOUT, async {
            loop {
                published
                    .changed()
                    .await
                    .expect("T14 O actor remains running");
                if published.borrow_and_update().is_some_and(|poll| {
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
        .await;
        assert!(
            done.is_ok(),
            "T14 O actor captures and durably delivers its source"
        );
        assert!(
            self.io.alarms.0.lock().unwrap().is_empty(),
            "T14 O writer raises no hidden alarm"
        );
    }

    async fn stop(self) {
        for handle in self.handles {
            tokio::time::timeout(TIMEOUT, handle.stop_and_join())
                .await
                .expect("T14 O host stops within its deadline")
                .expect("T14 O writer stops cleanly");
        }
    }
}

fn answer_count(h: &RelayE2eHarness, answer: &str) -> usize {
    h.messages()
        .iter()
        .filter(|(_, body)| body.contains(answer))
        .count()
}

async fn settled(h: &RelayE2eHarness, logical: &str, stage: &str, answer: &str) {
    // Only REST-created/edited mock message bodies prove delivery; an empty row is not completion.
    let arrived = wait_until(TIMEOUT, || {
        let (messages, answer) = (h.mock.messages.clone(), answer.to_owned());
        Box::pin(async move {
            messages
                .lock()
                .unwrap()
                .values()
                .any(|(_, body)| body.contains(&answer))
        })
    })
    .await;
    assert!(
        arrived,
        "T14 {stage} answer never reached mock Discord: {:?}",
        h.messages()
    );
    assert_eq!(
        answer_count(h, answer),
        1,
        "T14 {stage} answer is delivered exactly once"
    );
    let clear = wait_until(TIMEOUT, || {
        let (shared, channel, logical) = (h.shared.clone(), h.channel_id, logical.to_owned());
        Box::pin(async move {
            let mailbox = crate::services::discord::mailbox_snapshot(&shared, channel).await;
            mailbox.cancel_token.is_none()
                && crate::services::discord::inflight::load_inflight_state_read_only(
                    &crate::services::provider::ProviderKind::Claude,
                    channel.get(),
                )
                .is_none()
                && !dedupe::external_input_relay_lease_present(
                    PROVIDER_KEY,
                    &logical,
                    channel.get(),
                )
        })
    })
    .await;
    assert!(clear, "T14 {stage} settles naturally before the next input");
    assert_eq!(
        answer_count(h, answer),
        1,
        "T14 {stage} settlement keeps exactly one delivered answer"
    );
}

fn sends(rig: &HerdrRig) -> Vec<Value> {
    rig.requests()
        .into_iter()
        .filter(|r| r["method"] == "pane.send_text" || r["method"] == "pane.send_keys")
        .collect()
}

fn assert_input_pair(pair: &[Value], prompt: &str) {
    assert_eq!(pair.len(), 2, "T14 one paste and one Enter");
    assert_eq!(pair[0]["method"], "pane.send_text");
    assert_eq!(pair[1]["method"], "pane.send_keys");
    assert!(pair[0]["params"]["text"].as_str().unwrap().contains(prompt));
    assert_eq!(pair[1]["params"]["keys"], json!(["enter"]));
    assert!(pair.iter().all(|r| r["params"]["pane_id"] == PANE));
}

async fn scenario(rig: Arc<HerdrRig>, mut config: crate::config::Config) {
    let mut database = None;
    let h = Box::pin(RelayE2eHarness::start_inner_with_env_lock(
        ProviderStub::Success,
        true,
        async {
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            database = Some(db);
            Some(pool)
        },
        false,
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    ))
    .await;
    let database = database.unwrap();
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
    let disk: crate::config::Config =
        serde_yaml::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    let disk_boot = crate::config::session_hosts::BootSessionHosts::from_config(&disk).unwrap();
    assert_eq!(
        disk_boot,
        crate::config::session_hosts::BootSessionHosts::from_config(&config).unwrap(),
        "T14 registered disk and live endpoint/channel/node agree"
    );
    assert!(
        crate::config::session_hosts::with_boot(|boot| boot == Some(&disk_boot)),
        "T14 registered disk and initial boot agree"
    );
    crate::config_live_reload::install(config);
    h.answer_placeholders_immediately();
    h.cache_relay_transport();
    h.use_mock_provider_bot(TIMEOUT).await;
    h.shared
        .restart
        .reconcile_done
        .store(true, Ordering::Release);
    use std::os::unix::fs::PermissionsExt;
    let claude_binary = h.root.path().join("bin/claude");
    std::fs::create_dir_all(claude_binary.parent().unwrap()).unwrap();
    let claude_non_version = h
        .root
        .path()
        .canonicalize()
        .unwrap()
        .join("claude-non-version.invoked");
    assert!(claude_non_version.is_absolute());
    let invocation_marker =
        crate::services::process::shell_escape(claude_non_version.to_str().unwrap());
    std::fs::write(&claude_binary, format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then printf '%s\\n' '2.1.0 (Claude Code)'; exit 0; fi\nprintf '%s\\n' \"$*\" >> {invocation_marker}\nexit 97\n")).unwrap();
    std::fs::set_permissions(&claude_binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let claude_binary = claude_binary.canonicalize().unwrap();
    assert!(
        claude_binary.starts_with(h.root.path()) && claude_binary.file_name().unwrap() == "claude"
    );
    let claude_binary_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CLAUDE_PATH",
        &claude_binary,
    );
    let claude_home = h.root.path().join("claude-config");
    std::fs::create_dir_all(&claude_home).unwrap();
    let _claude_home = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "CLAUDE_CONFIG_DIR",
        &claude_home,
    );
    let _tmux = dedupe::binding_context::tests::fake_tmux(h.root.path());
    let _endpoint = publish_hook_endpoint("http://127.0.0.1:1".into());
    let (store, era) =
        crate::services::herdr_launch::o_store_for_test(h.root.path(), &[CHANNEL_ID]);
    assert_eq!(
        store
            .open_channel(&era, CHANNEL_ID)
            .unwrap()
            .unwrap()
            .binding_checkpoint()
            .unwrap(),
        None,
        "T14 the real writer must follow the fresh production binding events from the start"
    );
    let writer = OConsumer::start(&h);
    rig.answer(
        "ping",
        json!({"type":"pong", "version":"0.9.3", "protocol":22}),
    );
    rig.answer(
        "workspace.create",
        json!({"type":"workspace_created", "root_pane":{
        "pane_id":PANE, "workspace_id":"w1", "tab_id":"w1:1", "revision":0, "cwd":h.root.path()}}),
    );
    rig.show_panes(&[PANE]);
    let ready_screen = "✻ Baked for 2s\n────────────────────────────────────────────────────\n\x1b[36m❯\x1b[0m \n────────────────────────────────────────────────────\n  🤖 Opus(H) │ 7% │ MCP: 2";
    rig.answer(
        "pane.read",
        json!({"type":"pane_read", "read":{
        "pane_id":PANE, "source":"recent_unwrapped", "text":ready_screen,
        "revision":2, "truncated":false}}),
    );

    // The live runtime API normally persists this canonical metadata before provider dispatch.
    // This harness has no direct runtime API; only production launch/attach may fill hosted_execution.
    let provider = crate::services::provider::ProviderKind::Claude;
    let registered = super::super::registered_bootstrap_e2e::FALLBACK;
    let logical_key = provider.build_tmux_session_name(registered);
    let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
        &h.shared.token_hash,
        &provider,
        &logical_key,
    );
    sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id) VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3)")
        .bind(&session_key).bind(&h.shared.token_hash).bind(CHANNEL_ID.to_string())
        .execute(&pool).await.unwrap();
    let channel = CHANNEL_ID.to_string();
    let initial = load_hosted_execution_pg(
        &pool,
        HostedLookupKey::Canonical {
            provider: "claude",
            identity: CanonicalSessionIdentity {
                kind: SessionIdentityKind::DiscordChannel,
                discord_token_hash: &h.shared.token_hash,
                channel_id: &channel,
            },
        },
    )
    .await;
    assert!(
        matches!(initial, HostedLookup::Found(row) if matches!(row.record, HostedRecord::Legacy)),
        "T14 starts with canonical metadata only, before any hosted execution exists"
    );

    // A real Discord history intake/kickoff owns the hosted transition; the test never writes Bound.
    let first = recent_message_id();
    h.seed_channel_history(&[(first, FIRST, false)]);
    h.run_catch_up().await;
    let before_kickoff = h.mailbox().await;
    eprintln!(
        "T14_KICKOFF_BEFORE {}",
        json!({
            "queue_len":before_kickoff.intervention_queue.len(),
            "active":format!("{:?}", before_kickoff.active_user_message_id),
            "checkpoint":h.checkpoint(), "history_queries":format!("{:?}", h.history_queries()),
            "requests":rig.requests(), "unhandled":h.unhandled_requests(),
        })
    );
    let started = kick_off(&h).await;
    let after_kickoff = h.mailbox().await;
    // Catch-up's deferred drain may already own this input; the explicit kick is level-triggered.
    eprintln!(
        "T14_KICKOFF_RESULT {}",
        json!({
            "manual_started":started, "queue_before":before_kickoff.intervention_queue.len(),
            "queue_after":after_kickoff.intervention_queue.len(),
            "active":format!("{:?}", after_kickoff.active_user_message_id),
        })
    );
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
        "T14 the first Discord input starts and persists canonical Pending before provider evidence: manual_started={started} requests={:?} unhandled={:?}",
        rig.requests(),
        h.unhandled_requests()
    );
    let pending = execution(&pool, &h.shared.token_hash).await.unwrap();
    let nonce = pending.execution_nonce.clone();
    let context = dedupe::binding_context::execution_context(PROVIDER_KEY, &nonce).unwrap();
    let launch_script = crate::services::tmux_common::session_temp_path(
        &context.tmux_session,
        crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
    );
    let script = std::fs::read_to_string(launch_script).unwrap();
    let fake_exec = format!(
        "exec {} ",
        crate::services::process::shell_escape(claude_binary.to_str().unwrap())
    );
    assert!(
        script.lines().any(|line| line.starts_with(&fake_exec)),
        "T14 actual prepared launch resolves the isolated fake Claude executable"
    );
    let session = context.expected_native_session_id.unwrap();
    let context_path = crate::config::runtime_root()
        .unwrap()
        .join(format!("runtime/binding_contexts/claude/{nonce}.json"));
    rig.run_provider(&context_path, false);
    rig.exists(
        crate::services::session_host::herdr_socket_rig_tests::PROVIDER,
        true,
    );
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        h.root.path(),
        &session,
        None,
    )
    .unwrap();
    assert!(
        path.starts_with(h.root.path()),
        "T14 transcript stays in the fixture"
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"").unwrap();
    let envelope = HookBindingEnvelope::capture_from_env(PROVIDER_KEY, |name| {
        (name == "AGENTDESK_BINDING_CONTEXT").then(|| context_path.clone().into_os_string())
    })
    .encode()
    .unwrap();
    hook(
        &session,
        "SessionStart",
        &json!({"session_id":session, "source":"startup", "transcript_path":path}),
        &envelope,
    )
    .await;
    assert!(
        wait_until(TIMEOUT, || {
            let ready = sends(&rig).len() >= 2;
            Box::pin(async move { ready })
        })
        .await,
        "T14 first executor sends its actual prompt"
    );
    assert_input_pair(&sends(&rig), FIRST);
    append(&path, &records(&session, FIRST, "T14 first answer"));
    let logical = pending.owner.logical_key.clone();
    settled(&h, &logical, "first Discord turn", "T14 first answer").await;
    writer.drained().await;
    let bound = execution(&pool, &h.shared.token_hash).await.unwrap();
    assert_eq!(
        bound.state,
        HostedState::Bound,
        "T14 production attach created Bound"
    );
    assert_eq!(bound.execution_nonce, nonce);
    assert_eq!(bound.location.as_ref().unwrap().pane_id, PANE);
    let before = dedupe::runtime_binding_for_tmux_session(&logical).unwrap();
    assert_eq!(before.session_id.as_deref(), Some(session.as_str()));
    assert_eq!(Path::new(&before.output_path), path);
    assert!(
        dedupe::hosted_source::current_execution_for_adoption(
            &h.shared,
            PROVIDER_KEY,
            h.channel_id,
            &logical
        )
        .await
    );

    // The fake provider records one human input through the ordinary hook ingress, on that live source.
    let relayed = observe_hooks(&h, subscribe_hook_events(), None);
    let mut direct = records(&session, PROMPT, "T14 direct answer");
    direct[0]["uuid"] = json!(PROMPT_ID);
    append(&path, &direct[..1]);
    let payload = json!({"hook_event_name":"UserPromptSubmit", "session_id":session,
        "transcript_path":path, "prompt":PROMPT, "prompt_id":PROMPT_ID});
    let pin =
        dedupe::hosted_source::capture_hook(PROVIDER_KEY, &logical, CHANNEL_ID, &payload, None)
            .unwrap();
    assert_eq!(pin.execution_nonce, nonce);
    hook(&session, "UserPromptSubmit", &payload, &envelope).await;
    let announced = wait_until(TIMEOUT, || {
        let messages = h.mock.messages.clone();
        Box::pin(async move {
            messages.lock().unwrap().values().any(|(_, body)| {
                !matches!(body.as_str(), "..." | "⠋ 계속 처리 중") && body.contains(PROMPT)
            })
        })
    })
    .await;
    assert!(
        announced,
        "t14_direct_live_relay_once_contract: messages={:?} unhandled={:?}",
        h.messages(),
        h.unhandled_requests()
    );
    assert_eq!(
        announcements(&h),
        1,
        "T14 direct input is mirrored exactly once"
    );
    wait_for_relays(&relayed, 1).await;
    assert!(
        crate::services::discord::inflight::load_inflight_state_read_only(
            &crate::services::provider::ProviderKind::Claude,
            CHANNEL_ID
        )
        .is_some(),
        "T14 ordinary direct input actually creates its synthetic lifecycle before its answer"
    );
    assert_eq!(
        sends(&rig).len(),
        2,
        "T14 observing direct input injects no provider input"
    );
    hook(&session, "UserPromptSubmit", &payload, &envelope).await;
    let _ = settled_counts(&h).await;
    assert_eq!(
        announcements(&h),
        1,
        "T14 duplicate hook creates no duplicate announcement"
    );
    assert_eq!(
        relayed.load(Ordering::SeqCst),
        1,
        "T14 duplicate hook is suppressed before relay"
    );
    append(&path, &direct[1..]);
    settled(&h, &logical, "direct human turn", "T14 direct answer").await;
    writer.drained().await;
    assert_eq!(
        dedupe::hosted_source::capture_hook(PROVIDER_KEY, &logical, CHANNEL_ID, &payload, None)
            .unwrap(),
        pin
    );
    assert_eq!(execution(&pool, &h.shared.token_hash).await.unwrap(), bound);

    // The next real Discord FullEvent reaches the ordinary executor on the exact same execution/source.
    h.deliver_user_message(first + 1, FOLLOWUP).await.unwrap();
    assert!(
        wait_until(TIMEOUT, || {
            let ready = sends(&rig).len() >= 4;
            Box::pin(async move { ready })
        })
        .await,
        "T14 Discord followup reaches the actual executor after the direct turn"
    );
    let all = sends(&rig);
    assert_eq!(all.len(), 4);
    assert_input_pair(&all[..2], FIRST);
    assert_input_pair(&all[2..], FOLLOWUP);
    append(&path, &records(&session, FOLLOWUP, "T14 followup answer"));
    settled(&h, &logical, "Discord followup", "T14 followup answer").await;
    writer.drained().await;
    let after = dedupe::runtime_binding_for_tmux_session(&logical).unwrap();
    assert_eq!(
        (after.session_id, after.output_path),
        (before.session_id, before.output_path)
    );
    assert_eq!(execution(&pool, &h.shared.token_hash).await.unwrap(), bound);
    let creates = rig
        .requests()
        .into_iter()
        .filter(|r| r["method"] == "workspace.create")
        .count();
    assert_eq!(creates, 1, "T14 the full roundtrip launches exactly once");
    assert_eq!(
        h.provider_starts(),
        0,
        "T14 the original harness provider stub stays unused"
    );
    assert!(
        !claude_non_version
            .try_exists()
            .expect("read fake Claude invocation sentinel"),
        "T14 the isolated fake Claude receives no non-version execution"
    );
    assert_eq!(announcements(&h), 1);
    for answer in [
        "T14 first answer",
        "T14 direct answer",
        "T14 followup answer",
    ] {
        assert_eq!(
            answer_count(&h, answer),
            1,
            "T14 no late duplicate answer: {answer}"
        );
    }
    assert!(h.unhandled_requests().is_empty());
    eprintln!(
        "T14_COMPOSED protocol=fake-herdr mode=legacy_dedicated create=1 prompt_pairs=2 direct_relay=1 duplicate=0 nonce={nonce} session={session} source={}",
        path.display()
    );
    writer.stop().await;
    drop(_tmux);
    drop(_claude_home);
    drop(claude_binary_guard);
    drop(h);
    pool.close().await;
    database.drop().await;
}
