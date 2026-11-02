//! An owned front reaches real intake; a pre-submit busy refusal is not a completed native turn.
use super::*;
use crate::services::discord::queue_io::input_owner::admission::Admission;
use crate::services::discord::router::message_handler::provider_dispatch::submission_tests::{
    d1_execution_proof, d1_fixture_config,
};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{self as discord, SharedData, turn_completion_events};
use poise::serenity_prelude::{ChannelId, MessageId, UserId};

#[derive(Clone, Default)]
struct Trace(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Trace {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Trace {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

impl Trace {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|p| p.into_inner())).into_owned()
    }
}

struct IntakeRig {
    shared: Arc<SharedData>,
    mailbox: ChannelMailboxHandle,
    channel: ChannelId,
    mock: discord_mock::DiscordMockState,
    server: tokio::task::JoinHandle<()>,
    _env: Vec<crate::config::TestEnvVarGuard>,
    _hook: crate::services::claude_tui::hook_server::HookEndpointGuard,
    _tmux: Fixture,
    _api: discord::admin_host_guard::tests::Recorder,
    trace: Trace,
    renderer: std::path::PathBuf,
}

impl Drop for IntakeRig {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl IntakeRig {
    async fn new(id: u64, busy: bool) -> Self {
        Self::with_bot(id, busy, ProviderKind::Claude).await
    }

    async fn with_bot(id: u64, busy: bool, bot: ProviderKind) -> Self {
        let trace = Trace::default();
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(trace.clone())
            .try_init()
            .unwrap();
        let f = Fixture::after_shared_test_env_lock();
        let (mut env, mut config) = d1_fixture_config(&f, &[id]);
        if bot == ProviderKind::Codex {
            let path = f.root.path().join("cg-d1-fixture.yaml");
            let mut yaml: serde_yaml::Value =
                serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            yaml["providers"]["codex"] = serde_yaml::from_str("{runtime: tui}").unwrap();
            yaml["discord"] =
                serde_yaml::from_str("bots: {codex: {token: test-token, provider: codex}}")
                    .unwrap();
            yaml["agents"][0]["channels"]["claude"]["provider"] = "claude".into();
            fs::write(path, serde_yaml::to_string(&yaml).unwrap()).unwrap();
            config = serde_yaml::from_value(yaml).unwrap();
        }
        env.push(
            crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                "AGENTDESK_CODEX_PATH",
                &f.root.path().join("claude-fixture"),
            ),
        );
        env.push(
            crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
                "ADK_INTAKE_ROUTING_MODE",
                std::ffi::OsStr::new("disabled"),
            ),
        );
        crate::config::session_hosts::install(&config).unwrap();
        crate::services::tui_o::channel_policy::install(&config).unwrap();
        let hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(
            "http://127.0.0.1:9".into(),
        );
        let api = discord::admin_host_guard::tests::Recorder::start().await;
        discord::internal_api::init(api.port, None);
        let mut shared = discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared).unwrap().api_port = api.port;
        Arc::get_mut(&mut shared).unwrap().provider = bot.clone();
        shared.settings.write().await.provider = bot.clone();
        assert_eq!(discord::resolve_discord_bot_provider("test-token"), bot);
        let mock = discord_mock::DiscordMockState::new();
        mock.park_first_placeholder
            .store(false, std::sync::atomic::Ordering::SeqCst);
        mock.allow_channel(id);
        mock.name_channel(id, &format!("a1-rollback-{id}"));
        let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
        let ctx = discord_mock::serenity_context(proxy, gateway).await;
        shared.http.cached_serenity_ctx.set(ctx).unwrap();
        shared
            .http
            .cached_bot_token
            .set("test-token".into())
            .unwrap();
        let channel = ChannelId::new(id);
        let label = format!("a1-rollback-{id}");
        let name = ProviderKind::Claude.build_tmux_session_name(&label);
        let renderer = renderer_with(&f, &name, "normal", view("", false), "", PROMPT);
        let proof = d1_execution_proof(&name, id);
        let sid = proof.context.expected_native_session_id.as_deref().unwrap();
        let workspace = f.root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            &workspace,
            sid,
            Some(f.root.path()),
        )
        .unwrap();
        if busy {
            fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            fs::write(&transcript, BUSY).unwrap();
        }
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            &name,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                output_path: transcript.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(sid.into()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        discord::host_defer_gate::tests::map_channel(&shared, channel, &label).await;
        {
            let mut core = shared.core.lock().await;
            let session = core.sessions.get_mut(&channel).unwrap();
            session.current_path = Some(workspace.display().to_string());
            session.session_id = Some(sid.into());
        }
        let mailbox = shared.mailbox(channel);
        let persistence = discord::queue_persistence_context(&shared, &bot, channel);
        mailbox
            .v2_ensure_channel(&persistence, channel)
            .await
            .unwrap();
        let mut original: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
            "message_id":1_479_671_302_387_076_000u64+id,"channel_id":id,
            "queued_generation":1,"text":PROMPT}))
        .unwrap();
        if bot == ProviderKind::Codex {
            original.source_message_ids.push(original.message_id);
            original.source_message_queued_generations.push(serde_json::from_value(json!({
                "message_id": original.message_id, "queued_generation": original.queued_generation,
                "route": {"target": channel, "parent": null, "role": channel}
            })).unwrap());
        }
        let admitted = admit(&mailbox, &original, AdmissionTicket::new()).await;
        assert!(matches!(admitted, Admission::Accepted(_)), "{admitted:?}");
        Self {
            shared,
            mailbox,
            channel,
            mock,
            server,
            _env: env,
            _hook: hook,
            _tmux: f,
            _api: api,
            trace,
            renderer,
        }
    }

    fn drive(
        &self,
    ) -> (
        Arc<crate::services::discord::queue_io::input_owner::drive::Drive>,
        tokio::sync::mpsc::UnboundedReceiver<Result<(), String>>,
    ) {
        let mut drive = super::super::drive(&self.shared, self.channel);
        let actual = drive.handoff.clone();
        let result = Arc::new(std::sync::Mutex::new(None));
        let reported = result.clone();
        Arc::get_mut(&mut drive).unwrap().handoff = Arc::new(move |front| {
            let actual = actual.clone();
            let reported = reported.clone();
            Box::pin(async move {
                let result = actual(front).await;
                *reported.lock().unwrap_or_else(|p| p.into_inner()) = Some(result.clone());
                result
            })
        });
        let (settled, receiver) = tokio::sync::mpsc::unbounded_channel();
        Arc::get_mut(&mut drive).unwrap().settled = Some(Arc::new(move || {
            let settled = settled.clone();
            let result = result.lock().unwrap_or_else(|p| p.into_inner()).take();
            Box::pin(async move {
                let _ = settled.send(result.expect("the real intake handoff ran"));
            })
        }));
        (drive, receiver)
    }

    async fn state(&self) -> Value {
        self.mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a1_owned_busy_rollback_keeps_the_bot_queue_namespace() {
    if !discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "a1_owned_busy_rollback_keeps_the_bot_queue_namespace"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = IntakeRig::with_bot(7_676_104, true, ProviderKind::Codex).await;
    let original = rig
        .mailbox
        .v2_request(|slot| Ok(slot.as_ref().unwrap().front().unwrap().1))
        .await
        .unwrap();
    let item = crate::services::turn_orchestrator::pending_queue_item_to_intervention(
        original,
        std::time::SystemTime::now(),
        std::time::Instant::now(),
    );
    use crate::services::turn_orchestrator::{
        load_channel_pending_dispatch_marker_checked, save_channel_pending_dispatch_marker,
    };
    save_channel_pending_dispatch_marker(
        &ProviderKind::Codex,
        &rig.shared.token_hash,
        rig.channel,
        &item,
        None,
    )
    .unwrap();
    assert!(
        load_channel_pending_dispatch_marker_checked(
            &ProviderKind::Codex,
            &rig.shared.token_hash,
            rig.channel
        )
        .unwrap()
        .is_some()
    );
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive);
    let result = tokio::time::timeout(Duration::from_secs(30), settled.recv())
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    assert_eq!(
        result,
        Err("NotSent(\"hosted_tui_busy_pre_submit\")".into()),
        "{}",
        rig.trace.text()
    );
    assert!(
        rig.trace
            .text()
            .contains("prior turn genuinely busy per JSONL"),
        "{}",
        rig.trace.text()
    );
    assert!(
        load_channel_pending_dispatch_marker_checked(
            &ProviderKind::Codex,
            &rig.shared.token_hash,
            rig.channel
        )
        .unwrap()
        .is_none(),
        "rollback must consume its marker in the bot queue namespace"
    );
    assert!(events(&rig.renderer).is_empty());
    assert_eq!(rig.state().await["items"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a1_owned_busy_rollback_keeps_front_without_completing_or_requeueing() {
    if !discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "a1_owned_busy_rollback_keeps_front_without_completing_or_requeueing"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = IntakeRig::new(7_676_101, true).await;
    let original = rig.state().await["items"].clone();
    let mut completions = turn_completion_events::subscribe_turn_completion_events(&rig.shared);
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
    let result = tokio::time::timeout(Duration::from_secs(30), settled.recv())
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    assert!(
        result.as_ref().is_err_and(|e| e.starts_with("NotSent(")),
        "{result:?}\n{}",
        rig.trace.text()
    );
    assert!(
        rig.trace
            .text()
            .contains("prior turn genuinely busy per JSONL"),
        "{}",
        rig.trace.text()
    );
    assert!(
        completions.try_recv().is_err(),
        "a rejected input is not a native completion"
    );
    assert!(
        rig.shared.restart.deferred_hook_channels.is_empty(),
        "no rollback timer is armed"
    );
    let state = rig.state().await;
    assert_eq!(state["items"], original);
    assert!(state["head_injection"].is_null());
    assert_eq!(state["retained_inputs"], json!([]));
    assert!(!rig.mailbox.has_active_turn().await.unwrap());
    assert!(
        rig.mailbox.snapshot().await.intervention_queue.is_empty(),
        "V1 never receives the V2 front"
    );
    assert!(
        !rig.mock
            .written_texts()
            .iter()
            .any(|s| s.contains("provider가 응답을 완료하지 못했어요"))
    );

    // A real turn still publishes its completion, which grants the same front another opportunity.
    let token = Arc::new(CancelToken::new());
    rig.mailbox
        .restore_active_turn(token, UserId::new(7), MessageId::new(9))
        .await;
    discord::mailbox_finish_turn(&rig.shared, &ProviderKind::Claude, rig.channel).await;
    assert!(completions.recv().await.unwrap().queue_is_eligible());
    crate::services::discord::queue_io::input_owner::drive::wake(drive);
    let retry = tokio::time::timeout(Duration::from_secs(20), settled.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        retry.is_err(),
        "a genuine completion wakes the front through real intake again"
    );
    assert!(
        completions.try_recv().is_err(),
        "the second busy refusal also has no self completion"
    );
    assert_eq!(rig.state().await["items"], original);
    assert!(
        events(&rig.renderer).is_empty(),
        "busy refusal sends no keys"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a1_owned_busy_front_never_calls_legacy_requeue() {
    if !discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "a1_owned_busy_front_never_calls_legacy_requeue"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = IntakeRig::new(7_676_103, true).await;
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive);
    let result = tokio::time::timeout(Duration::from_secs(30), settled.recv())
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    assert!(result.is_err());
    let trace = rig.trace.text();
    assert!(
        trace.contains("prior turn genuinely busy per JSONL"),
        "{trace}"
    );
    assert!(
        !trace.contains("mailbox requeue-front failed"),
        "V2 must not attempt a refused V1 write: {trace}"
    );
    assert!(rig.mailbox.snapshot().await.intervention_queue.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a1_not_sent_intake_posts_no_provider_failure() {
    if !discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "a1_not_sent_intake_posts_no_provider_failure"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = IntakeRig::new(7_676_102, false).await;
    let (completed, bridge) = tokio::sync::oneshot::channel();
    *discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((rig.channel, completed));
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive);
    let result = tokio::time::timeout(Duration::from_secs(30), settled.recv())
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    assert_eq!(
        result,
        Err("NotSent(\"idle_unobserved\")".into()),
        "{}",
        rig.trace.text()
    );
    tokio::time::timeout(Duration::from_secs(20), bridge)
        .await
        .unwrap()
        .unwrap();
    let writes = rig.mock.written_texts();
    assert!(
        !writes
            .iter()
            .any(|s| s.contains("provider가 응답을 완료하지 못했어요")),
        "{writes:?}"
    );
    assert_eq!(
        rig.state().await["items"].as_array().unwrap().len(),
        1,
        "NotSent leaves the original unattempted"
    );
    assert!(
        events(&rig.renderer).is_empty(),
        "suppressing a notice cannot replay input"
    );
    let name =
        ProviderKind::Claude.build_tmux_session_name(&format!("a1-rollback-{}", rig.channel.get()));
    assert!(
        matches!(
            crate::services::platform::tmux::pane_liveness(&name),
            crate::services::platform::tmux::PaneLiveness::Live
        ),
        "NotSent cannot tear down the pane"
    );
    for error in [
        "real provider failure",
        "Failed(\"stash_failed\")",
        "WriteUncertain(\"enter_failed\")",
        "upstream mentioned NotSent(\"x\")",
    ] {
        assert!(
            super::super::super::producer_error(None, rig.channel.get(), error.into()).is_some(),
            "{error}"
        );
    }
}
