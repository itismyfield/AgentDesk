//! Native task-notification output through capture, O delivery and the live watcher loop.

use super::*;
use crate::services::discord::outbound::o_delivery_evidence::tests::Alarms;
use crate::services::discord::outbound::o_writer_io::{ChannelLeases, GatewayPort};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{
    CaptureOutcome, CaptureSource, ShadowProvider, binding_reader::source_id_for,
    capture::SourceCapture,
};
use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
use crate::services::tui_o::writer::{
    deliver::{ChannelWriter, Step},
    pieces::UnitDeriver,
};
use crate::test_support::wait::{WAIT_CAP, await_within};

const BODY: &str = "The background task completed; here is its final result.";

fn notification_turn() -> String {
    let prompt = serde_json::json!({
        "type":"user", "uuid":"task-completed", "sessionId":"native-session", "isMeta":true,
        "message":{"role":"user", "content":[{"type":"text", "text":
            "<task-notification>\n<task-id>background-1</task-id>\n<status>completed</status>\n<summary>Background task finished</summary>\n</task-notification>"}]}
    });
    let answer = serde_json::json!({
        "type":"assistant", "uuid":"task-answer", "apiBlockIndex":0,
        "message":{"id":"task-answer-message", "role":"assistant",
            "content":[{"type":"text", "text":BODY}]}
    });
    format!("{prompt}\n{answer}\n{}", stop())
}

async fn publish(h: &Harness, start: u64, replay: bool) {
    let root = crate::config::runtime_root().unwrap();
    let source = source_id_for("native-session", std::path::Path::new(&h.path)).unwrap();
    let mut capture = SourceCapture::open(source.clone(), start).unwrap();
    let initialized = Initialized {
        channel: h.channel.get(),
        sources: vec![InitSource {
            source_id: source,
            delivery_start: start,
            prefix_hash: capture.prefix_hash(),
        }],
        initial_anchor: 0,
        build_digest: "self-started fixture".into(),
        at: chrono::Utc::now(),
    };
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root)
        .unwrap()
        .unwrap();
    store.init_channel(&initialized).unwrap();
    store
        .begin_era(&[h.channel.get()], chrono::Utc::now(), |_| {
            Ok(initialized.clone())
        })
        .unwrap();
    let era = store.read_era().unwrap().unwrap();
    let mut channel = store.open_channel(&era, h.channel.get()).unwrap().unwrap();
    let mut deriver = UnitDeriver::new(h.channel.get(), ShadowProvider::Claude);
    let mut work = Vec::new();
    while capture.captured_through() < h.len() {
        let CaptureOutcome::Batch(batch) = capture.poll(1024 * 1024) else {
            panic!("native transcript must be captured");
        };
        channel
            .append_spool(&batch, &capture.prefix_hash())
            .unwrap();
        work.extend(
            batch
                .records
                .iter()
                .flat_map(|record| deriver.derive(record)),
        );
    }
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let port = Arc::new(GatewayPort::new(h.http.clone(), 1));
    let mut writer = ChannelWriter::new(
        channel,
        gate,
        port,
        ChannelLeases::from_shared(h.shared.clone()),
        Alarms,
    );
    for _ in 0..if replay { 2 } else { 1 } {
        for piece in &work {
            assert_eq!(
                await_within(writer.deliver(piece), WAIT_CAP).await,
                Step::Done
            );
        }
    }
    assert_eq!(writer.store().ledger().violation(), None);
}

fn assert_one_body(h: &Harness) {
    assert_eq!(
        h.observe(&[BODY]).copies[0].len(),
        1,
        "one visible final body"
    );
    assert_eq!(
        h.discord
            .lock()
            .unwrap()
            .shown
            .iter()
            .filter(|text| text.contains(BODY))
            .count(),
        1,
        "one body transport across O, watcher and replay"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_notification_without_inflight_is_published_once() {
    if !isolated_in(
        "self_started_publish_tests",
        "task_notification_without_inflight_is_published_once",
        &[(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[[6284281,\"claude_tui\"]]",
        )],
    ) {
        return;
    }
    let mut h = Harness::new(181, "").await;
    let _bound =
        crate::services::tui_o::cutover::test_override::bind_claude_tui_session(&h.tmux, &h.path);
    h.append(notification_turn().as_bytes());
    assert!(h.row().is_none(), "AgentDesk never started this turn");
    publish(&h, 0, false).await;
    h.spawn(0);
    await_within(h.drained("rowless task notification"), WAIT_CAP).await;
    assert_one_body(&h);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_notification_overlapping_agentdesk_turn_and_replay_has_no_duplicate() {
    if !isolated_in(
        "self_started_publish_tests",
        "task_notification_overlapping_agentdesk_turn_and_replay_has_no_duplicate",
        &[(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[[6284282,\"claude_tui\"]]",
        )],
    ) {
        return;
    }
    let seed = format!(
        "{}{}{}",
        user("AgentDesk input"),
        said("Earlier answer"),
        stop()
    );
    let mut h = Harness::new(182, &seed).await;
    let start = h.len();
    h.commit(0, start);
    let mut row = h.row_at(start);
    row.user_msg_id = 91;
    row.turn_source = crate::services::discord::inflight::TurnSource::Managed;
    h.save(&row);
    let _bound =
        crate::services::tui_o::cutover::test_override::bind_claude_tui_session(&h.tmux, &h.path);
    h.spawn(start);
    h.append(notification_turn().as_bytes());
    tokio::join!(
        publish(&h, start, true),
        await_within(h.drained("overlapping task notification"), WAIT_CAP),
    );
    assert_one_body(&h);
}
