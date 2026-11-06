//! An owned front reaches real intake; a pre-submit busy refusal is not a completed native turn.
use super::*;
use crate::services::discord::queue_io::input_owner::admission::Admission;
use crate::services::discord::router::message_handler::provider_dispatch::submission_tests::{
    creator_child, d1_execution_proof, d1_fixture_config,
};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{self as discord, SharedData, turn_completion_events};
use poise::serenity_prelude::ChannelId;

#[path = "owner_input_carried_restart_tests.rs"]
mod carried_restart;
#[path = "owner_input_owned_stop_tests.rs"]
mod owned_stop;

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
        Self::build(id, busy, bot, None, None, None, None).await
    }

    async fn busy_with_screen(id: u64, screen: String) -> Self {
        Self::build(
            id,
            true,
            ProviderKind::Claude,
            None,
            None,
            None,
            Some(screen),
        )
        .await
    }

    async fn with_storage(
        id: u64,
        busy: bool,
        bot: ProviderKind,
        pool: Option<sqlx::PgPool>,
    ) -> Self {
        Self::build(id, busy, bot, pool, None, None, None).await
    }

    /// A busy pane whose front a `turn/start` admitted with its headless provenance.
    async fn turn_start(id: u64) -> Self {
        Self::build(
            id,
            true,
            ProviderKind::Claude,
            None,
            Some(false),
            None,
            None,
        )
        .await
    }

    /// A `turn/start` naming `origin` on a stored channel whose pane's transcript its observer finds.
    async fn turn_start_stored(id: u64, pool: sqlx::PgPool, origin: &str) -> Self {
        Self::build(
            id,
            true,
            ProviderKind::Claude,
            Some(pool),
            Some(false),
            Some(origin),
            None,
        )
        .await
    }

    async fn build(
        id: u64,
        busy: bool,
        bot: ProviderKind,
        pool: Option<sqlx::PgPool>,
        start: Option<bool>,
        origin: Option<&str>,
        screen: Option<String>,
    ) -> Self {
        // A `turn/start` front is admitted when `start` is set, as a fresh routine's when it is true;
        // a Codex start runs on its own Codex pane, and when true is the routine executor's.
        let (start, fresh) = (start.is_some(), start == Some(true));
        let codex = bot == ProviderKind::Codex && start;
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
            if codex {
                let channel = format!("{{codex: {{id: '{id}', runtime: tui, provider: codex}}}}");
                yaml["agents"][0]["channels"] = serde_yaml::from_str(&channel).unwrap();
            }
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
        if codex {
            crate::services::provider_hosting::install_provider_hosting_config(&config);
        }
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
        let pane = if codex {
            bot.clone()
        } else {
            ProviderKind::Claude
        };
        let name = pane.build_tmux_session_name(&label);
        let workspace = f.root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let (renderer, sid) = match codex {
            true => (
                super::super::codex_tests::codex_renderer_in(&f, &name, true, Some(&workspace)),
                native_read::codex::execution_proof(&name, id),
            ),
            false => {
                let screen = screen.unwrap_or_else(|| view("", false));
                let renderer = renderer_with(&f, &name, "normal", screen, "", PROMPT);
                let proof = d1_execution_proof(&name, id);
                (renderer, proof.context.expected_native_session_id.unwrap())
            }
        };
        let sid = sid.as_str();
        let home = f.root.path().join("codex-home");
        if codex {
            let guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
            env.push(guard("CODEX_HOME", &home));
        }
        let transcript = match start && origin.is_none() {
            // The rollout the pane's Codex session writes, named by its id.
            _ if codex => home
                .join("sessions/2026/11/06")
                .join(format!("rollout-2026-11-06T00-00-00-{sid}.jsonl")),
            // Known only through the pane's binding, as when no idle observer can be built.
            true => f
                .root
                .path()
                .join("bound-only")
                .join(format!("{sid}.jsonl")),
            false => crate::services::claude_tui::transcript_tail::claude_transcript_path(
                &workspace,
                sid,
                Some(f.root.path()),
            )
            .unwrap(),
        };
        if codex {
            fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            fs::write(&transcript, native_read::codex::meta(sid, &workspace)).unwrap();
        } else if busy {
            fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            fs::write(&transcript, BUSY).unwrap();
        }
        let kind = match codex {
            true => crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
            false => crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        };
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            &name,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: kind,
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
        let admitted = match start {
            false => admit(&mailbox, &original, AdmissionTicket::new()).await,
            true => {
                use crate::services::discord::queue_io::input_owner::{admission, routine, start};
                let generation =
                    crate::services::discord::runtime_store::generation_path().unwrap();
                fs::create_dir_all(generation.parent().unwrap()).unwrap();
                fs::write(generation, "5").unwrap();
                let turn = format!("discord:{id}:9100095826424066539");
                let (original, ticket) = routine::synthetic_original(&turn, id, PROMPT, 1).unwrap();
                let provenance = start::HeadlessProvenance {
                    owner: (7, "imessage"),
                    source: Some("imessage"),
                    metadata: origin.map(|o| match fresh {
                        true => json!({"human_input": {"origin_id": o},
                            "routine_id": "routine-7772", "execution_strategy": "fresh"}),
                        false => json!({"human_input": {"origin_id": o}}),
                    }),
                    channel_hint: None,
                    label: None,
                    dm: None,
                };
                let context = match codex && fresh {
                    true => native_read::codex::routine_context(origin),
                    false => provenance.json(&turn),
                };
                admission::admit_with_context(&mailbox, &original, ticket, context).await
            }
        };
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

    /// Rebinds the pane to a transcript that only its binding names.
    async fn bind_only(&mut self) {
        self.transcript = self.renderer.join("bound-only.jsonl");
        let name = format!("a1-rollback-{}", self.channel.get());
        let name = ProviderKind::Claude.build_tmux_session_name(&name);
        let core = self.shared.core.lock().await;
        let session = core.sessions.get(&self.channel).unwrap().session_id.clone();
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            &name,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                output_path: self.transcript.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: session,
                last_offset: 0,
                relay_last_offset: None,
            },
        );
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
    assert_eq!(result, Ok(()), "{}", rig.trace.text());
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
    assert_eq!(events(&rig.renderer), ["paste", "Enter"]);
    assert_eq!(rig.state().await["items"], json!([]));
}

/// Real drive and intake: a pane busy with a turn AgentDesk did not start takes the owned front
/// once, settled delivered, with no completion, requeue or provider failure from the intake turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_front_on_a_busy_pane_is_pasted_once_without_completion() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "owned_front_on_a_busy_pane_is_pasted_once_without_completion"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = IntakeRig::new(7_676_101, true).await;
    let mut completions = turn_completion_events::subscribe_turn_completion_events(&rig.shared);
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
    let result = tokio::time::timeout(Duration::from_secs(30), settled.recv())
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    assert_eq!(result, Ok(()), "{}", rig.trace.text());
    assert!(
        rig.trace
            .text()
            .contains("prior turn genuinely busy per JSONL"),
        "{}",
        rig.trace.text()
    );
    assert_eq!(events(&rig.renderer), ["paste", "Enter"]);
    let pasted = fs::read_to_string(rig.renderer.join("body")).unwrap();
    assert!(pasted.ends_with(PROMPT), "{pasted:?}");
    let state = rig.state().await;
    assert_eq!(state["items"], json!([]), "{state}");
    assert!(state["head_injection"].is_null(), "{state}");
    assert_eq!(state["retained_inputs"], json!([]), "{state}");
    assert!(!rig.mailbox.has_active_turn().await.unwrap());
    assert!(
        completions.try_recv().is_err(),
        "the released intake turn is not a native completion"
    );
    assert!(rig.shared.restart.deferred_hook_channels.is_empty());
    assert!(rig.mailbox.snapshot().await.intervention_queue.is_empty());
    assert!(
        !rig.mock
            .written_texts()
            .iter()
            .any(|s| s.contains("provider가 응답을 완료하지 못했어요"))
    );
    drop(drive);
}

/// A `turn/start` behind a busy native turn waits unclaimed, with the pane's transcript known only
/// through its binding and no database: no key, no post, and its original stays first in line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_start_into_a_busy_pane_waits_unclaimed() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "a_turn_start_into_a_busy_pane_waits_unclaimed"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = IntakeRig::turn_start(7_676_106).await;
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
    let result = tokio::time::timeout(Duration::from_secs(30), settled.recv())
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    assert_eq!(
        result,
        Err("NotSent(\"synthetic_waits_idle\")".into()),
        "{}",
        rig.trace.text()
    );
    assert!(
        events(&rig.renderer).is_empty(),
        "{:?}",
        events(&rig.renderer)
    );
    let state = rig.state().await;
    assert_eq!(state["items"].as_array().unwrap().len(), 1, "{state}");
    assert!(state["head_injection"].is_null(), "unclaimed: {state}");
    assert_eq!(rig.mock.written_texts(), Vec::<String>::new());
    drop(drive);
}

/// A start held behind a running native turn is not handed while that turn runs; the turn's end
/// hands it once as a turn of its own, and the running turn's answer is never posted for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_start_held_behind_a_running_turn_runs_once_after_it_ends() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "a_turn_start_held_behind_a_running_turn_runs_once_after_it_ends"
    )) {
        return;
    }
    const ANSWER: &str = "running turn answer 7676";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = IntakeRig::turn_start(7_676_107).await;
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
    let result = next_settle(&mut settled, &rig.trace).await;
    assert_eq!(result, Err("NotSent(\"synthetic_waits_idle\")".into()));
    let mut relayed = crate::services::tui_prompt_dedupe::subscribe_observed_prompts();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        settled.try_recv().is_err(),
        "the held start was handed mid-turn"
    );
    assert!(events(&rig.renderer).is_empty());
    // The native turn answers its own prompt and ends.
    let rows = [
        json!({"type":"assistant","uuid":"native-answer","message":{"id":"native-answer",
            "role":"assistant","content":[{"type":"text","text":ANSWER}],"stop_reason":"end_turn"}}),
        json!({"type":"system","subtype":"turn_duration"}),
    ];
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&rig.transcript)
        .unwrap();
    for row in rows {
        std::io::Write::write_all(&mut file, format!("{row}\n").as_bytes()).unwrap();
    }
    let ran = next_settle(&mut settled, &rig.trace).await;
    assert_eq!(ran, Ok(()), "{}", rig.trace.text());
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(settled.try_recv().is_err(), "the start was handed twice");
    assert_eq!(events(&rig.renderer), ["paste", "Enter"]);
    let mut opened = 0;
    while let Ok(event) = relayed.try_recv() {
        opened += usize::from(event.prompt.contains(PROMPT));
    }
    assert!(opened <= 1, "the relay opened {opened} turns for the start");
    assert!(!rig.mock.written_texts().iter().any(|t| t.contains(ANSWER)));
    assert!(rig.state().await["items"].as_array().unwrap().is_empty());
    drop(drive);
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
    let settlement = tokio::time::timeout(Duration::from_secs(30), settled.recv())
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    assert_eq!(settlement, Ok(()), "{}", rig.trace.text());
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

/// The turn receipts that name `origin`, by outcome.
async fn origin_receipts(pool: &sqlx::PgPool, origin: &str) -> Vec<String> {
    let sql = "SELECT terminal_outcome FROM turns
               WHERE consumed_origin_ids @> ARRAY[$1]::TEXT[] AND terminal_outcome IS NOT NULL";
    let rows = sqlx::query_scalar::<_, String>(sql)
        .bind(origin)
        .fetch_all(pool);
    rows.await.unwrap()
}

/// A `turn/start` meeting a native busy turn on a stored tmux session waits unclaimed through a
/// failed pane read: nothing is written, retried or killed, and the turn's idle alone runs it once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_start_waits_out_a_native_busy_turn_then_runs_as_its_own_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "a_turn_start_waits_out_a_native_busy_turn_then_runs_as_its_own_pg"
    )) {
        return;
    }
    use crate::services::agents::input_result::{InputResult, OriginChannel, lookup};
    const ORIGIN: &str = "G-native-busy";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = IntakeRig::turn_start_stored(7_340_801, pool.clone(), ORIGIN).await;
    // The pane is a stored tmux session the host guard lets an automatic reset clear.
    let key = discord::adk_session::build_adk_session_key(
        &rig.shared,
        rig.channel,
        &ProviderKind::Claude,
        None,
    );
    let key = key.await.expect("channel session key");
    discord::inflight::seed_session_row_keyed(&pool, &key, rig.channel.get(), None).await;
    // What Enter submits lands in the same native transcript the observer reads.
    let native = rig.renderer.join("transcript");
    fs::write(&native, BUSY).unwrap();
    fs::remove_file(&rig.transcript).unwrap();
    fs::hard_link(&native, &rig.transcript).unwrap();
    let append = |row: Value| {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&rig.transcript)
            .unwrap();
        std::io::Write::write_all(&mut file, format!("{row}\n").as_bytes()).unwrap();
    };
    let channels = [OriginChannel {
        provider: "claude".into(),
        channel_id: rig.channel.get(),
    }];
    let answer = || lookup(&pool, "agent", ORIGIN, &channels);
    assert!(matches!(
        answer().await,
        Ok(Some(InputResult::Accepted { .. }))
    ));
    let name =
        ProviderKind::Claude.build_tmux_session_name(&format!("a1-rollback-{}", rig.channel.get()));
    let live = || {
        matches!(
            crate::services::platform::tmux::pane_liveness(&name),
            crate::services::platform::tmux::PaneLiveness::Live
        )
    };

    let (completed, bridge) = tokio::sync::oneshot::channel();
    *discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((rig.channel, completed));
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive);
    let held = tokio::time::timeout(Duration::from_secs(30), settled.recv())
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    if held.is_ok() {
        // The native turn takes a pasted input as a queued command and has not ended yet.
        let frame = fs::read_to_string(rig.renderer.join("body")).unwrap_or_default();
        append(
            json!({"type":"attachment","uuid":"absorbed-row","attachment":
            {"type":"queued_command","prompt":frame,"commandMode":"prompt"}}),
        );
        let _ = tokio::time::timeout(Duration::from_secs(60), bridge).await;
    }
    let busy = (
        held,
        events(&rig.renderer),
        live(),
        rig.trace.text().contains("killing tmux session"),
        origin_receipts(&pool, ORIGIN).await,
        // A claimed start opens a headless turn, whose placeholder is posted and removed.
        (rig.mock.written_texts().len(), rig.mock.deleted().len()),
    );
    let expected = (
        Err("NotSent(\"synthetic_waits_idle\")".to_string()),
        Vec::<String>::new(),
        true,
        false,
        Vec::<String>::new(),
        (0, 0),
    );
    assert_eq!(busy, expected, "{}", rig.trace.text());
    assert!(matches!(
        answer().await,
        Ok(Some(InputResult::Accepted { .. }))
    ));
    let state = rig.state().await;
    assert_eq!(state["items"].as_array().unwrap().len(), 1, "{state}");
    assert!(state["head_injection"].is_null(), "unclaimed: {state}");

    // One read of the pane fails while the watch runs; a later read sees it again.
    let tmux = rig._tmux.root.path();
    let (shim, inner, fail) = (
        tmux.join("tmux"),
        tmux.join("tmux-inner"),
        tmux.join("fail-read"),
    );
    fs::rename(&shim, &inner).unwrap();
    let script = format!(
        "#!/bin/sh\ncase \"$*\" in *list-panes*pane_dead*) [ ! -f '{fail}' ] || {{ rm '{fail}'; exit 1; }} ;; esac\nexec '{inner}' \"$@\"\n",
        fail = fail.display(),
        inner = inner.display()
    );
    fs::write(&shim, script).unwrap();
    fs::set_permissions(&shim, fs::metadata(&inner).unwrap().permissions()).unwrap();
    fs::write(&fail, "").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while fail.exists() {
        assert!(std::time::Instant::now() < deadline, "no pane read failed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;

    // The native turn ends; with no other wake, the input runs once as a turn of its own.
    let (completed, bridge) = tokio::sync::oneshot::channel();
    *discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((rig.channel, completed));
    append(json!({"type":"system","subtype":"turn_duration"}));
    // Stands in for the outbox worker: each queued headless body is delivered.
    let outbox = pool.clone();
    let worker = tokio::spawn(async move {
        loop {
            let sent = "UPDATE message_outbox SET status = 'sent' WHERE status = 'pending'";
            let _ = sqlx::query(sent).execute(&outbox).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    let ran = tokio::time::timeout(Duration::from_secs(30), settled.recv())
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    assert_eq!(ran, Ok(()), "{}", rig.trace.text());
    wait("the start's own user row", || {
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
    append(
        json!({"type":"assistant","uuid":"own-answer","apiBlockIndex":0,"message":
        {"id":"own-answer","role":"assistant","content":[{"type":"text","text":"own turn answer"}],
        "stop_reason":"end_turn"}}),
    );
    append(json!({"type":"system","subtype":"turn_duration"}));
    tokio::time::timeout(Duration::from_secs(90), bridge)
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    // Nothing hands the input on a second time once its own turn has ended.
    let again = tokio::time::timeout(Duration::from_secs(6), settled.recv()).await;
    worker.abort();
    let enters = events(&rig.renderer)
        .iter()
        .filter(|e| *e == "Enter")
        .count();
    let after = (
        enters,
        live(),
        rig.trace.text().contains("killing tmux session"),
        origin_receipts(&pool, ORIGIN).await,
    );
    let sql = "SELECT turn_id || ' ' || coalesce(terminal_outcome, '-') || ' ' ||
               array_to_string(consumed_origin_ids, ',') FROM turns";
    let rows: Vec<String> = sqlx::query_scalar(sql).fetch_all(&pool).await.unwrap();
    assert_eq!(
        after,
        (1, true, false, vec!["completed".to_string()]),
        "{rows:?}\n{}",
        rig.trace.text()
    );
    // The drive ended with its queue empty: no second result, only its closed channel.
    assert!(!matches!(again, Ok(Some(_))), "a second handoff: {again:?}");
    assert!(rig.state().await["items"].as_array().unwrap().is_empty());
    drop(rig);
    pool.close().await;
    db.drop().await;
}

/// A start the provider never received leaves no turn row; the clear that later removes it commits
/// one cancelled receipt at its removal instant with no session, and owes nothing after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsent_start_cleared_later_leaves_only_its_removal_receipt_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "an_unsent_start_cleared_later_leaves_only_its_removal_receipt_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (claude, origin) = (ProviderKind::Claude, Some("G-unsent"));
    let rig = IntakeRig::build(
        7_340_802,
        false,
        claude.clone(),
        Some(pool.clone()),
        Some(true),
        origin,
        None,
    );
    let mut rig = rig.await;
    // A fresh start reads the idle pane through its binding, then clears it, so no idle observer
    // can be built for the start.
    rig.bind_only().await;
    fs::write(&rig.transcript, IDLE).unwrap();
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
    tokio::time::timeout(Duration::from_secs(30), bridge)
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    let rows = "SELECT turn_id, session_id, started_at, finished_at, terminal_outcome,
                    terminal_confirmed_at FROM turns WHERE channel_id = $1";
    type Row = (
        String,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    );
    let read = || sqlx::query_as::<_, Row>(rows).bind(rig.channel.to_string());
    let analytics = read().fetch_all(&pool).await.unwrap();
    assert_eq!(analytics, [], "an unsent attempt is no turn");
    assert_eq!(rig.state().await["items"].as_array().unwrap().len(), 1);

    let persistence = discord::queue_persistence_context(&rig.shared, &claude, rig.channel);
    let cleared = rig.mailbox.clear(persistence).await;
    assert_eq!(cleared.discarded_message_ids.len(), 1);
    let drops = owed_drops(&rig.mailbox).await;
    let [removed] = drops.as_slice() else {
        panic!("one owed drop: {drops:?}");
    };
    discord::settle_v2_drops(Some(&pool), &rig.mailbox, "claude", rig.channel).await;
    let at = chrono::DateTime::from_timestamp_millis(removed.at_ms as i64).unwrap();
    let turn = format!("discord:{}:9100095826424066539", rig.channel);
    let receipt = (turn, None, at, at, Some("cancelled".into()), Some(at));
    assert_eq!(read().fetch_all(&pool).await.unwrap(), [receipt]);
    assert!(owed_drops(&rig.mailbox).await.is_empty());
    drop(rig);
    pool.close().await;
    db.drop().await;
}

async fn owed_drops(
    mailbox: &ChannelMailboxHandle,
) -> Vec<crate::services::turn_orchestrator::OwedDrop> {
    let read = mailbox.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.owed_drops()));
    read.await.unwrap()
}

async fn next_settle(
    settled: &mut tokio::sync::mpsc::UnboundedReceiver<Result<(), String>>,
    trace: &Trace,
) -> Result<(), String> {
    let recv = tokio::time::timeout(Duration::from_secs(30), settled.recv());
    recv.await
        .unwrap_or_else(|_| panic!("{}", trace.text()))
        .unwrap()
}

/// A start waiting across a restart stays queued: boot recovery hands it again, the earlier process's
/// carried row keeps it unpasted, and that row's close (archive and one kick) runs it once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_waiting_across_a_restart_runs_once_after_the_carried_row_closes_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "a_start_waiting_across_a_restart_runs_once_after_the_carried_row_closes_pg"
    )) {
        return;
    }
    use crate::services::discord::inflight;
    const ORIGIN: &str = "G-restart-carried";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = IntakeRig::turn_start_stored(7_340_803, pool.clone(), ORIGIN).await;
    let (shared, channel) = (rig.shared.clone(), rig.channel);
    let key =
        discord::adk_session::build_adk_session_key(&shared, channel, &ProviderKind::Claude, None);
    let key = key.await.expect("channel session key");
    discord::inflight::seed_session_row_keyed(&pool, &key, channel.get(), None).await;
    // The carried turn has ended in the transcript; its row from the earlier process still stands.
    let native = rig.renderer.join("transcript");
    fs::write(&native, IDLE).unwrap();
    fs::remove_file(&rig.transcript).unwrap();
    fs::hard_link(&native, &rig.transcript).unwrap();
    let name =
        ProviderKind::Claude.build_tmux_session_name(&format!("a1-rollback-{}", channel.get()));
    let mut row = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        channel.get(),
        None,
        7,
        9_100_095_826_424_000_803,
        9_100_095_826_424_000_804,
        "carried".into(),
        None,
        Some(name.clone()),
        Some(rig.transcript.display().to_string()),
        None,
        0,
    );
    row.born_generation = shared.restart.current_generation.saturating_sub(1);
    row.restart_mode = Some(discord::InflightRestartMode::DrainRestart);
    row.restart_generation = Some(shared.restart.current_generation);
    inflight::save_inflight_state(&row).unwrap();

    // The restarted process recovers the owner's queue and wakes its drive.
    discord::router::message_handler::owner_recover(&shared, channel).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let waiting = (
        events(&rig.renderer),
        origin_receipts(&pool, ORIGIN).await,
        rig.state().await["items"].as_array().unwrap().len(),
    );
    assert_eq!(waiting, (vec![], vec![], 1), "{}", rig.trace.text());

    // The close archives the row and kicks the queue once, as carried_turn_close does.
    let outbox = pool.clone();
    let worker = tokio::spawn(async move {
        loop {
            let sent = "UPDATE message_outbox SET status = 'sent' WHERE status = 'pending'";
            let _ = sqlx::query(sent).execute(&outbox).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    let (completed, bridge) = tokio::sync::oneshot::channel();
    *discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((channel, completed));
    let file = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        channel.get(),
    );
    let archive = file.parent().unwrap().join("archive");
    fs::create_dir_all(&archive).unwrap();
    fs::rename(
        &file,
        archive.join(format!("{}.json.carried", channel.get())),
    )
    .unwrap();
    discord::schedule_deferred_idle_queue_kickoff_immediate(
        shared.clone(),
        ProviderKind::Claude,
        channel,
        "carried_turn_closed",
    );
    wait("the start's own user row", || {
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
    let mut native = fs::OpenOptions::new()
        .append(true)
        .open(&rig.transcript)
        .unwrap();
    let answer = json!({"type":"assistant","uuid":"restart-answer","apiBlockIndex":0,"message":
        {"id":"restart-answer","role":"assistant","content":[{"type":"text","text":"after restart"}],
        "stop_reason":"end_turn"}});
    std::io::Write::write_all(&mut native, format!("{answer}\n").as_bytes()).unwrap();
    let end = json!({"type":"system","subtype":"turn_duration"});
    std::io::Write::write_all(&mut native, format!("{end}\n").as_bytes()).unwrap();
    tokio::time::timeout(Duration::from_secs(90), bridge)
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await;
    worker.abort();
    let enters = events(&rig.renderer)
        .iter()
        .filter(|e| *e == "Enter")
        .count();
    let ran = (
        enters,
        origin_receipts(&pool, ORIGIN).await,
        rig.state().await["items"].as_array().unwrap().len(),
    );
    assert_eq!(
        ran,
        (1, vec!["completed".to_string()], 0),
        "{}",
        rig.trace.text()
    );
    drop(rig);
    pool.close().await;
    db.drop().await;
}

/// A finished turn's row kept marked because its receipt failed: the owned front's one wake records
/// that receipt, replaces the row and submits once to the idle pane, with no other wake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_marked_row_is_settled_and_the_owned_front_submitted_on_one_wake_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::",
        "a_marked_row_is_settled_and_the_owned_front_submitted_on_one_wake_pg"
    )) {
        return;
    }
    use crate::services::discord::inflight;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig =
        IntakeRig::with_storage(7_676_108, false, ProviderKind::Claude, Some(pool.clone())).await;
    fs::create_dir_all(rig.transcript.parent().unwrap()).unwrap();
    fs::write(rig.renderer.join("transcript"), IDLE).unwrap();
    fs::hard_link(rig.renderer.join("transcript"), &rig.transcript).unwrap();
    let mut marked = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        rig.channel.get(),
        None,
        7,
        9,
        0,
        "ended".into(),
        None,
        None,
        None,
        None,
        0,
    );
    marked.turn_nonce = Some("ended-turn".into());
    marked.input_origin_ids = vec!["G-marked".into()];
    marked.terminal_receipt = Some(inflight::TerminalReceiptStamp {
        outcome: crate::db::turns::TerminalOutcome::Completed,
        confirmed_at: chrono::Utc::now(),
    });
    inflight::save_inflight_state_create_new(&marked).unwrap();
    let (completed, bridge) = tokio::sync::oneshot::channel();
    *discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((rig.channel, completed));
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
    let result = tokio::time::timeout(Duration::from_secs(30), settled.recv())
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    assert_eq!(result, Ok(()), "{}", rig.trace.text());
    let receipt: Option<(Vec<String>, String)> = sqlx::query_as(
        "SELECT consumed_origin_ids, terminal_outcome FROM turns
         WHERE turn_id = $1 AND terminal_outcome IS NOT NULL",
    )
    .bind(format!("discord:{}:9", rig.channel))
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(receipt, Some((vec!["G-marked".into()], "completed".into())));
    let row = inflight::load_inflight_state_read_only(&ProviderKind::Claude, rig.channel.get());
    assert_ne!(
        row.map(|row| row.user_msg_id),
        Some(9),
        "the marked row is gone"
    );
    assert_eq!(rig.state().await["items"], json!([]));
    use std::io::Write;
    let mut native = fs::OpenOptions::new()
        .append(true)
        .open(&rig.transcript)
        .unwrap();
    writeln!(native, "{}", json!({"type":"assistant","uuid":"marked-answer","apiBlockIndex":0,
        "message":{"id":"marked-answer","role":"assistant","content":[{"type":"text","text":"answered after the marked row"}],"stop_reason":"end_turn"}})).unwrap();
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
    let enters = events(&rig.renderer)
        .iter()
        .filter(|e| *e == "Enter")
        .count();
    assert_eq!(enters, 1, "submitted once");
    assert!(settled.try_recv().is_err(), "one wake, one pass");
    drop(drive);
    drop(rig);
    pool.close().await;
    db.drop().await;
}

#[path = "owner_input_native_read_tests.rs"]
mod native_read;
