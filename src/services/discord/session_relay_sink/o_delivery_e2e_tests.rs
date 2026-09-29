use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::task_notification_delivery as cards;
use crate::services::session_backend::StreamLineState;
use crate::services::tui_o::{cutover, shadow::ShadowProvider};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[path = "fixtures/o_writer.rs"]
mod writer;

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
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &qualified, "--nocapture"])
        .env(CHILD, "1")
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env("TMPDIR", root.path())
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "")
        .env_remove(cutover::test_override::CHILD_ENV)
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();
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
    let _delegated = cutover::test_override::force_on();
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
