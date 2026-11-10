use super::*;
use crate::services::tui_o::store::ChannelStore;

fn frames(
    store: &mut ChannelStore,
    source: &SourceId,
) -> Vec<(crate::services::tui_o::store::spool::SpoolFrame, u32)> {
    let mut frames = Vec::new();
    store
        .for_each_frame(source, |frame, version| frames.push((frame, version)))
        .unwrap();
    frames
}

fn bind(seq: u64, old: Option<&SourceId>, new: &SourceId) -> Vec<u8> {
    let mut event: p5::BindingEvent = serde_json::from_slice(&logged(seq, old, new)).unwrap();
    event.provider = "codex".into();
    let mut line = serde_json::to_vec(&event).unwrap();
    line.push(b'\n');
    line
}

async fn until(predicate: impl Fn() -> bool) -> bool {
    for _ in 0..5000 {
        if predicate() {
            return true;
        }
        tokio::time::advance(POLL_INTERVAL).await;
        tokio::task::yield_now().await;
    }
    predicate()
}

async fn rebind(dormant: bool) {
    let config = serde_json::from_value(serde_json::json!({
        "server": {}, "tui_o": {"writer": {"channels": [CHANNEL],
            "dormant_channels": if dormant { vec![CHANNEL] } else { vec![] }}},
        "agents": [{"id": "archived", "name": "Archived", "channels": {
            "codex": {"id": CHANNEL.to_string(), "runtime": "tui"}}}]
    }))
    .unwrap();
    channel_policy::install(&config).unwrap();
    crate::config_live_reload::install(config);
    let (h, path, old) = switched_over(b"");
    h.gate.acquired();
    append(
        &path,
        b"{\"type\":\"progress\",\"uuid\":\"retained-archive-evidence\"}\n",
    );
    let mut capture = SourceCapture::open(old.clone(), 0).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("capture evidence")
    };
    let mut store = h.channel();
    store.append_spool(&batch, &capture.prefix_hash()).unwrap();
    store.set_binding_checkpoint(1).unwrap();
    let cursor = store.cursor(&old).unwrap().clone();
    let retained = frames(&mut store, &old);
    assert!(!retained.is_empty());
    drop((capture, store));
    std::fs::write(path.with_extension("jsonl.zst"), b"existence only").unwrap();
    std::fs::remove_file(&path).unwrap();
    let first = bind(1, None, &old);
    p5_log(h._runtime.path(), CHANNEL, &first);
    let _selected = test_override::force_channels(&[(CHANNEL, CodexTui)]);
    let io = TestIo::over(&h);
    let ready = Arc::new(Readiness::default());
    let tasks = hosted_as(ShadowProvider::Codex, &h, &io, (true, &ready));
    assert_eq!(tasks.len(), 1);
    assert!(
        until(|| !io.alarms.halted().is_empty()).await,
        "current archive without EOF proof must halt"
    );
    assert!(!ready.accepts(CHANNEL));
    assert_eq!(h.channel().cursor(&old).unwrap(), &cursor);
    let fresh_path = path.with_file_name("fresh.jsonl");
    std::fs::write(&fresh_path, concat!(
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"fresh\"}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"fresh-answer\",\"content\":[{\"type\":\"output_text\",\"text\":\"fresh reply\"}]}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"fresh\"}}\n",
    )).unwrap();
    let fresh = source_id_for("fresh", &fresh_path).unwrap();
    p5_log(
        h._runtime.path(),
        CHANNEL,
        &[first, bind(2, Some(&old), &fresh)].concat(),
    );
    p5::forget_channel_for_tests(CHANNEL);
    let resumed = until(|| {
        ready.accepts(CHANNEL)
            && h.port
                .posts()
                .iter()
                .filter(|body| *body == "fresh reply")
                .count()
                == 1
    })
    .await;
    assert!(
        resumed,
        "historical archive must resume B; ready={} leases={} posts={:?} alarms={:?} bindings={:?}",
        ready.accepts(CHANNEL),
        io.leases.lock().unwrap().len(),
        h.port.posts(),
        io.alarms.0.lock().unwrap(),
        ChannelBindingLog::new(CHANNEL, ShadowProvider::Codex).binding_events_since(CHANNEL, 0)
    );
    tokio::time::advance(POLL_INTERVAL * 3).await;
    tokio::task::yield_now().await;
    let posts = h.port.posts();
    assert_eq!(
        posts.iter().filter(|body| *body == "fresh reply").count(),
        1
    );
    assert_eq!(posts.first().map(String::as_str), Some("fresh reply"));
    assert_eq!(
        io.leases.lock().unwrap().len(),
        2,
        "one retry in the original host"
    );
    assert!(
        io.alarms
            .1
            .lock()
            .unwrap()
            .iter()
            .any(|(_, state)| *state == "cleared")
    );
    let mut store = h.channel();
    assert_eq!(store.cursor(&old).unwrap(), &cursor);
    assert!(!store.cursor(&old).unwrap().retired);
    assert_eq!(frames(&mut store, &old), retained);
    assert_eq!(
        store.cursor(&fresh).unwrap().captured_through,
        std::fs::metadata(fresh_path).unwrap().len()
    );
    abort(tasks);
}

#[tokio::test(start_paused = true)]
async fn archived_current_halt_rebind_resumes_same_host_legacy() {
    if child_case(concat!(
        module_path!(),
        "::archived_current_halt_rebind_resumes_same_host_legacy"
    )) {
        rebind(false).await;
    }
}

#[tokio::test(start_paused = true)]
async fn archived_current_halt_rebind_resumes_same_host_readers() {
    if child_case(concat!(
        module_path!(),
        "::archived_current_halt_rebind_resumes_same_host_readers"
    )) {
        rebind(true).await;
    }
}
