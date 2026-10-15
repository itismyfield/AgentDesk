use super::*;
use crate::services::tui_o::store::ledger::LocalRetryMark;

async fn not_found(harness: &Harness) {
    for _ in 0..4000 {
        if harness
            .alarms
            .raised
            .lock()
            .unwrap()
            .iter()
            .any(|alarm| matches!(alarm, WriterAlarm::NotFound { .. }))
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("root never reached NotFound");
}

fn spawn_at(mut writer: Writer, utc: DateTime<Utc>) -> (watch::Sender<bool>, Actor) {
    let bindings = startup_log(&mut writer);
    let (stop, stopped) = watch::channel(false);
    let task = tokio::spawn(crate::services::tui_o::writer::actor::run_channel_at(
        writer,
        ShadowProvider::Claude,
        bindings,
        stopped,
        utc,
    ));
    (stop, task)
}

#[tokio::test(start_paused = true)]
async fn local_retry_actor_gc_and_restart_use_ledger_without_the_original_spool() {
    let body = row("m0", "before");
    let (harness, path, source) = switched_over(&body);
    harness.gate.acquired();
    harness.port.say(BOT + 1, "readable");
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Unsent);
    append(&path, &row("m1", "ledger survives GC"));
    let mut store = harness.channel();
    let mut capture = SourceCapture::open(source.clone(), body.len() as u64).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("capture failed");
    };
    store.append_spool(&batch, &capture.prefix_hash()).unwrap();
    let used = store.spool_bytes();
    store.set_limits_for_test(1, used);
    append(
        &path,
        b"{\"type\":\"user\",\"uuid\":\"ignored\",\"message\":{\"content\":[]}}\n",
    );
    let (stop, task) = spawn_at(
        writer_over(&harness, store),
        Utc::now() - chrono::Duration::seconds(30),
    );
    not_found(&harness).await;
    polls(2).await;
    halt(stop, task).await;
    let mut store = harness.channel();
    assert!(
        !store.ledger().gc_segments(&source).is_empty(),
        "the payload segment was actually collected"
    );
    assert_eq!(
        store.ledger().piece(0).unwrap().payload,
        "ledger survives GC"
    );
    let at = store.ledger().piece(0).unwrap().outcome_at.unwrap();
    store
        .for_each_frame(&source, |frame| {
            if let crate::services::tui_o::store::spool::SpoolFrame::Record(record) = frame {
                assert!(!String::from_utf8_lossy(&record.line).contains("ledger survives GC"));
            }
        })
        .unwrap();
    drop(store);
    let (stop, task) = spawn_at(harness.writer(), at);
    polls(29).await;
    assert_eq!(harness.port.posts(), ["ledger survives GC"]);
    polls(4).await;
    assert_eq!(
        harness.port.posts(),
        ["ledger survives GC", "ledger survives GC"]
    );
    halt(stop, task).await;
    for _ in 0..3 {
        let (stop, task) = spawn(harness.writer());
        polls(70).await;
        halt(stop, task).await;
    }
    assert_eq!(harness.port.posts().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn local_retry_actor_stop_during_delay_posts_zero_and_next_lifecycle_resumes() {
    let (harness, _, _) = switched_over(b"");
    harness.gate.acquired();
    let mut store = harness.channel();
    store
        .append_ledger(LedgerEntry::Prepared {
            serial: 0,
            unit_key: unit("root"),
            piece_index: 0,
            payload: "restart".into(),
            anchor_id: 100,
            epoch: 1,
            local_retry: Some(LocalRetryMark::Budget),
        })
        .unwrap();
    store
        .append_ledger(LedgerEntry::NotFound { serial: 0 })
        .unwrap();
    let (stop, task) = spawn(writer_over(&harness, store));
    polls(10).await;
    halt(stop, task).await;
    polls(70).await;
    assert!(harness.port.posts().is_empty());
    let (stop, task) = spawn(harness.writer());
    polls(33).await;
    assert_eq!(harness.port.posts(), ["restart"]);
    polls(70).await;
    assert_eq!(harness.port.posts(), ["restart"]);
    halt(stop, task).await;
}
