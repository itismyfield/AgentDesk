use super::*;
use crate::services::tui_o::store::ledger::PieceDisposition;

fn seed_approval(harness: &Harness, provider: ShadowProvider) {
    let mut channel = harness.channel();
    let mut key = unit("ledger-only");
    key.channel_id = harness.channel_id;
    key.provider = provider;
    channel
        .append_ledger(LedgerEntry::Prepared {
            serial: 0,
            unit_key: key,
            piece_index: 0,
            payload: "ledger-only payload".into(),
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
    harness
        .store
        .record_operator_resume(harness.channel_id, 0, "operator", "restored")
        .unwrap();
}

fn native(provider: ShadowProvider, id: &str, text: &str) -> Vec<u8> {
    match provider {
        ShadowProvider::Claude => row(id, text),
        ShadowProvider::Codex => {
            let line = |kind: &str, payload: serde_json::Value| {
                format!("{}\n", serde_json::json!({"type":kind,"payload":payload}))
            };
            [
                line(
                    "event_msg",
                    serde_json::json!({"type":"task_started","turn_id":id}),
                ),
                line(
                    "response_item",
                    serde_json::json!({"type":"message","role":"assistant","id":id,
                    "content":[{"type":"output_text","text":text}]}),
                ),
                line(
                    "event_msg",
                    serde_json::json!({"type":"task_complete","turn_id":id}),
                ),
            ]
            .concat()
            .into_bytes()
        }
    }
}

#[tokio::test(start_paused = true)]
async fn operator_resume_middle_piece_and_following_output_survive_actor_restarts_both_providers() {
    for provider in [ShadowProvider::Claude, ShadowProvider::Codex] {
        let (harness, path, _) = switched_over(b"");
        harness.gate.acquired();
        harness
            .port
            .replies
            .lock()
            .unwrap()
            .extend([Reply::Created, Reply::Refused(403)]);
        let body = "x".repeat(4500);
        append(&path, &native(provider, "long", &body));
        append(&path, &native(provider, "later", "following unit"));
        let (_stop, task) = spawn_as(harness.writer(), provider);
        polls(3).await;
        let cause = task.await.unwrap().unwrap();
        assert_eq!(cause.alarm, WriterAlarm::Blocked { status: 403 });
        assert_eq!(harness.port.posts().len(), 2);
        let rejected = harness.channel().ledger().piece(1).unwrap().clone();
        assert_eq!(rejected.outcome, Some(PieceOutcome::Rejected(403)));
        harness
            .store
            .record_operator_resume(CHANNEL, 1, "operator", "restored")
            .unwrap();
        let (stop, task) = spawn_as(harness.writer(), provider);
        polls(3).await;
        let sent = harness.port.posts();
        let successful = [&sent[0..1], &sent[2..]].concat();
        assert_eq!(
            successful.concat(),
            format!("{body}following unit"),
            "{provider:?}"
        );
        assert_eq!(sent[1], rejected.payload);
        assert_eq!(sent[2], rejected.payload);
        let channel = harness.channel();
        assert_eq!(channel.ledger().blocked(), None);
        assert_eq!(
            channel
                .ledger()
                .disposition(&rejected.unit_key, rejected.piece_index),
            PieceDisposition::Settled
        );
        assert_eq!(
            channel.ledger().approval(1).unwrap().consumed_serial,
            Some(2)
        );
        assert!(channel.ledger().violation().is_none());
        halt(stop, task).await;
        let count = sent.len();
        let (stop, task) = spawn_as(harness.writer(), provider);
        polls(2).await;
        assert_eq!(harness.port.posts().len(), count);
        append(&path, &native(provider, "new", "new output"));
        polls(3).await;
        assert_eq!(harness.port.posts().last().unwrap(), "new output");
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn operator_resume_empty_spool_is_owed_before_wait_observation_at_300_seconds() {
    use std::time::Duration;
    let (harness, _, _) = switched_over(b"");
    harness.gate.acquired();
    seed_approval(&harness, ShadowProvider::Claude);
    harness.lease.busy.store(true, Ordering::SeqCst);
    let (alarms, stop, task) = spawn_waiting(&harness, Utc::now());
    waiting_poll(&stop, Duration::ZERO).await;
    assert!(harness.port.posts().is_empty());
    assert_eq!(harness.channel().ledger().next_serial(), 1);
    waiting_poll(&stop, Duration::from_millis(300_000)).await;
    assert!(alarms.raised.lock().unwrap().is_empty());
    waiting_poll(&stop, Duration::from_millis(1)).await;
    assert!(matches!(
        alarms.raised.lock().unwrap().as_slice(),
        [WriterAlarm::WaitingTooLong {
            ready: 1,
            prepared: None,
            ..
        }]
    ));
    harness.lease.busy.store(false, Ordering::SeqCst);
    waiting_poll(&stop, Duration::ZERO).await;
    assert_eq!(harness.port.posts(), ["ledger-only payload"]);
    assert_eq!(alarms.cleared.load(Ordering::SeqCst), 1);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn operator_resume_lease_wait_preserves_readiness_and_drain_owing() {
    use super::super::super::actor::{Demand, Owing, spawn_projecting};
    let _hosts = crate::config::session_hosts::force_for_test(Some("book"), &[(CHANNEL, "book")]);
    let (harness, _, _) = switched_over(b"");
    harness.gate.acquired();
    seed_approval(&harness, ShadowProvider::Claude);
    harness.lease.busy.store(true, Ordering::SeqCst);
    let mut writer = harness.writer();
    let bindings = startup_log(&mut writer);
    let (stop, stopped) = watch::channel(false);
    let (resumed, readiness) = watch::channel(false);
    let owing = Owing::default();
    let demand: Demand = owing.demand.clone();
    let projection = owing.published.subscribe();
    let _wanting = demand.want();
    let task = spawn_projecting(
        &WriterConfig { enabled: true },
        writer,
        ShadowProvider::Claude,
        bindings,
        (stopped, resumed, watch::channel(None).0, owing),
    )
    .unwrap();
    polls(2).await;
    assert!(
        *readiness.borrow(),
        "source recovery still makes the actor ready while approval waits"
    );
    let pending = projection.borrow().unwrap();
    assert_eq!((pending.owed, pending.prepared), (1, 0));
    assert!(
        harness
            .channel()
            .ledger()
            .approval(0)
            .unwrap()
            .consumed_serial
            .is_none()
    );
    harness.lease.busy.store(false, Ordering::SeqCst);
    polls(2).await;
    assert_eq!(harness.port.posts(), ["ledger-only payload"]);
    assert_eq!(projection.borrow().unwrap().owed, 0);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn operator_resume_open_consumed_retry_without_raw_work_settles_before_following() {
    let (harness, _, _) = switched_over(b"");
    harness.gate.acquired();
    seed_approval(&harness, ShadowProvider::Claude);
    let mut channel = harness.channel();
    let original = channel.ledger().piece(0).unwrap().clone();
    channel
        .append_ledger(LedgerEntry::Prepared {
            serial: 1,
            unit_key: original.unit_key,
            piece_index: 0,
            payload: original.payload,
            anchor_id: 100,
            epoch: 1,
        })
        .unwrap();
    harness.port.unreadable.store(true, Ordering::SeqCst);
    let (stop, task) = spawn(harness.writer());
    polls(12).await;
    let channel = harness.channel();
    assert!(matches!(
        channel.ledger().piece(1).unwrap().outcome,
        Some(PieceOutcome::Unresolved(_))
    ));
    assert!(harness.port.posts().is_empty());
    assert_eq!(channel.ledger().blocked(), None);
    assert!(channel.ledger().resume_pieces().next().is_none());
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn operator_resume_projection_does_not_overtake_a_real_schema_blocked_record() {
    let (harness, path, _) = switched_over(b"");
    seed_approval(&harness, ShadowProvider::Claude);
    let line = serde_json::json!({"type":"assistant","uuid":"ambiguous","apiBlockIndex":0,
        "message":{"id":"ambiguous","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}});
    append(&path, format!("{line}\n").as_bytes());
    let mut channel = harness.channel();
    let source = channel.cursors().next().unwrap().source.clone();
    let mut capture = SourceCapture::open(source, 0).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("capture failed")
    };
    channel
        .append_spool(&batch, &capture.prefix_hash())
        .unwrap();
    drop(channel);
    harness.gate.acquired();
    let (_stop, task) = spawn(harness.writer());
    let cause = task.await.unwrap().unwrap();
    assert!(matches!(cause.alarm, WriterAlarm::SchemaBlocked { .. }));
    assert!(harness.port.posts().is_empty());
    assert_eq!(
        harness
            .channel()
            .ledger()
            .approval(0)
            .unwrap()
            .consumed_serial,
        None
    );
}
