//! An owned front reaches real intake; a pre-submit busy refusal is not a completed native turn.
use super::*;
use crate::services::discord::queue_io::input_owner::admission::Admission;
use crate::services::discord::router::message_handler::provider_dispatch::submission_tests::{
    creator_child, d1_execution_proof, d1_fixture_config,
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
    transcript: std::path::PathBuf,
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
        Self::with_storage(id, busy, bot, None).await
    }

    async fn with_storage(
        id: u64,
        busy: bool,
        bot: ProviderKind,
        pool: Option<sqlx::PgPool>,
    ) -> Self {
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
        let mut shared = discord::make_shared_data_for_tests_with_storage(pool);
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
            transcript,
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
    if !creator_child(concat!(
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
    if !creator_child(concat!(
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

    let calls = rig._api.take();
    let last_status = calls
        .iter()
        .filter_map(|call| {
            let body = call.splitn(3, ' ').nth(2)?;
            let body: Value = serde_json::from_str(body).ok()?;
            (body["channel_id"] == rig.channel.to_string())
                .then(|| body["status"].as_str().map(str::to_owned))
                .flatten()
        })
        .last();
    assert_eq!(last_status.as_deref(), Some("awaiting_user"), "{calls:?}");

    // A real turn still publishes its completion, which grants the same front another opportunity.
    let token = Arc::new(CancelToken::new());
    rig.mailbox
        .restore_active_turn(token, UserId::new(7), MessageId::new(9))
        .await;
    discord::queue_io::spawn_turn_completion_idle_queue_listener(
        rig.shared.clone(),
        ProviderKind::Claude,
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        while discord::queue_io::cancel_backstop_test_support::listener_completed_reconciles(
            rig.channel,
        ) == 0
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let attempts = rig
        .trace
        .text()
        .matches("prior turn genuinely busy per JSONL")
        .count();
    assert_eq!(
        attempts, 1,
        "listener startup must not manufacture a completion"
    );
    discord::mailbox_finish_turn(&rig.shared, &ProviderKind::Claude, rig.channel).await;
    assert!(completions.recv().await.unwrap().queue_is_eligible());
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if rig
                .trace
                .text()
                .matches("prior turn genuinely busy per JSONL")
                .count()
                > attempts
                && !rig.mailbox.has_active_turn().await.unwrap()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{}", rig.trace.text()));
    assert_eq!(
        rig.trace
            .text()
            .matches("prior turn genuinely busy per JSONL")
            .count(),
        2,
        "real completion listener re-enters actual intake once, without a manual drive wake"
    );
    drop(drive);
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
    if !creator_child(concat!(
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
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "a1_not_sent_intake_posts_no_provider_failure"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = IntakeRig::new(7_676_102, false).await;
    let (_, status) = not_sent_turn(rig).await;
    assert_eq!(status.as_deref(), Some("awaiting_user"));
}

async fn not_sent_turn(rig: IntakeRig) -> (IntakeRig, Option<String>) {
    let session_before = rig.shared.core.lock().await.sessions[&rig.channel]
        .session_id
        .clone();
    let mut completions = turn_completion_events::subscribe_turn_completion_events(&rig.shared);
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
    // Process the bridge's earlier settlement messages before inspecting the event bus.
    let _ = rig
        .shared
        .turn_finalizer
        .has_live_watcher_pending(rig.channel, rig.shared.restart.current_generation)
        .await;
    let writes = rig.mock.written_texts();
    let trace = rig.trace.text();
    let mut queue_eligible = 0;
    while let Ok(event) = completions.try_recv() {
        if event.channel_id == rig.channel && event.queue_is_eligible() {
            queue_eligible += 1;
        }
    }
    let session_after = rig.shared.core.lock().await.sessions[&rig.channel]
        .session_id
        .clone();
    let observation = (
        !writes.iter().any(|s| s.contains("⚠️")),
        !trace.contains("Empty response"),
        !trace.contains("auto-retrying with fresh session"),
        queue_eligible,
        session_after,
    );
    eprintln!("A1B_NOT_SENT_OBSERVATION {observation:?}");
    assert_eq!(
        observation,
        (true, true, true, 0, session_before),
        "{writes:?}\n{trace}"
    );
    let deleted = rig.mock.deleted();
    assert!(
        !deleted.is_empty(),
        "NotSent removes its intake placeholder"
    );
    assert!(
        !deleted.contains(&(1_479_671_302_387_076_000 + rig.channel.get())),
        "the user message is never a placeholder"
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
    let calls = rig._api.take();
    let status = calls
        .iter()
        .filter_map(|call| {
            let body: Value = serde_json::from_str(call.splitn(3, ' ').nth(2)?).ok()?;
            (body["channel_id"] == rig.channel.to_string())
                .then(|| body["status"].as_str().map(str::to_owned))
                .flatten()
        })
        .last();
    eprintln!("NOT_SENT_LAST_STATUS {status:?}");
    (rig, status)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_sent_intake_records_only_the_later_success_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "not_sent_intake_records_only_the_later_success_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig =
        IntakeRig::with_storage(7_676_105, false, ProviderKind::Claude, Some(pool.clone())).await;
    let (rig, status) = not_sent_turn(rig).await;
    let turn = format!(
        "discord:{}:{}",
        rig.channel,
        1_479_671_302_387_076_000u64 + rig.channel.get()
    );
    let receipts = sqlx::query_scalar::<_, String>(
        "SELECT terminal_outcome FROM turns WHERE turn_id=$1 AND terminal_outcome IS NOT NULL",
    )
    .bind(&turn)
    .fetch_all(&pool)
    .await
    .unwrap();
    eprintln!("NOT_SENT_TERMINAL_RECEIPTS {receipts:?}; status={status:?}");
    assert_eq!(
        (receipts, status),
        (Vec::<String>::new(), Some("awaiting_user".into()))
    );

    // The next real idle admission uses the same source message; the pane supplies its native rows.
    fs::create_dir_all(rig.transcript.parent().unwrap()).unwrap();
    fs::write(rig.renderer.join("transcript"), IDLE).unwrap();
    fs::hard_link(rig.renderer.join("transcript"), &rig.transcript).unwrap();
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
    assert_eq!(result, Ok(()), "{}", rig.trace.text());
    wait("the submitted native user row", || {
        fs::read_to_string(&rig.transcript)
            .unwrap()
            .lines()
            .any(|line| {
                serde_json::from_str::<Value>(line).is_ok_and(|row| {
                    row["type"] == "user"
                        && row["message"]["content"]
                            .as_str()
                            .is_some_and(|s| s.contains(PROMPT))
                })
            })
    });
    use std::io::Write;
    let mut native = fs::OpenOptions::new()
        .append(true)
        .open(&rig.transcript)
        .unwrap();
    writeln!(native, "{}", json!({"type":"assistant","uuid":"delivered-answer","apiBlockIndex":0,
        "message":{"id":"delivered-answer","role":"assistant","content":[{"type":"text","text":"delivered exactly once"}],"stop_reason":"end_turn"}})).unwrap();
    writeln!(
        native,
        "{}",
        json!({"type":"system","subtype":"turn_duration"})
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(30), bridge)
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    let receipts = sqlx::query_scalar::<_, String>(
        "SELECT terminal_outcome FROM turns WHERE turn_id=$1 AND terminal_outcome IS NOT NULL",
    )
    .bind(&turn)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        receipts,
        ["completed"],
        "{}\nDiscord: {:?}\nNative: {}",
        rig.trace.text(),
        rig.mock.written_texts(),
        fs::read_to_string(&rig.transcript).unwrap()
    );
    assert_eq!(
        events(&rig.renderer)
            .iter()
            .filter(|e| *e == "Enter")
            .count(),
        1
    );
    assert!(
        rig.mock
            .written_texts()
            .iter()
            .any(|s| s.contains("delivered exactly once"))
    );
    drop(rig);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_waiting_status_keeps_a_successor_active_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "terminal_waiting_status_keeps_a_successor_active_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let config = crate::config::Config::default();
    let broadcast_tx = crate::eventbus::new_broadcast();
    let state = crate::app_state::AppState {
        pg_pool: Some(pool.clone()),
        engine: crate::engine::PolicyEngine::new(&config).unwrap(),
        config: Arc::new(config),
        batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: None,
        cluster_instance_id: Some("isolated-status-test".into()),
    };
    let router = axum::Router::new()
        .route(
            "/api/dispatched-sessions/webhook",
            axum::routing::post(crate::services::dispatched_sessions::hook_session_with_query),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    discord::internal_api::init(listener.local_addr().unwrap().port(), Some(pool.clone()));
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    sqlx::query(
        "INSERT INTO sessions(session_key, provider, status, active_turn_nonce)
        VALUES ('not-sent-status', 'claude', 'turn_active', 'actor-a')",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (current, caller, expected) in [
        ("actor-a", "actor-a", "awaiting_user"),
        ("actor-b", "actor-a", "turn_active"),
        ("actor-b", "actor-b", "awaiting_user"),
    ] {
        sqlx::query("UPDATE sessions SET status='turn_active', active_turn_nonce=$1 WHERE session_key='not-sent-status'")
            .bind(current).execute(&pool).await.unwrap();
        discord::adk_session::post_adk_turn_terminal_status(
            discord::adk_session::TurnTerminalStatus {
                session_key: Some("not-sent-status"),
                name: None,
                model: None,
                status: "awaiting_user",
                provider: &ProviderKind::Claude,
                session_info: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                thread_channel_id: None,
                channel_id: None,
                agent_id: None,
                turn_nonce: Some(caller),
            },
        )
        .await;
        let row: (String, String) = sqlx::query_as(
            "SELECT status, active_turn_nonce FROM sessions WHERE session_key='not-sent-status'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row,
            (expected.into(), current.into()),
            "current={current}, caller={caller}"
        );
    }
    server.abort();
    pool.close().await;
    db.drop().await;
}
