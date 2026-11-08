use super::*;
use crate::services::tui_o::writer::ReaderConfig;
use crate::services::tui_o::writer::actor::{Owing, spawn_projecting};
use crate::services::tui_o::writer::pieces::UnitDeriver;

fn missing_codex_source(unsealed: bool) -> (Harness, Writer) {
    let (harness, path, source) = switched_over(b"");
    let value = if unsealed {
        serde_json::json!({"type":"event_msg", "payload":{
            "type":"item_completed", "item":{"type":"AgentMessage", "id":"waiting-body"}}})
    } else {
        serde_json::json!({"type":"event_msg", "payload":{"type":"task_complete"}})
    };
    append(&path, format!("{value}\n").as_bytes());
    let mut capture = SourceCapture::open(source, 0).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("fixture source must be captured before it disappears");
    };
    let mut sanity = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
    for record in &batch.records {
        assert!(sanity.derive(record).is_empty());
    }
    assert_eq!(sanity.has_unsealed(), unsealed);
    let mut store = harness.channel();
    store.append_spool(&batch, &capture.prefix_hash()).unwrap();
    store.set_binding_checkpoint(0).unwrap();
    std::fs::remove_file(path).unwrap();
    harness.gate.acquired();
    let writer = writer_over(&harness, store);
    (harness, writer)
}

#[tokio::test(start_paused = true)]
async fn sessionless_latest_missing_codex_source_with_unsealed_work_stays_halted() {
    for readers in [None, Some(ReaderConfig { dormant: true })] {
        let (harness, writer) = missing_codex_source(true);
        let config = serde_json::from_value(serde_json::json!({
            "enabled": true, "producer_absent": true
        }))
        .unwrap();
        let (stop, stopped) = watch::channel(false);
        let (resumed, ready) = watch::channel(false);
        let task = spawn_projecting(
            &config,
            readers,
            writer,
            ShadowProvider::Codex,
            Arc::new(FakeBindings::new()),
            (stopped, resumed, watch::channel(None).0, Owing::default()),
        )
        .unwrap();
        polls(3).await;
        assert!(
            !*ready.borrow(),
            "an unsealed latest source must not publish resumed readiness"
        );
        assert!(
            task.is_finished(),
            "unsealed work must retain the existing halt"
        );
        let cause = task
            .await
            .unwrap()
            .expect("a halted actor reports its stop cause");
        assert!(matches!(
            cause.alarm,
            WriterAlarm::HaltedMissingSource { .. }
        ));
        assert!(harness.port.posts().is_empty());
        assert!(harness.alarms.taken().iter().any(|alarm| matches!(
            alarm,
            WriterAlarm::Halted { .. } | WriterAlarm::HaltedMissingSource { .. }
        )));
        drop(stop);
    }
}

#[tokio::test(start_paused = true)]
async fn sessionless_latest_missing_codex_source_without_unsealed_work_resumes() {
    for readers in [None, Some(ReaderConfig { dormant: true })] {
        let (harness, writer) = missing_codex_source(false);
        let config = serde_json::from_value(serde_json::json!({
            "enabled": true, "producer_absent": true
        }))
        .unwrap();
        let (stop, stopped) = watch::channel(false);
        let (resumed, ready) = watch::channel(false);
        let task = spawn_projecting(
            &config,
            readers,
            writer,
            ShadowProvider::Codex,
            Arc::new(FakeBindings::new()),
            (stopped, resumed, watch::channel(None).0, Owing::default()),
        )
        .unwrap();
        polls(3).await;
        assert!(
            *ready.borrow(),
            "a sessionless latest source with no unsealed work must park and resume"
        );
        assert!(!task.is_finished(), "{:?}", harness.alarms.taken());
        assert!(harness.alarms.taken().is_empty());
        assert!(harness.port.posts().is_empty());
        halt(stop, task).await;
    }
}
