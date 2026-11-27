#![cfg(unix)]

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;

use super::confirm::{self, Verdict};
use super::deliver::{self, ChannelWriter, Step};
use super::pieces::{self, Derived, PieceWork, UnitDeriver};
use super::test_support::*;
use super::*;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::{LedgerEntry, PieceOutcome};
use crate::services::tui_o::store::{ChannelStore, InitSource, Initialized, OStore, StoreConfig};

#[tokio::test(start_paused = true)]
async fn a_piece_is_prepared_under_ownership_then_posted_once() {
    let harness = Harness::new();
    let epoch = harness.gate.acquired();
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m1", "hello")).await, Step::Done);
    assert_eq!(writer.deliver(&piece("m1", "hello")).await, Step::Done);
    assert_eq!(harness.port.posts(), ["hello"]);
    assert_eq!(*harness.port.prepared_before_post.lock().unwrap(), [true]);
    let ledger = writer.store().ledger();
    assert_eq!(
        (ledger.anchor(), ledger.piece(0).map(|piece| piece.epoch)),
        (1000, Some(epoch))
    );
    assert_eq!(outcome(&mut writer, "m1"), Some(PieceOutcome::Posted(1000)));
    assert_eq!(harness.lease.released.load(Ordering::SeqCst), 1);
    assert_eq!(harness.alarms.taken(), []);
}

#[tokio::test(start_paused = true)]
async fn nothing_is_prepared_or_posted_without_ownership_and_the_delivery_lease() {
    let harness = Harness::new();
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::NoGateway);
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::NoGateway);
    assert_eq!(harness.alarms.taken(), [WriterAlarm::PausedNoGateway]);
    harness.gate.acquired();
    harness.lease.busy.store(true, Ordering::SeqCst);
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::LeaseBusy);
    harness.lease.busy.store(false, Ordering::SeqCst);
    // Ownership lost after the lease check and before admission: the gate refuses the hand-off.
    let gate = Arc::clone(&harness.gate);
    *harness.lease.on_acquire.lock().unwrap() = Some(Box::new(move || gate.uncertain()));
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::NoGateway);
    *harness.lease.on_acquire.lock().unwrap() = None;
    assert_eq!(writer.store().ledger().next_serial(), 0);
    assert!(harness.port.posts().is_empty());
    harness.gate.acquired();
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::Done);
    assert_eq!(harness.port.posts(), ["a"]);
}

#[tokio::test(start_paused = true)]
async fn an_unclear_post_is_settled_from_history_and_retried_only_once_if_absent() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .extend([Reply::CreatedUnseen, Reply::Unsent]);
    assert_eq!(writer.deliver(&piece("m1", "first")).await, Step::Done);
    assert_eq!(outcome(&mut writer, "m1"), Some(PieceOutcome::Posted(1000)));
    let started = tokio::time::Instant::now();
    assert_eq!(writer.deliver(&piece("m2", "second")).await, Step::Done);
    assert!(started.elapsed() >= confirm::LAST_LOOK);
    assert_eq!(outcome(&mut writer, "m2"), Some(PieceOutcome::NotFound));
    assert_eq!(
        writer.deliver(&piece("m2", "second")).await,
        Step::LeaseBusy
    );
    let when = super::local_retry::pending(writer.store().ledger())
        .unwrap()
        .due;
    assert_eq!(writer.retry_local(when).await, Step::Done);
    assert_eq!(writer.deliver(&piece("m2", "second")).await, Step::Done);
    assert_eq!(harness.port.posts(), ["first", "second", "second"]);
    assert_eq!(writer.store().ledger().anchor(), 1001);
    assert_eq!(
        harness.alarms.taken(),
        [WriterAlarm::NotFound { serial: 1 }]
    );
}

#[tokio::test(start_paused = true)]
async fn a_created_reply_by_another_author_is_settled_from_history_rather_than_trusted() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let replies = [Reply::CreatedByOther];
    harness.port.replies.lock().unwrap().extend(replies);
    assert_eq!(writer.deliver(&piece("m1", "first")).await, Step::Done);
    assert_eq!(outcome(&mut writer, "m1"), Some(PieceOutcome::NotFound));
    assert_eq!(harness.port.posts(), ["first"]);
}

#[tokio::test(start_paused = true)]
async fn settlement_stays_ambiguous_or_unresolved_when_history_cannot_single_out_the_post() {
    let port = FakePort::default();
    port.say(BOT, "same");
    port.say(BOT, "same");
    let proof = [1001];
    assert_eq!(
        confirm::settle(&port, CHANNEL, 0, "same", false, &proof).await,
        Verdict::Ambiguous(vec![1000, 1001])
    );
    assert_eq!(
        confirm::settle(&port, CHANNEL, 1000, "same", true, &proof).await,
        Verdict::Ambiguous(vec![1001])
    );
    assert_eq!(
        confirm::settle(&port, CHANNEL, 1000, "same", false, &proof).await,
        Verdict::Posted(1001)
    );
    assert_eq!(
        confirm::settle(&port, CHANNEL, 1001, "same", false, &proof).await,
        Verdict::NotFound
    );
    assert!(
        matches!(
            confirm::settle(&port, CHANNEL, 1001, "same", false, &[]).await,
            Verdict::Unresolved(_)
        ),
        "an empty page with no own post read back proves nothing"
    );
    assert!(
        matches!(
            confirm::settle(&port, CHANNEL, 1001, "same", false, &[4242]).await,
            Verdict::Unresolved(_)
        ),
        "an own post gone with a 404 proves nothing"
    );
    port.unreadable.store(true, Ordering::SeqCst);
    assert!(matches!(
        confirm::settle(&port, CHANNEL, 1001, "same", false, &proof).await,
        Verdict::Unresolved(_)
    ));
    port.unreadable.store(false, Ordering::SeqCst);
    port.say(BOT + 1, "someone else");
    assert_eq!(
        confirm::settle(&port, CHANNEL, 1001, "same", false, &[]).await,
        Verdict::NotFound,
        "a listed message shows the history readable"
    );
    let pages = confirm::HISTORY_PAGE * confirm::MAX_PAGES;
    for _ in 0..pages {
        port.say(7, "chatter");
    }
    assert!(matches!(
        confirm::settle(&port, CHANNEL, 0, "same", false, &proof).await,
        Verdict::Unresolved(_)
    ));
    assert_eq!(confirm::judge(&[5], false), Some(Verdict::Posted(5)));
}

/// Answers every history read with a full page of chatter just inside the read timeout.
struct SlowPages;

impl DiscordPort for SlowPages {
    fn bot_id(&self) -> u64 {
        BOT
    }

    fn post(&self, _: u64, _: String) -> impl Future<Output = PostOutcome> + Send + 'static {
        std::future::pending()
    }

    async fn history_after(&self, _: u64, after: u64) -> Result<Vec<SeenMessage>, String> {
        let just_inside = confirm::READ_TIMEOUT - std::time::Duration::from_millis(100);
        tokio::time::sleep(just_inside).await;
        let chatter = |n| SeenMessage {
            id: after + n,
            author_id: 7,
            content: "chatter".into(),
        };
        Ok((1..=confirm::HISTORY_PAGE as u64).map(chatter).collect())
    }

    async fn message(&self, _: u64, _: u64) -> Result<Option<SeenMessage>, String> {
        Ok(None)
    }
}

/// Reads that each finish in time still end the settlement at its deadline, inside the lease.
#[tokio::test(start_paused = true)]
async fn a_settlement_ends_at_its_deadline_however_slowly_full_pages_arrive() {
    let started = tokio::time::Instant::now();
    let verdict = confirm::settle(&SlowPages, CHANNEL, 0, "same", false, &[]).await;
    let deadline = confirm::SETTLE_DEADLINE;
    let late = format!("settlement exceeded {deadline:?}");
    assert_eq!(verdict, Verdict::Unresolved(late));
    assert_eq!(started.elapsed(), deadline);
}

#[tokio::test(start_paused = true)]
async fn an_unclear_post_sharing_an_earlier_unsettled_payload_stays_ambiguous() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .extend([Reply::Unsent, Reply::CreatedUnseen]);
    writer.deliver(&piece("m1", "ok")).await;
    writer.deliver(&piece("m2", "ok")).await;
    // No post of this bot yet proves the empty history readable.
    assert!(matches!(
        outcome(&mut writer, "m1"),
        Some(PieceOutcome::Unresolved(_))
    ));
    assert_eq!(
        outcome(&mut writer, "m2"),
        Some(PieceOutcome::Ambiguous(vec![1000]))
    );
    assert_eq!(writer.store().ledger().anchor(), 100);
}

#[tokio::test(start_paused = true)]
async fn a_refused_post_blocks_the_channel_even_after_reopening() {
    let harness = Harness::new();
    harness.gate.acquired();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Refused(403));
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m1", "x")).await, Step::Stopped);
    assert_eq!(writer.deliver(&piece("m2", "y")).await, Step::Stopped);
    assert_eq!(
        harness.alarms.taken(),
        [WriterAlarm::Blocked { status: 403 }]
    );
    let mut reopened = harness.writer();
    assert!(reopened.is_stopped());
    assert_eq!(reopened.deliver(&piece("m2", "y")).await, Step::Stopped);
    assert_eq!(harness.port.posts(), ["x"]);
}

#[tokio::test(start_paused = true)]
async fn a_prepared_left_by_a_crash_is_settled_before_the_next_post() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut channel = harness.channel();
    for (serial, payload) in [(0, "never sent"), (1, "was sent")] {
        let (unit_key, payload) = (unit(&format!("c{serial}")), payload.to_string());
        let anchor_id = channel.ledger().anchor();
        let prepared = LedgerEntry::Prepared {
            notice_origin: None,
            local_retry: None,
            serial,
            unit_key,
            piece_index: 0,
            payload,
            anchor_id,
            epoch: 1,
        };
        channel.append_ledger(prepared).unwrap();
        if serial == 0 {
            channel
                .append_ledger(LedgerEntry::NotFound { serial })
                .unwrap();
        }
    }
    harness.port.say(BOT, "was sent");
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m1", "next")).await, Step::Done);
    assert_eq!(outcome(&mut writer, "c1"), Some(PieceOutcome::Posted(1000)));
    assert_eq!(outcome(&mut writer, "m1"), Some(PieceOutcome::Posted(1001)));
    assert_eq!(harness.port.posts(), ["next"]);
}

#[tokio::test(start_paused = true)]
async fn a_blocked_record_or_a_ledger_violation_stops_the_channel_before_any_post() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let blocked = Derived::Blocked {
        reason: "unsupported block".into(),
    };
    assert_eq!(writer.deliver(&blocked).await, Step::Stopped);
    assert_eq!(writer.deliver(&piece("m1", "x")).await, Step::Stopped);
    let mut channel = harness.channel();
    channel
        .append_ledger(LedgerEntry::Posted {
            serial: 9,
            msg_id: 5,
            confirmation: None,
        })
        .unwrap();
    let mut reopened = harness.writer();
    assert_eq!(reopened.deliver(&piece("m1", "x")).await, Step::Stopped);
    let alarms = harness.alarms.taken();
    assert!(matches!(
        alarms.as_slice(),
        [
            WriterAlarm::SchemaBlocked { .. },
            WriterAlarm::LedgerViolation { .. }
        ]
    ));
    assert!(harness.port.posts().is_empty());
}

#[test]
fn derivation_splits_each_unit_once_and_excludes_or_blocks_the_rest() {
    let row = |id: &str, text: &str| {
        let row = serde_json::json!({
            "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
            "message": {"id": id, "content": [{"type": "text", "text": text}]},
        });
        let line = serde_json::to_vec(&row).unwrap();
        CapturedRecord {
            start: 0,
            end: line.len() as u64 + 1,
            line,
        }
    };
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let long = "word ".repeat(900);
    let pieces = deriver.derive(&row("msg_1", &long));
    let expected = crate::domain::message_split::split_for_shadow(long.trim());
    assert!(expected.len() >= 2);
    let payloads: Vec<String> = pieces
        .iter()
        .map(|item| match item {
            Derived::Piece(piece) => piece.payload.clone(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        payloads,
        expected
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
    );
    assert!(
        deriver.derive(&row("msg_1", &long)).is_empty(),
        "a fork copy is not owed twice"
    );
    assert!(matches!(
        deriver.derive(&row("msg_1", "changed")).as_slice(),
        [Derived::Blocked { .. }]
    ));
    // Same pieces and UTF-16 lengths: only the piece digest can tell this change apart.
    assert!(matches!(
        deriver
            .derive(&row("msg_1", &long.replacen("word", "ward", 1)))
            .as_slice(),
        [Derived::Blocked { .. }]
    ));
    let result = serde_json::json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}});
    let line = serde_json::to_vec(&result).unwrap();
    let record = CapturedRecord {
        start: 0,
        end: line.len() as u64 + 1,
        line,
    };
    assert!(matches!(
        deriver.derive(&record).as_slice(),
        [Derived::Excluded { .. }]
    ));
    let torn = CapturedRecord {
        start: 0,
        end: 3,
        line: b"{\"t".to_vec(),
    };
    assert!(matches!(
        deriver.derive(&torn).as_slice(),
        [Derived::Blocked { .. }]
    ));
}

/// A tool call is recorded excluded for the live panel and never posted; the body after it posts
/// and becomes the channel's newest O post.
#[tokio::test(start_paused = true)]
async fn a_tool_call_is_left_to_the_panel_and_only_the_body_posts() {
    let record = |row: serde_json::Value| {
        let line = serde_json::to_vec(&row).unwrap();
        let end = line.len() as u64 + 1;
        CapturedRecord {
            start: 0,
            end,
            line,
        }
    };
    let assistant = |id: &str, block: serde_json::Value| {
        record(
            serde_json::json!({"type": "assistant", "uuid": format!("u-{id}"),
            "apiBlockIndex": 0, "message": {"id": id, "content": [block]}}),
        )
    };
    let tool_use = serde_json::json!({"type": "tool_use", "id": "toolu_1", "name": "Bash",
        "input": {"command": "ls"}});
    let codex_call = record(serde_json::json!({"type": "response_item", "payload": {
        "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "shell",
        "arguments": "{}"}}));
    let mut codex = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
    let excluded = |items: &[Derived]| {
        matches!(items, [Derived::Excluded { unit_key, reason }]
            if unit_key.kind == UnitKind::Tool && reason == pieces::TOOL_CALL_PANEL)
    };
    assert!(excluded(&codex.derive(&codex_call)));

    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let tool = deriver.derive(&assistant("msg_t", tool_use));
    assert!(excluded(&tool), "{tool:?}");
    let body_block = serde_json::json!({"type": "text", "text": "done"});
    let items = [tool, deriver.derive(&assistant("msg_b", body_block))].concat();
    for item in &items {
        assert_eq!(writer.deliver(item).await, Step::Done);
    }
    assert_eq!(harness.port.posts(), ["done"]);
    let tool_key = UnitKey {
        native_key: "msg_t:0".into(),
        kind: UnitKind::Tool,
        ..unit("")
    };
    let ledger = writer.store().ledger();
    assert_eq!(ledger.excluded(&tool_key), Some(pieces::TOOL_CALL_PANEL));
    let Some(PieceOutcome::Posted(posted)) = outcome(&mut writer, "msg_b:0") else {
        panic!("body not posted");
    };
    assert!(deliver::last_posted(CHANNEL) >= Some(posted));
}

/// A writer opened after a restart learns O's newest post from the recovered ledger.
#[tokio::test(start_paused = true)]
async fn a_restarted_writer_recovers_the_newest_post_from_its_ledger() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m1", "hello")).await, Step::Done);
    let Some(PieceOutcome::Posted(posted)) = outcome(&mut writer, "m1") else {
        panic!("piece not posted");
    };
    drop(writer);
    let _restart = deliver::forget_posted_for_tests(CHANNEL);
    let _restarted = harness.writer();
    assert!(deliver::last_posted(CHANNEL) >= Some(posted));
}

/// Guards that no POST's HTTP request starts after ownership closes, for Unknown and Lost alike.
#[tokio::test(start_paused = true)]
async fn no_post_starts_its_request_after_ownership_closes() {
    let closes: [fn(&OwnershipGate); 2] = [OwnershipGate::uncertain, OwnershipGate::lost];
    for close in closes {
        let harness = Harness::new();
        harness.gate.acquired();
        harness.port.lazy.store(true, Ordering::SeqCst);
        let log = Arc::clone(&harness.port.started);
        let (wake, woken) = tokio::sync::oneshot::channel::<()>();
        let (gate, closed_log) = (Arc::clone(&harness.gate), Arc::clone(&log));
        // Runs as soon as the writer yields after handing the POST over.
        let closer = tokio::spawn(async move {
            woken.await.unwrap();
            close(&gate);
            closed_log.lock().unwrap().push("closed");
        });
        tokio::task::yield_now().await;
        let wake = Mutex::new(Some(wake));
        *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
            wake.lock().unwrap().take().map(|wake| wake.send(()));
        }));
        let mut writer = harness.writer();
        writer.deliver(&piece("m1", "a")).await;
        closer.await.unwrap();
        assert_eq!(*log.lock().unwrap(), ["http", "closed"]);
    }
}

/// Guards that the delivery lease outlives an aborted writer until its POST can no longer send,
/// and that a timed-out POST hands the lease back exactly once.
#[tokio::test(start_paused = true)]
async fn the_delivery_lease_is_held_until_the_post_task_ends() {
    let harness = Harness::new();
    harness.gate.acquired();
    harness.port.lazy.store(true, Ordering::SeqCst);
    let (release, hold) = tokio::sync::oneshot::channel();
    *harness.port.hold.lock().unwrap() = Some(hold);
    let mut writer = harness.writer();
    let parent = tokio::spawn(async move { writer.deliver(&piece("m1", "a")).await });
    while harness.port.started.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    parent.abort();
    assert!(parent.await.unwrap_err().is_cancelled());
    let released = || harness.lease.released.load(Ordering::SeqCst);
    assert_eq!(
        released(),
        0,
        "lease released while the POST could still send"
    );
    release.send(()).unwrap();
    while released() == 0 {
        tokio::task::yield_now().await;
    }
    let (_never, hold) = tokio::sync::oneshot::channel();
    *harness.port.hold.lock().unwrap() = Some(hold);
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m2", "b")).await, Step::Done);
    assert_eq!(released(), 2);
    assert_eq!(harness.port.posts(), ["a", "b"]);
}

/// Guards that a ledger violation raised while running stops the same writer before its next POST.
#[tokio::test(start_paused = true)]
async fn a_violation_recorded_while_running_stops_the_writer_before_the_next_post() {
    let harness = Harness::new();
    harness.gate.acquired();
    let replies = [Reply::CreatedBehind];
    harness.port.replies.lock().unwrap().extend(replies);
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::Stopped);
    assert_eq!(writer.deliver(&piece("m2", "b")).await, Step::Stopped);
    assert_eq!(harness.port.posts(), ["a"]);
    assert!(matches!(
        harness.alarms.taken().as_slice(),
        [WriterAlarm::LedgerViolation { .. }]
    ));
}

/// Guards that an id Discord minted just behind the previous post neither stops nor re-sends.
#[tokio::test(start_paused = true)]
async fn a_post_whose_id_trails_the_previous_one_keeps_the_writer_running() {
    let harness = Harness::new();
    harness.gate.acquired();
    let replies = [Reply::Created, Reply::CreatedReordered];
    harness.port.replies.lock().unwrap().extend(replies);
    let mut writer = harness.writer();
    for (key, body) in [("m1", "a"), ("m2", "b"), ("m3", "c")] {
        assert_eq!(writer.deliver(&piece(key, body)).await, Step::Done);
    }
    assert_eq!(harness.port.posts(), ["a", "b", "c"]);
    assert!(harness.alarms.taken().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_stop_keeps_its_store_error_and_names_a_prepared_only_while_nothing_posted_it() {
    use crate::services::tui_o::store::fault::{self, Keep, Step as At};
    let full = std::io::ErrorKind::StorageFull;
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let planted = fault::plant(&ledger_path(&harness), At::Append(Keep::All), full, None);
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::Stopped);
    drop(planted);
    writer.stop(WriterAlarm::SelectionMissing);
    let cause = writer.stop_cause().cloned().unwrap();
    assert!(
        matches!(cause.alarm, WriterAlarm::Halted { .. }),
        "{cause:?}"
    );
    assert_eq!(cause.io, Some(full), "the first stop keeps its cause");
    assert_eq!(cause.unsent.as_ref().map(|unsent| unsent.serial()), Some(0));
    assert!(harness.port.posts().is_empty());
    assert_eq!(
        harness.alarms.taken().len(),
        1,
        "only the first stop alarms"
    );

    // A failure after the POST went out names nothing, so recovery settles it from history.
    for keep in [Keep::Nothing, Keep::Half] {
        let harness = Harness::new();
        harness.gate.acquired();
        let mut writer = harness.writer();
        let (ledger, slot) = (ledger_path(&harness), Arc::new(Mutex::new(None)));
        let planting = Arc::clone(&slot);
        *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
            let planted = fault::plant(&ledger, At::Append(keep), full, Some(1));
            *planting.lock().unwrap() = Some(planted);
        }));
        assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::Stopped);
        let cause = writer.stop_cause().cloned().unwrap();
        assert_eq!((cause.io, cause.unsent), (Some(full), None), "{keep:?}");
        assert_eq!(harness.port.posts(), ["a"]);
    }
}
#[path = "notice_retry_tests.rs"]
mod notice_retry_tests;
#[path = "tool_card_replay_tests.rs"]
mod tool_card_replay;

#[path = "actor_tests.rs"]
pub(crate) mod actor;
#[path = "operator_resume_tests.rs"]
mod operator_resume;
#[path = "tool_result_tests.rs"]
mod tool_result_tests;
#[path = "turn_summary_replay_tests.rs"]
mod turn_summary_replay;

#[path = "boundary_pending_health_tests.rs"]
mod boundary_pending_health_tests;
#[path = "codex_launch_cause_tests.rs"]
mod codex_launch_cause;
#[path = "restart_notice_tests.rs"]
mod restart_notice_tests;

#[tokio::test(start_paused = true)]
async fn a_gateway_return_ends_the_pause_in_process_health() {
    use crate::services::tui_o::alarm::{AlarmRouter, health_reasons};
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::a_gateway_return_ends_the_pause_in_process_health"
    )) {
        return;
    }
    let harness = Harness::new();
    let (gate, port) = (Arc::clone(&harness.gate), Arc::clone(&harness.port));
    let lease = Arc::clone(&harness.lease);
    let router = AlarmRouter::for_process(None, None);
    let mut writer = ChannelWriter::new(harness.channel(), gate, port, lease, router);
    let paused = [format!("tui_o:paused_no_gateway:{CHANNEL}")];
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::NoGateway);
    assert_eq!(health_reasons(), paused);
    harness.gate.acquired();
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::Done);
    assert!(health_reasons().is_empty(), "{:?}", health_reasons());
    harness.gate.uncertain();
    assert_eq!(writer.deliver(&piece("m2", "b")).await, Step::NoGateway);
    assert_eq!(health_reasons(), paused);
}
