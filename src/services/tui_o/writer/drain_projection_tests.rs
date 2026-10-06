//! What a Herdr-configured channel's actor publishes that it still owes, and the POSTs its writers
//! count as running, both as a drain of the channel's home reads them.

use std::time::Duration;

use super::super::super::actor::{
    Demand, Owing, Undelivered, spawn_projecting, undelivered_reads_for_test,
};
use super::super::super::deliver::{POST_TIMEOUT, posts_in_flight};
use super::*;
use crate::services::tui_o::store::rotation::BOUNDARY_FILE;

fn herdr_configured() -> crate::config::session_hosts::ForcedSessionHosts {
    crate::config::session_hosts::force_for_test(Some("mac-mini"), &[(CHANNEL, "mac-mini")])
}

/// What a poll publishes for the single read each test makes, stamped with that read's number.
fn owes(owed: usize, prepared: usize, uncaptured: usize, binding_pending: usize) -> Undelivered {
    Undelivered {
        owed,
        prepared,
        unsealed: 0,
        uncaptured,
        binding_pending,
        asked: 1,
    }
}

/// A piece left `Prepared` by a crash, with no result recorded.
fn crashed_mid_post(harness: &Harness) {
    let mut channel = harness.channel();
    let anchor_id = channel.ledger().anchor();
    let prepared = LedgerEntry::Prepared {
        serial: 0,
        unit_key: unit("c0"),
        piece_index: 0,
        payload: "maybe sent".into(),
        anchor_id,
        epoch: 1,
    };
    channel.append_ledger(prepared).unwrap();
}

/// Runs the actor publishing what it owes while its demand is wanted; the stop sender ends it.
fn spawn_owing(
    writer: Writer,
    bindings: Arc<FakeBindings>,
    provider: ShadowProvider,
) -> (
    watch::Sender<bool>,
    Actor,
    watch::Receiver<Option<Undelivered>>,
    Demand,
) {
    let (stop, stopped) = watch::channel(false);
    let owing = Owing::default();
    let (owed, demand) = (owing.published.subscribe(), owing.demand.clone());
    let watches = (
        stopped,
        watch::channel(false).0,
        watch::channel(None).0,
        owing,
    );
    let config = WriterConfig { enabled: true };
    let task = spawn_projecting(&config, writer, provider, bindings, watches);
    (stop, task.expect("enabled"), owed, demand)
}

// Each kind of responsibility the drain waits on shows in the published projection while it
// lasts and clears once delivered; an unreadable store publishes no answer rather than zero.
#[tokio::test(start_paused = true)]
async fn the_actor_publishes_each_undelivered_responsibility_until_it_clears() {
    let (harness, path, a) = switched_over(&row("m0", "before"));
    let _hosts = herdr_configured();
    crashed_mid_post(&harness);
    let mut writer = harness.writer();
    let bindings = startup_log(&mut writer);
    let claude = ShadowProvider::Claude;
    let (stop, task, owed, demand) = spawn_owing(writer, Arc::clone(&bindings), claude);
    let _wanting = demand.want();
    polls(2).await;
    assert_eq!(*owed.borrow(), Some(owes(0, 1, 0, 0)), "an open Prepared");

    let torn = row("m2", "two");
    let (head, tail) = torn.split_at(torn.len() / 2);
    append(&path, &row("m1", "one"));
    append(&path, head);
    let pending = BindingTarget::Pending {
        payload_session_id: "s2".into(),
        payload_transcript_path: path.with_file_name("s2.jsonl"),
    };
    bindings.commit(bound(2, Some(&a), pending, BindingCause::Clear, None));
    polls(2).await;
    assert_eq!(
        *owed.borrow(),
        Some(owes(1, 1, 1, 1)),
        "without ownership the piece, the torn line and the pending bind all wait"
    );

    // The open Prepared is settled from history first, after its last look.
    harness.gate.acquired();
    polls(35).await;
    assert_eq!(harness.port.posts(), ["one"]);
    assert_eq!(*owed.borrow(), Some(owes(0, 0, 1, 1)), "posted and settled");
    append(&path, tail);
    let next = path.with_file_name("s2.jsonl");
    std::fs::write(&next, row("n1", "new")).unwrap();
    let resolved = BindingRecord::Resolved {
        resolves_seq: 2,
        source: source_id_for("s2", &next).unwrap(),
    };
    bindings.commit(event(3, resolved, Utc::now()));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["one", "two", "new"]);
    assert_eq!(*owed.borrow(), Some(owes(0, 0, 0, 0)), "all clear");

    let store = path.parent().unwrap().join("o_store");
    let boundary = store.join(CHANNEL.to_string()).join(BOUNDARY_FILE);
    std::fs::write(&boundary, b"{not json").unwrap();
    polls(2).await;
    assert_eq!(*owed.borrow(), None, "unreadable is not zero");
    halt(stop, task).await;
    assert!(owed.has_changed().is_err(), "the ended actor dropped it");
}

// A channel without Herdr publishes nothing and counts no POSTs, even while a read waits, so it
// pays no extra reads.
#[tokio::test(start_paused = true)]
async fn a_channel_without_herdr_publishes_nothing_and_counts_no_posts() {
    let (harness, path, _) = switched_over(&row("m0", "before"));
    harness.gate.acquired();
    let mut writer = harness.writer();
    let bindings = startup_log(&mut writer);
    let (stop, task, owed, demand) = spawn_owing(writer, bindings, ShadowProvider::Claude);
    let _wanting = demand.want();
    append(&path, &row("m1", "one"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["one"]);
    assert_eq!(*owed.borrow(), None);
    assert!(!owed.has_changed().unwrap(), "never published");
    assert_eq!(undelivered_reads_for_test(), 0);
    assert_eq!(posts_in_flight(CHANNEL), None);
    halt(stop, task).await;
}

// An announced unit whose sealing record has not come yet is owed until that record arrives.
#[tokio::test(start_paused = true)]
async fn an_announced_unit_counts_as_unsealed_until_its_record_seals_it() {
    let codex = |value: serde_json::Value| {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        line
    };
    let (harness, path, _) = switched_over(b"");
    let _hosts = herdr_configured();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let bindings = startup_log(&mut writer);
    let (stop, task, owed, demand) = spawn_owing(writer, bindings, ShadowProvider::Codex);
    let _wanting = demand.want();
    let announced = serde_json::json!({"type": "event_msg", "payload": {
        "type": "item_completed", "item": {"type": "AgentMessage", "id": "msg_c1"}}});
    append(&path, &codex(announced));
    polls(3).await;
    let unsealed = Undelivered {
        unsealed: 1,
        ..owes(0, 0, 0, 0)
    };
    assert_eq!(*owed.borrow(), Some(unsealed));
    let sealed = serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "assistant", "id": "msg_c1",
        "content": [{"type": "output_text", "text": "hello"}]}});
    append(&path, &codex(sealed));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["hello"]);
    assert_eq!(*owed.borrow(), Some(owes(0, 0, 0, 0)), "sealed and posted");
    halt(stop, task).await;
}

/// One POST that waits on `hold` until it is released or dropped.
fn held_post(harness: &Harness) -> tokio::sync::oneshot::Sender<()> {
    let (release, hold) = tokio::sync::oneshot::channel();
    harness.port.lazy.store(true, Ordering::SeqCst);
    *harness.port.hold.lock().unwrap() = Some(hold);
    release
}

// A POST counts from its start until its request ends, is timed out and aborted, or finishes
// after the writer that started it is gone; the channel's writers share the count.
#[tokio::test(start_paused = true)]
async fn a_post_counts_as_running_from_its_start_until_its_request_is_gone() {
    let harness = Harness::new();
    let _hosts = herdr_configured();
    harness.gate.acquired();
    assert_eq!(posts_in_flight(CHANNEL), None, "no writer counted yet");

    let release = held_post(&harness);
    let mut writer = harness.writer();
    assert_eq!(posts_in_flight(CHANNEL), Some(0));
    let delivered = tokio::spawn(async move { writer.deliver(&piece("m1", "a")).await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(posts_in_flight(CHANNEL), Some(1), "running");
    release.send(()).unwrap();
    assert_eq!(delivered.await.unwrap(), Step::Done);
    assert_eq!(posts_in_flight(CHANNEL), Some(0), "ended");

    let _never = held_post(&harness);
    let mut writer = harness.writer();
    let timed_out = tokio::spawn(async move { writer.deliver(&piece("m2", "b")).await });
    tokio::time::sleep(POST_TIMEOUT - Duration::from_secs(1)).await;
    assert_eq!(posts_in_flight(CHANNEL), Some(1));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(posts_in_flight(CHANNEL), Some(0), "aborted at the timeout");
    timed_out.await.unwrap();

    let release = held_post(&harness);
    let mut writer = harness.writer();
    let dropped = tokio::spawn(async move { writer.deliver(&piece("m3", "c")).await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    dropped.abort();
    let _ = dropped.await;
    let _next = harness.writer();
    assert_eq!(posts_in_flight(CHANNEL), Some(1), "outlives its writer");
    release.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(posts_in_flight(CHANNEL), Some(0));
}
