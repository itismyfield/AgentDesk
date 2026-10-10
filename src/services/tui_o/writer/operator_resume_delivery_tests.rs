use super::super::*;
use crate::services::tui_o::store::ledger::{PieceDisposition, ResumeApproval};

#[tokio::test(start_paused = true)]
async fn operator_resume_timeout_boundary_joins_request_before_settlement_and_following_post() {
    use std::time::Duration;
    let (harness, _) = approved();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Unsent);
    harness.port.lazy.store(true, Ordering::SeqCst);
    harness.port.unreadable.store(true, Ordering::SeqCst);
    let (never, hold) = tokio::sync::oneshot::channel();
    *harness.port.hold.lock().unwrap() = Some(hold);
    let mut writer = harness.writer();
    let task = tokio::spawn(async move {
        let step = writer.deliver(&piece("original", "original bytes")).await;
        (writer, step)
    });
    while harness.port.started.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(59_999)).await;
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    assert_eq!(harness.lease.released.load(Ordering::SeqCst), 0);
    assert_eq!(harness.channel().ledger().unresolved().unwrap().0, 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    let (mut writer, step) = task.await.unwrap();
    assert_eq!(step, Step::Done);
    assert!(
        never.send(()).is_err(),
        "the request receiver has been dropped"
    );
    assert_eq!(harness.lease.released.load(Ordering::SeqCst), 1);
    assert!(matches!(
        outcome(&mut writer, "original"),
        Some(PieceOutcome::Unresolved(_))
    ));
    assert_eq!(
        writer.deliver(&piece("following", "later")).await,
        Step::Done
    );
    assert_eq!(harness.port.posts(), ["original bytes", "later"]);
}

#[tokio::test]
async fn operator_resume_complete_prepared_survives_process_exit_and_is_only_settled() {
    let (harness, _) = approved();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "services::tui_o::writer::tests::operator_resume::delivery::operator_resume_prepared_child",
            "--test-threads=1"])
        .env("AGENTDESK_OPERATOR_PREPARED_CHILD", harness._runtime.path())
        .output().unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    assert_eq!(
        harness
            .channel()
            .ledger()
            .approval(0)
            .unwrap()
            .consumed_serial,
        Some(1)
    );
    harness.port.unreadable.store(true, Ordering::SeqCst);
    let mut writer = harness.writer();
    assert!(!writer.is_stopped());
    assert_eq!(
        writer.deliver(&piece("original", "original bytes")).await,
        Step::Done
    );
    assert!(
        harness.port.posts().is_empty(),
        "a new process cannot infer Unsent"
    );
    assert!(matches!(
        outcome(&mut writer, "original"),
        Some(PieceOutcome::Unresolved(_))
    ));
    assert_eq!(
        writer.store().ledger().approval(0).unwrap().consumed_serial,
        Some(1)
    );
    assert_eq!(
        writer.deliver(&piece("following", "later")).await,
        Step::Done
    );
    assert_eq!(harness.port.posts(), ["later"]);
}

#[test]
fn operator_resume_prepared_child() {
    let Some(runtime) = std::env::var_os("AGENTDESK_OPERATOR_PREPARED_CHILD") else {
        return;
    };
    let store = OStore::existing(std::path::Path::new(&runtime)).unwrap();
    let era = store.read_era().unwrap().unwrap();
    let mut channel = store.open_channel(&era, CHANNEL).unwrap().unwrap();
    let original = channel.ledger().piece(0).unwrap().clone();
    channel
        .append_ledger(LedgerEntry::Prepared {
            serial: 1,
            unit_key: original.unit_key,
            piece_index: original.piece_index,
            payload: original.payload,
            anchor_id: 100,
            epoch: 1,
        })
        .unwrap();
    assert_eq!(
        channel.ledger().approval(0).unwrap().consumed_serial,
        Some(1)
    );
}

fn approved() -> (Harness, ResumeApproval) {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut channel = harness.channel();
    channel
        .append_ledger(LedgerEntry::Prepared {
            serial: 0,
            unit_key: unit("original"),
            piece_index: 0,
            payload: "original bytes".into(),
            anchor_id: 100,
            epoch: 1,
        })
        .unwrap();
    channel
        .append_ledger(LedgerEntry::Rejected {
            serial: 0,
            status: 403,
        })
        .unwrap();
    drop(channel);
    let approval = harness
        .store
        .record_operator_resume(CHANNEL, 0, "operator", "restored")
        .unwrap();
    (harness, approval)
}

#[tokio::test]
async fn operator_resume_no_approval_restarts_block_but_same_harness_with_approval_posts_once() {
    let harness = Harness::new();
    harness.gate.acquired();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Refused(403));
    let mut writer = harness.writer();
    assert_eq!(
        writer.deliver(&piece("original", "original bytes")).await,
        Step::Stopped
    );
    drop(writer);
    let mut writer = harness.writer();
    assert!(writer.is_stopped());
    assert_eq!(
        writer.deliver(&piece("following", "later")).await,
        Step::Stopped
    );
    assert_eq!(harness.port.posts(), ["original bytes"]);
    drop(writer);
    harness
        .store
        .record_operator_resume(CHANNEL, 0, "operator", "restored")
        .unwrap();
    let ledger = ledger_path(&harness);
    let next = std::sync::atomic::AtomicU64::new(1);
    *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
        let text = std::fs::read_to_string(&ledger).unwrap();
        let line: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(line["entry"]["type"], "prepared");
        assert_eq!(line["entry"]["serial"], next.fetch_add(1, Ordering::SeqCst));
        assert_eq!(line["entry"]["epoch"], 1);
    }));
    let mut writer = harness.writer();
    assert!(!writer.is_stopped());
    assert_eq!(
        writer
            .deliver(&piece("original", "different rederived bytes"))
            .await,
        Step::Done
    );
    assert_eq!(
        writer.deliver(&piece("following", "later")).await,
        Step::Done
    );
    assert_eq!(
        harness.port.posts(),
        ["original bytes", "original bytes", "later"]
    );
    assert!(
        harness
            .port
            .prepared_before_post
            .lock()
            .unwrap()
            .iter()
            .all(|ready| *ready)
    );
    assert_eq!(writer.store().ledger().blocked(), None);
    assert_eq!(
        writer.store().ledger().approval(0).unwrap().consumed_serial,
        Some(1)
    );
}

#[tokio::test]
async fn operator_resume_busy_and_gate_loss_leave_approval_unconsumed_then_send_canonical_bytes() {
    let (harness, approval) = approved();
    let mut writer = harness.writer();
    harness.lease.busy.store(true, Ordering::SeqCst);
    assert_eq!(
        writer.deliver(&piece("original", "wrong")).await,
        Step::LeaseBusy
    );
    harness.lease.busy.store(false, Ordering::SeqCst);
    let gate = harness.gate.clone();
    *harness.lease.on_acquire.lock().unwrap() = Some(Box::new(move || gate.lost()));
    assert_eq!(
        writer.deliver(&piece("original", "wrong")).await,
        Step::NoGateway
    );
    assert_eq!(writer.store().ledger().approval(0), Some(&approval));
    assert_eq!(writer.store().ledger().next_serial(), 1);
    assert!(harness.port.posts().is_empty());
    *harness.lease.on_acquire.lock().unwrap() = None;
    harness.gate.acquired();
    assert_eq!(
        writer.deliver(&piece("original", "wrong")).await,
        Step::Done
    );
    assert_eq!(harness.port.posts(), ["original bytes"]);
}

#[tokio::test]
async fn operator_resume_retry_rejection_demands_the_new_serial_and_a_new_approval_id() {
    let (harness, approval) = approved();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Refused(404));
    let mut writer = harness.writer();
    assert_eq!(
        writer.deliver(&piece("original", "original bytes")).await,
        Step::Stopped
    );
    drop(writer);
    assert!(harness.writer().is_stopped());
    let duplicate = harness
        .store
        .record_operator_resume(CHANNEL, 0, "again", "again")
        .unwrap();
    assert_eq!(duplicate.approval_id, approval.approval_id);
    assert_eq!(duplicate.consumed_serial, Some(1));
    let next = harness
        .store
        .record_operator_resume(CHANNEL, 1, "operator", "new failure restored")
        .unwrap();
    assert_ne!(next.approval_id, approval.approval_id);
    let mut writer = harness.writer();
    assert_eq!(
        writer.deliver(&piece("original", "original bytes")).await,
        Step::Done
    );
    assert_eq!(harness.port.posts(), ["original bytes", "original bytes"]);
    assert_eq!(
        writer.store().ledger().approval(1).unwrap().consumed_serial,
        Some(2)
    );
}

#[tokio::test]
async fn operator_resume_uncertain_retry_never_reblocks_or_reposts_after_reopening() {
    for case in ["not_found", "ambiguous", "unresolved"] {
        let (harness, _) = approved();
        if case == "ambiguous" {
            harness.port.say(BOT, "original bytes");
            harness.port.say(BOT, "original bytes");
        }
        if case == "unresolved" {
            harness.port.unreadable.store(true, Ordering::SeqCst);
        }
        harness
            .port
            .replies
            .lock()
            .unwrap()
            .push_back(Reply::Unsent);
        let mut writer = harness.writer();
        assert_eq!(
            writer.deliver(&piece("original", "original bytes")).await,
            Step::Done
        );
        let result = outcome(&mut writer, "original").unwrap();
        assert!(match case {
            "not_found" => matches!(result, PieceOutcome::NotFound),
            "ambiguous" => matches!(result, PieceOutcome::Ambiguous(_)),
            _ => matches!(result, PieceOutcome::Unresolved(_)),
        });
        drop(writer);
        for _ in 0..2 {
            let mut writer = harness.writer();
            assert!(!writer.is_stopped(), "{case}");
            assert_eq!(
                writer.deliver(&piece("original", "original bytes")).await,
                Step::Done
            );
            assert_eq!(
                writer.store().ledger().disposition(&unit("original"), 0),
                PieceDisposition::Settled
            );
        }
        assert_eq!(harness.port.posts(), ["original bytes"]);
        let mut writer = harness.writer();
        assert_eq!(
            writer.deliver(&piece("following", "later")).await,
            Step::Done
        );
        assert_eq!(harness.port.posts(), ["original bytes", "later"]);
    }
}

#[tokio::test]
async fn operator_resume_unsent_append_errors_restore_approval_and_post_once() {
    use crate::services::tui_o::store::fault::{self, Keep, Step as At};
    for keep in [Keep::Nothing, Keep::Half, Keep::All] {
        let (harness, approval) = approved();
        let mut writer = harness.writer();
        let planted = fault::plant(
            &ledger_path(&harness),
            At::Append(keep),
            std::io::ErrorKind::StorageFull,
            Some(1),
        );
        assert_eq!(
            writer.deliver(&piece("original", "original bytes")).await,
            Step::Stopped
        );
        let cause = writer.stop_cause().cloned().unwrap();
        assert_eq!(cause.unsent.as_ref().unwrap().serial(), 1);
        assert!(harness.port.posts().is_empty());
        drop(planted);
        drop(writer);
        let era = harness.store.read_era().unwrap().unwrap();
        let channel = harness
            .store
            .open_channel_withdrawing(&era, CHANNEL, cause.unsent.as_ref())
            .unwrap()
            .unwrap();
        assert_eq!(channel.ledger().approval(0), Some(&approval));
        let mut writer = ChannelWriter::new(
            channel,
            harness.gate.clone(),
            harness.port.clone(),
            harness.lease.clone(),
            harness.alarms.clone(),
        );
        assert_eq!(
            writer.deliver(&piece("original", "original bytes")).await,
            Step::Done
        );
        assert_eq!(harness.port.posts(), ["original bytes"], "{keep:?}");
    }
}

#[tokio::test]
async fn operator_resume_schema_and_existing_ledger_violations_stay_stopped() {
    let (harness, _) = approved();
    let mut writer = harness.writer();
    assert_eq!(
        writer
            .deliver(&Derived::Blocked {
                reason: "unplaceable source".into()
            })
            .await,
        Step::Stopped
    );
    assert_eq!(
        writer.deliver(&piece("original", "original bytes")).await,
        Step::Stopped
    );
    assert_eq!(
        writer.store().ledger().approval(0).unwrap().consumed_serial,
        None
    );
    assert!(matches!(
        writer.stop_cause().unwrap().alarm,
        WriterAlarm::SchemaBlocked { .. }
    ));
    assert!(harness.port.posts().is_empty());
    drop(writer);
    let mut channel = harness.channel();
    channel
        .append_ledger(LedgerEntry::Posted {
            serial: 9,
            msg_id: 50,
        })
        .unwrap();
    drop(channel);
    let writer = harness.writer();
    assert!(matches!(
        writer.stop_cause().unwrap().alarm,
        WriterAlarm::LedgerViolation { .. }
    ));
    assert!(harness.port.posts().is_empty());
}
