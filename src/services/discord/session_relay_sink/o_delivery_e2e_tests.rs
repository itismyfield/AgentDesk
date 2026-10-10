use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::task_notification_delivery as cards;
use crate::services::session_backend::StreamLineState;
use crate::services::tui_o::{cutover, shadow::ShadowProvider};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[path = "fixtures/o_writer.rs"]
mod writer;

#[path = "o_adoption_e2e_tests.rs"]
mod adoption;

const BODY: &str = "동일한 응답 본문 — delivered by O";
const CHILD: &str = "ADK_O_DELIVERY_E2E_CHILD";

fn isolated(name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/o-delivery-e2e");
    std::fs::create_dir_all(&base).unwrap();
    let root = tempfile::tempdir_in(base).unwrap();
    let qualified = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", &qualified, "--nocapture"])
        .env(CHILD, "1")
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env("TMPDIR", root.path())
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "")
        .env_remove(cutover::test_override::CHILD_ENV)
        .env_remove("DATABASE_URL");
    if name == "retired_registered_supervisor_consumers_hold_legacy_while_o_delivers_once" {
        use std::os::unix::fs::PermissionsExt;
        let fake = root.path().join("tmux");
        let script = r#"#!/bin/sh
while [ "${1#-}" != "$1" ]; do
  [ "$1" = "-V" ] && { echo 'tmux fixture'; exit 0; }
  shift
done
printf '%s\n' "$*" >> "ROOT/tmux-calls"
case "$1" in
  has-session) exit 0 ;;
  list-panes) echo 0; exit 0 ;;
  capture-pane) printf '%s\n' '⏺ Running a fixture command'; exit 0 ;;
  kill-session) printf '%s\n' "$*" >> "ROOT/tmux-kills"; exit 97 ;;
  *) printf '%s\n' "$*" >> "ROOT/tmux-errors"; exit 97 ;;
esac
"#;
        std::fs::write(
            &fake,
            script.replace("ROOT", &root.path().display().to_string()),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        child.env("PATH", format!("{}:/usr/bin:/bin", root.path().display()));
    }
    let output = child.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    false
}

#[derive(Clone, Copy)]
enum Turn {
    Claude,
    Codex,
    Task,
}

fn transcript(turn: Turn) -> String {
    let assistant = serde_json::json!({"type":"assistant", "uuid":"row-answer", "apiBlockIndex":0,
        "message":{"id":"answer", "content":[{"type":"text", "text":BODY}]}});
    let rows = match turn {
        Turn::Claude => vec![
            assistant,
            serde_json::json!({"type":"result", "result":BODY}),
        ],
        Turn::Codex => vec![
            serde_json::json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "id":"answer", "content":[{"type":"output_text", "text":BODY}]}}),
            serde_json::json!({"type":"event_msg", "payload":{"type":"task_complete", "last_agent_message":BODY}}),
        ],
        Turn::Task => vec![
            task_note(),
            assistant,
            serde_json::json!({"type":"result", "result":BODY}),
        ],
    };
    rows.iter().map(|row| format!("{row}\n")).collect()
}

fn task_note() -> serde_json::Value {
    serde_json::json!({"type":"system", "subtype":"task_notification", "task_id":"e2e-task",
        "tool_use_id":"e2e-tool", "status":"completed", "summary":"background work", "task_notification_kind":"background"})
}

struct CardTransport;
impl cards::TaskCardTransport for CardTransport {
    async fn post_card(
        &self,
        _: &cards::CardBot,
        _: u64,
        _: &str,
        _: &str,
    ) -> Result<u64, cards::TaskCardTransportError> {
        Ok(90001)
    }
    async fn edit_card(
        &self,
        _: &cards::CardBot,
        _: u64,
        _: u64,
        _: &str,
    ) -> Result<(), cards::TaskCardTransportError> {
        panic!("a confirmed task card needs no edit")
    }
}

fn snapshot(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    if path.exists() {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                result.extend(snapshot(&path));
            } else {
                result.insert(path.clone(), std::fs::read(path).unwrap());
            }
        }
    }
    result
}

async fn run(turn: Turn) {
    let root = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    let runtime = root.join("runtime");
    let channel_id = 640010;
    let codex = matches!(turn, Turn::Codex);
    let (provider, shadow, kind) = if codex {
        (
            ProviderKind::Codex,
            ShadowProvider::Codex,
            RuntimeHandoffKind::CodexTui,
        )
    } else {
        (
            ProviderKind::Claude,
            ShadowProvider::Claude,
            RuntimeHandoffKind::ClaudeTui,
        )
    };
    let binding = if codex {
        super::super::tests::matched_codex(&channel_id.to_string())
    } else {
        matched(&channel_id.to_string())
    };
    let source = PathBuf::from(&binding.expected_rollout_path);
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "").unwrap();
    let session = &binding.expected_session_name;
    let generation_path = crate::services::tmux_common::session_temp_path(session, "generation");
    std::fs::create_dir_all(Path::new(&generation_path).parent().unwrap()).unwrap();
    std::fs::write(&generation_path, b"e2e-generation").unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        session,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: kind,
            output_path: binding.expected_rollout_path.clone(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("e2e".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let started = "2026-09-29T00:00:00Z";
    let mut row = inflight_with_identity_offset(channel_id, session, 710, started, Some(0));
    row.provider = provider.as_str().to_owned();
    row.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    row.current_msg_id = 88010;
    crate::services::discord::inflight::save_inflight_state(&row).unwrap();
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
    let registry = Arc::new(HealthRegistry::new());
    registry
        .register(provider.as_str().into(), shared.clone())
        .await;
    if matches!(turn, Turn::Task) {
        let context =
            cards::TaskNotificationContext::from_stream_json(&task_note(), &StreamLineState::new())
                .unwrap();
        let clients = cards::CardDeliveryClients::new([cards::CardBot::new(
            cards::provider_bot_key(provider.as_str()),
            shared.serenity_http_or_token_fallback().unwrap(),
        )]);
        let event = context.to_event(channel_id, provider.as_str(), session);
        let card = cards::ensure_card_with_shared(
            &shared,
            &clients,
            &CardTransport,
            &event,
            cards::EnsureIntent::Promotion,
        )
        .await
        .unwrap();
        assert_eq!(card.message_id, 90001);
    }
    let gateway = Arc::new(RelayContractFakeGateway::edited());
    let mut sink = SessionBoundDiscordRelaySink::new(registry);
    sink.test_gateway = Some(gateway.clone());
    let writer = writer::WriterFixture::new(&root, &source, shadow, channel_id, session);
    let payload = transcript(turn);
    std::fs::write(&source, &payload).unwrap();
    let end = payload.len() as u64;
    let mut frame = terminal_frame_offset(&binding, &payload, 1, end, 710, started, Some(0));
    frame.relay_generation_mtime_ns = Some(dr::current_generation_mtime_ns(session));
    if codex {
        use crate::services::cluster::stream_relay::SourceFileIdentity;
        use crate::services::discord::delivery_lease_cell::source_epoch_observer;
        let file = std::fs::File::open(&source).unwrap();
        let witness = crate::services::discord::tmux::tmux_output_stream::watcher_source_witness(
            &provider,
            session,
            source.to_str().unwrap(),
        )
        .unwrap();
        frame.relay_source_stamp = Some(
            source_epoch_observer::source_stamp(
                session,
                witness,
                SourceFileIdentity::from_open_file(&file),
            )
            .unwrap(),
        );
    }
    let paths = [
        runtime.join("discord_pending_queue"),
        runtime.join("last_message"),
        crate::services::discord::settings::channel_upload_dir(ChannelId::new(channel_id)).unwrap(),
    ];
    let queue = paths[0]
        .join(provider.as_str())
        .join(&shared.token_hash)
        .join(format!("{channel_id}.json"));
    let checkpoint = paths[1]
        .join(provider.as_str())
        .join(format!("{channel_id}.txt"));
    let upload = paths[2].join("retained.txt");
    for (path, bytes) in [
        (&queue, b"[]".as_slice()),
        (&checkpoint, b"700".as_slice()),
        (&upload, b"retained attachment".as_slice()),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let before = paths.each_ref().map(|p| snapshot(p));
    let _delegated = cutover::test_override::force_channels(&[(channel_id, kind)]);
    for _ in 0..2 {
        assert_eq!(
            sink.deliver(&frame).await.unwrap(),
            RelaySinkOutcome::TerminalDelivered
        );
    }
    assert_eq!(
        (
            gateway.send_calls.load(Ordering::Acquire),
            gateway.replace_calls.load(Ordering::Acquire)
        ),
        (0, 0),
        "Legacy must consume without writing the body"
    );
    assert_eq!(
        dr::effective_committed_offset(
            &shared,
            &provider,
            ChannelId::new(channel_id),
            session,
            Some(end)
        ),
        end
    );
    assert!(
        dr::read_record(&provider, channel_id)
            .and_then(|record| record.delivered_frontier)
            .is_none(),
        "consumption is not delivery evidence"
    );
    assert_eq!(paths.each_ref().map(|p| snapshot(p)), before);
    let (stop, actor) = writer.start();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(
        writer.posts().is_empty(),
        "the gateway has not been acquired"
    );
    assert_eq!(
        writer.channel().cursors().next().unwrap().captured_through,
        end
    );
    let mut stored = writer.channel();
    let source_id = stored.cursors().next().unwrap().source.clone();
    let mut captured = Vec::new();
    stored
        .for_each_frame(&source_id, |frame| match frame {
            crate::services::tui_o::store::spool::SpoolFrame::Record(record) => {
                captured.extend(record.line);
                captured.push(b'\n');
            }
            _ => panic!("complete fixture records cannot be skipped"),
        })
        .unwrap();
    assert_eq!(
        captured,
        payload.as_bytes(),
        "O durably captured its own source"
    );
    writer.acquired();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    stop.send(true).unwrap();
    actor.await.unwrap();
    assert_eq!(
        writer.posts(),
        [BODY],
        "O must actually deliver the consumed body exactly once"
    );
    assert_eq!(paths.each_ref().map(|p| snapshot(p)), before);
}

#[tokio::test(start_paused = true)]
async fn claude_consumed_body_reaches_o_transport_once() {
    if isolated("claude_consumed_body_reaches_o_transport_once") {
        run(Turn::Claude).await;
    }
}

#[tokio::test(start_paused = true)]
async fn codex_consumed_body_reaches_o_transport_once() {
    if isolated("codex_consumed_body_reaches_o_transport_once") {
        run(Turn::Codex).await;
    }
}

#[tokio::test(start_paused = true)]
async fn task_consumed_body_reaches_o_transport_once() {
    if isolated("task_consumed_body_reaches_o_transport_once") {
        run(Turn::Task).await;
    }
}

/// One Claude channel's empty transcript, relay binding, open turn and Legacy sink, with a body
/// no other channel carries.
struct Leg {
    channel: u64,
    binding: crate::services::cluster::session_matcher::MatchedChannel,
    body: String,
    gateway: Arc<RelayContractFakeGateway>,
    sink: SessionBoundDiscordRelaySink,
}

const STARTED: &str = "2026-09-30T00:00:00Z";

impl Leg {
    fn new(channel: u64, registry: &Arc<HealthRegistry>) -> Self {
        let binding = matched(&channel.to_string());
        let source = Path::new(&binding.expected_rollout_path);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(source, "").unwrap();
        let session = &binding.expected_session_name;
        let generation = crate::services::tmux_common::session_temp_path(session, "generation");
        std::fs::create_dir_all(Path::new(&generation).parent().unwrap()).unwrap();
        std::fs::write(&generation, b"e2e-generation").unwrap();
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            session,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: binding.expected_rollout_path.clone(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("e2e".into()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let mut row = inflight_with_identity_offset(channel, session, 710, STARTED, Some(0));
        row.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
        row.current_msg_id = 88010;
        crate::services::discord::inflight::save_inflight_state(&row).unwrap();
        let gateway = Arc::new(RelayContractFakeGateway::edited());
        let mut sink = SessionBoundDiscordRelaySink::new(Arc::clone(registry));
        sink.test_gateway = Some(gateway.clone());
        let body = format!("raw unit of channel {channel}");
        Self {
            channel,
            binding,
            body,
            gateway,
            sink,
        }
    }

    fn source(&self) -> crate::services::tui_o::shadow::SourceId {
        let path = Path::new(&self.binding.expected_rollout_path);
        crate::services::tui_o::shadow::binding_reader::source_id_for("e2e", path).unwrap()
    }

    /// Writes the turn's one assistant unit and hands the terminal frame to the Legacy sink.
    async fn finish_turn(&self) -> Result<RelaySinkOutcome, RelaySinkError> {
        let text = &self.body;
        let rows = [
            serde_json::json!({"type":"assistant", "uuid":"row-answer", "apiBlockIndex":0,
                "message":{"id":"answer", "content":[{"type":"text", "text":text}]}}),
            serde_json::json!({"type":"result", "result":text}),
        ];
        let payload: String = rows.iter().map(|row| format!("{row}\n")).collect();
        std::fs::write(&self.binding.expected_rollout_path, &payload).unwrap();
        let end = payload.len() as u64;
        let binding = &self.binding;
        let mut frame = terminal_frame_offset(binding, &payload, 1, end, 710, STARTED, Some(0));
        let session = &binding.expected_session_name;
        frame.relay_generation_mtime_ns = Some(dr::current_generation_mtime_ns(session));
        self.sink.deliver(&frame).await
    }

    /// Body-carrying Legacy transport calls: a new message or an edited placeholder.
    fn legacy_posts(&self) -> u64 {
        let sent = self.gateway.send_calls.load(Ordering::Acquire);
        sent + self.gateway.replace_calls.load(Ordering::Acquire)
    }
}

/// Two Claude channels, one turn each, with `selected` as the writer list and the writer on:
/// each unit must reach Discord exactly once, through O only on a selected channel.
async fn run_canary_pair(selected: &[u64]) {
    use crate::services::tui_o::ownership::OwnershipGate;
    use crate::services::tui_o::writer::host::{self, HostParts, Readiness, test_io::TestHost};
    let root = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
    let registry = Arc::new(HealthRegistry::new());
    registry.register("claude".into(), shared.clone()).await;
    let legs = [Leg::new(640020, &registry), Leg::new(640021, &registry)];
    let owned: Vec<_> = selected
        .iter()
        .map(|&c| (c, RuntimeHandoffKind::ClaudeTui))
        .collect();
    // A canary list is injected; the empty case relies on what the boot install left.
    let _selected =
        (!selected.is_empty()).then(|| cutover::test_override::force_candidates(&owned));
    let io = TestHost::new(legs.iter().map(|leg| (leg.channel, leg.source())));
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let parts = || HostParts {
        io: Arc::clone(&io),
        runtime_root: Some(root.clone()),
        gate: Arc::clone(&gate),
        readiness: Arc::new(Readiness::default()),
    };
    let hosts = host::start(ShadowProvider::Claude, true, parts);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    for leg in &legs {
        let outcome = leg.finish_turn().await;
        assert!(
            matches!(outcome, Ok(RelaySinkOutcome::TerminalDelivered)),
            "{} body must be delivered, not held: {outcome:?}",
            leg.channel
        );
    }
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    for leg in &legs {
        // The oracle is the raw list, not the ownership helper under test.
        let via_o = u64::from(selected.contains(&leg.channel));
        let o_posts = io.posts.to(leg.channel);
        let o_units = o_posts
            .iter()
            .filter(|post| post.contains(&leg.body))
            .count() as u64;
        assert_eq!(o_posts.len() as u64, o_units, "{o_posts:?}");
        let counts = (o_units, leg.legacy_posts());
        assert_eq!(
            counts,
            (via_o, 1 - via_o),
            "O/Legacy posts of {}",
            leg.channel
        );
    }
    assert_eq!(*io.alarms.0.lock().unwrap(), []);
    hosts.iter().for_each(tokio::task::JoinHandle::abort);
}

#[tokio::test(start_paused = true)]
async fn a_canary_channel_posts_only_through_o_and_its_legacy_neighbour_only_through_legacy() {
    if isolated(
        "a_canary_channel_posts_only_through_o_and_its_legacy_neighbour_only_through_legacy",
    ) {
        run_canary_pair(&[640020]).await;
    }
}

// The empty list reaches the sinks through the same boot install the server entries run.
#[tokio::test(start_paused = true)]
async fn an_empty_writer_list_leaves_both_channels_to_legacy() {
    if isolated("an_empty_writer_list_leaves_both_channels_to_legacy") {
        let channel = |id: u64| {
            serde_json::json!({"id": format!("legacy-{id}"), "name": "Legacy",
            "channels": {"claude": {"id": id.to_string(), "runtime": "tui"}}})
        };
        let config = serde_json::from_value(serde_json::json!({"server": {},
            "agents": [channel(640020), channel(640021)], "tui_o": {"writer": {"channels": []}}}));
        crate::bootstrap::install_boot_snapshots(&config.unwrap()).unwrap();
        run_canary_pair(&[]).await;
    }
}

async fn next_registered_idle_tick() -> BTreeMap<String, (u64, u8)> {
    let ticks = crate::services::discord::session_relay_sink::tests::DC1_TICKS
        .get()
        .unwrap();
    let before = ticks.lock().unwrap().len();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        {
            let samples = ticks.lock().unwrap();
            if samples.len() > before {
                return samples.last().unwrap().clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "registered idle task must finish an actual scanner tick"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_registered_supervisor_consumers_hold_legacy_while_o_delivers_once() {
    if !isolated("retired_registered_supervisor_consumers_hold_legacy_while_o_delivers_once") {
        return;
    }
    use crate::services::cluster::relay_producer_registry::global_relay_producer_registry;
    use crate::services::cluster::session_registry::global_session_registry;
    use crate::services::cluster::stream_relay::{SourceFileIdentity, spawn_stream_relay};
    use crate::services::discord::health::legacy_supervision::{RetiredForTest, test_support};
    use crate::services::discord::inflight::o_seed_observation;

    let root = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    let provider = ProviderKind::Claude;
    let channel = 63_254_080;
    let mut fixture =
        crate::services::discord::tmux::InstallFixture::new(provider.clone(), channel).await;
    let mock = test_support::MockDiscord::start().await;
    fixture.http = mock.http.clone();
    let shared = fixture.shared.clone();
    let session = fixture.session.clone();
    let source = PathBuf::from(&fixture.output);
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&session, "generation"),
        "registered-consumer-generation",
    )
    .unwrap();
    crate::services::tmux_common::host_marker::record_tmux_host_marker(&session);
    let binding = crate::services::cluster::session_matcher::MatchedChannel {
        channel_id: channel.to_string(),
        agent_id: "n4d-supervisor".into(),
        provider: provider.clone(),
        expected_session_name: session.clone(),
        expected_rollout_path: fixture.output.clone(),
    };
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        &session,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: fixture.output.clone(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("e2e".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let writer =
        writer::WriterFixture::new(&root, &source, ShadowProvider::Claude, channel, &session);
    let mut row = inflight_with_identity_offset(channel, &session, 710, STARTED, Some(0));
    row.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    row.current_msg_id = 88010;
    row.output_path = Some(fixture.output.clone());
    test_support::seed_backfill_row(&row);
    let inflight_root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = test_support::tree_fingerprint(&inflight_root);
    let frontier_before = shared.committed_relay_offset(ChannelId::new(channel));
    let retired = RetiredForTest::new(provider.as_str(), channel);
    let owned = cutover::test_override::force_channels(&[(channel, RuntimeHandoffKind::ClaudeTui)]);
    let gateway = Arc::new(RelayContractFakeGateway::edited());
    let mut sink = SessionBoundDiscordRelaySink::new(fixture.registry.clone());
    sink.test_gateway = Some(gateway.clone());
    let sink = Arc::new(sink);
    let observations = o_seed_observation::Guard::new(&provider, channel);
    observations.register_source(&source);
    let relay = spawn_stream_relay(
        binding.clone(),
        Arc::new(LegacySessionRelayConsumer(sink.clone())),
    );
    let producers = global_relay_producer_registry();
    producers.register(session.clone(), relay.producer());
    let sessions = global_session_registry();
    sessions.upsert(binding.clone(), None);
    crate::services::discord::session_relay_sink::tests::DC1_TICKS
        .get_or_init(|| Mutex::new(Vec::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let idle = tokio::spawn(run_idle_jsonl_relay_loop(shutdown.clone(), sink.clone()));
    let (stop, actor) = writer.start();
    let first_idle_tick = next_registered_idle_tick().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let initial_captured = writer.channel().cursors().next().unwrap().captured_through;

    assert_eq!(
        fixture.install().await,
        crate::services::discord::OOnlyInstallOutcome::Spawned
    );
    for _ in 0..800 {
        if observations.snapshot().event_count("outer_eof") > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        observations.snapshot().event_count("outer_eof") > 0,
        "installer watcher must observe initial EOF"
    );
    assert!(
        observations.snapshot().event_count("rowless_host_probe") > 0,
        "confirmed tmux evidence must reach the real rowless host probe"
    );
    let payload = transcript(Turn::Claude);
    let nonterminal = format!("{}\n", payload.lines().next().unwrap());
    std::fs::write(&source, &nonterminal).unwrap();
    for _ in 0..800 {
        let metrics = relay.metrics().snapshot();
        let raw = observations.raw_process_snapshot();
        if observations
            .snapshot()
            .decoded_chunks
            .iter()
            .any(|chunk| chunk.contains(BODY))
            && (metrics.sink_errors > 0
                || metrics.frames_delivered > 0
                || raw.writable_inflight_load_calls > 0
                || raw.readonly_inflight_load_calls > 0)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    fixture.cancel_and_join().await;
    // Closing source bytes reach the registered Legacy consumer only after the watcher joins.
    // The O actor independently reads the same complete source and owns its delivery.
    let mut append = std::fs::OpenOptions::new().append(true).open(&source).unwrap();
    std::io::Write::write_all(&mut append, payload[nonterminal.len()..].as_bytes()).unwrap();
    let mut source_file = std::fs::File::open(&source).unwrap();
    let file_identity = SourceFileIdentity::from_open_file(&source_file);
    let mut forwarded_source = String::new();
    std::io::Read::read_to_string(&mut source_file, &mut forwarded_source).unwrap();
    let witness = source_epoch_observer::read_source_epoch_witness(&session);
    let stamp = source_epoch_observer::source_stamp(&session, witness, file_identity).unwrap();
    let closing_outcome = producers.get_producer(&session).unwrap().try_send_frame_with_source(
        forwarded_source[nonterminal.len()..].to_owned(),
        None,
        dr::current_generation_mtime_ns(&session),
        Some(stamp),
        Some((nonterminal.len() as u64, forwarded_source.len() as u64)),
    );
    writer.acquired();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let second_idle_tick = next_registered_idle_tick().await;

    stop.send(true).unwrap();
    actor.await.unwrap();
    shutdown.store(true, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(2), idle)
        .await
        .unwrap()
        .unwrap();
    sessions.remove(&session);
    producers.deregister(&session);
    let metrics = relay.metrics().clone();
    relay.shutdown_with_result().await.unwrap();
    let raw = observations.raw_process_snapshot();
    assert_eq!(
        (
            raw.writable_inflight_load_calls,
            raw.readonly_inflight_load_calls,
            raw.compatibility_backfill_attempts,
            raw.legacy_prefix_restore_calls
        ),
        (0, 0, 0, 0),
        "all process loader calls remain observable: {raw:?}",
    );
    let checkpointed = observations.snapshot();
    assert!(
        checkpointed
            .decoded_chunks
            .iter()
            .any(|chunk| chunk.contains(BODY)),
        "real installer watcher must decode the new body: {checkpointed:?}"
    );
    assert!(
        checkpointed.parser_responses.iter().any(|body| body == BODY),
        "real provider parser checkpoint must retain BODY through watcher join: {checkpointed:?}"
    );
    assert_eq!(forwarded_source, payload);
    assert!(closing_outcome.is_alive() && closing_outcome.sequence.is_some());
    let metrics = metrics.snapshot();
    assert!(
        metrics.frames_received >= 2 && metrics.sink_errors >= 2,
        "watcher and closing source must reach the registered consumer: {metrics:?}"
    );
    assert_eq!(metrics.frames_delivered, 0);
    assert_eq!(
        metrics.terminal_commits, 0,
        "the Legacy consumer cannot confirm an O delivery"
    );
    assert!(
        !first_idle_tick.contains_key(&session) && !second_idle_tick.contains_key(&session),
        "retired reader must not adopt a Legacy cursor"
    );
    assert_eq!(initial_captured, 0);
    assert_eq!(
        writer.channel().cursors().next().unwrap().captured_through,
        payload.len() as u64
    );
    assert_eq!(
        writer.posts(),
        [BODY],
        "the actual O decoder/writer must deliver one unit"
    );
    assert_eq!(observations.snapshot().writable_inflight_load_calls, 0);
    assert_eq!(
        shared.committed_relay_offset(ChannelId::new(channel)),
        frontier_before
    );
    assert_eq!(test_support::tree_fingerprint(&inflight_root), before);
    assert_eq!(gateway.send_calls.load(Ordering::Acquire), 0);
    assert_eq!(gateway.replace_calls.load(Ordering::Acquire), 0);
    assert!(
        !root.join("tmux-kills").exists(),
        "the watcher must not kill a fixture session"
    );
    assert!(
        !root.join("tmux-errors").exists(),
        "all fake host probes must match the fixture contract"
    );
    drop((owned, retired));
}

#[tokio::test(start_paused = true)]
async fn unretired_supervisor_consumer_preserves_legacy_decode_and_backfill() {
    if !isolated("unretired_supervisor_consumer_preserves_legacy_decode_and_backfill") {
        return;
    }
    use crate::services::discord::health::legacy_supervision::test_support;
    use crate::services::discord::inflight::o_seed_observation;
    let channel = 63_254_081;
    let provider = ProviderKind::Claude;
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
    let health = Arc::new(HealthRegistry::new());
    health.register(provider.as_str().into(), shared).await;
    let leg = Leg::new(channel, &health);
    let mut row = inflight_with_identity_offset(
        channel,
        &leg.binding.expected_session_name,
        710,
        STARTED,
        Some(0),
    );
    row.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    row.current_msg_id = 88010;
    let row_path = test_support::seed_backfill_row(&row);
    let before = test_support::fingerprint(&row_path);
    let payload = transcript(Turn::Claude);
    std::fs::write(&leg.binding.expected_rollout_path, &payload).unwrap();
    let frame = terminal_frame_offset(
        &leg.binding,
        &payload,
        1,
        payload.len() as u64,
        710,
        STARTED,
        Some(0),
    );
    let sink = Arc::new(leg.sink);
    let observations = o_seed_observation::Guard::new(&provider, channel);
    let outcome = LegacySessionRelayConsumer(sink.clone())
        .deliver(&frame)
        .await;
    assert!(
        matches!(outcome, Ok(RelaySinkOutcome::TerminalDelivered)),
        "{outcome:?}"
    );
    let raw = observations.raw_process_snapshot();
    assert!(
        raw.writable_inflight_load_calls > 0,
        "the unretired consumer retains its row path"
    );
    assert!(
        raw.compatibility_backfill_attempts > 0,
        "valid old rows still backfill"
    );
    assert_ne!(test_support::fingerprint(&row_path), before);
    assert_eq!(
        sink.frames_total.load(Ordering::Acquire),
        1,
        "actual parser received the frame"
    );
    assert_eq!(
        leg.gateway.send_calls.load(Ordering::Acquire)
            + leg.gateway.replace_calls.load(Ordering::Acquire),
        1
    );
}
