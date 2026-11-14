use super::*;
use crate::services::tui_input::human_frame::history::v2::{Registry, native_tests::Fixture};

#[test]
fn native_8148_b2_worker_effect_poll_and_rebuild_share_stamped_candidates() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let path = runtime.path().join("runtime/injected_frames.json");
    let f = Fixture::at(path, CHANNEL, false);
    let source = f.ticket.source.as_ref().unwrap();
    std::fs::write(&source.transcript, "").unwrap();
    let source_id = SourceId {
        session_id: source.session_id.clone(),
        path: source.transcript.clone(),
        dev: source.file[0],
        ino: source.file[1],
    };
    write(
        &source.transcript,
        &[
            json!({"type":"user","uuid":"outside","sessionId":"native","message":{"content":"scheduled task"}}),
            json!({"type":"queue-operation","operation":"enqueue","content":"한국어 원본 입력"}),
        ],
    );
    let probe = Probe::new(
        ShadowProvider::Claude,
        vec![bound(1, SESSION, &source_id, ShadowProvider::Claude)],
    );
    assert_eq!(probe.ask().1, "catching_up");
    assert_eq!(probe.run_jobs(), 1);
    let history = Registry::runtime().unwrap();
    let mut reader = native_input::Reader::new(
        SourceBinding {
            channel_id: CHANNEL,
            provider: ShadowProvider::Claude,
            source: source_id.clone(),
        },
        history.clone(),
    );
    let initial = reader.poll(u64::MAX).unwrap().unwrap();
    assert!(initial.evidence.is_empty());
    write(
        &source.transcript,
        &[
            end(),
            json!({"type":"queue-operation","operation":"dequeue"}),
            json!({"type":"user","uuid":"own","sessionId":"native","message":{"content":"한국어 원본 입력"}}),
            end(),
        ],
    );
    let _ = probe.ask();
    let observed = reader.poll(u64::MAX).unwrap().unwrap();
    assert_eq!(observed.evidence.len(), 1);
    assert_eq!(observed.evidence[0].membership_row.as_deref(), Some("own"));
    assert_eq!(observed.evidence[0].ticket, f.ticket);
    assert!(observed.revision > initial.revision);
    assert!(observed.through > initial.through);
    assert_eq!(observed.binding_seq, 0);
    assert!(
        history.json().unwrap()["live"][0]["registration"]["native_settlement"]["outcome"]
            .is_null()
    );
    let previous_generation = lock(&probe.watch).generation;
    *lock(&probe.fake.seq) = Ok(2);
    let _ = probe.ask();
    assert_eq!(probe.run_jobs(), 1);
    assert!(lock(&probe.watch).generation > previous_generation);
    assert_eq!(lock(&probe.watch).key.as_ref().unwrap().seq, 2);
    let mut rebuilt = native_input::Reader::new(observed.binding.clone(), history.clone());
    let replay = rebuilt.poll(u64::MAX).unwrap().unwrap();
    assert_eq!(replay.generation, observed.generation);
    assert_eq!(replay.binding_seq, observed.binding_seq);
    assert_eq!(
        replay.registrations_fingerprint,
        observed.registrations_fingerprint
    );
    assert_eq!(replay.evidence, observed.evidence);
    let unknown = InputFacts::open_observation_at(observed.binding.clone(), 0).unwrap();
    let mut unknown = unknown;
    assert!(
        unknown.observe_native().is_err(),
        "tail/observation has no full prefix authority"
    );
}

#[test]
fn native_8148_2a_rebuild_does_not_open_history_lock() {
    use crate::test_support::wait::{WAIT_CAP, join_within, recv_within};
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let path = runtime.path().join("runtime/injected_frames.json");
    let f = Fixture::at(path.clone(), CHANNEL, false);
    let source = f.ticket.source.as_ref().unwrap();
    let id = SourceId {
        session_id: source.session_id.clone(),
        path: source.transcript.clone(),
        dev: source.file[0],
        ino: source.file[1],
    };
    std::fs::write(
        &source.transcript,
        format!("{}\n", json!({"type":"user","message":{"content":"plain"}})),
    )
    .unwrap();
    let mut v = f.history.json().unwrap();
    v["version"] = json!(2);
    v["live"][0]["registration"]
        .as_object_mut()
        .unwrap()
        .remove("native_settlement");
    std::fs::write(&path, v.to_string()).unwrap();
    let (ready, rx) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))
            .unwrap();
        file.lock().unwrap();
        ready.send(()).unwrap();
        recv_within(&wait, WAIT_CAP);
    });
    recv_within(&rx, WAIT_CAP);
    let probe = Probe::new(
        ShadowProvider::Claude,
        vec![bound(1, SESSION, &id, ShadowProvider::Claude)],
    );
    assert_eq!(probe.ask().1, "catching_up");
    assert_eq!(probe.run_jobs(), 1);
    release.send(()).unwrap();
    join_within(thread, WAIT_CAP);
}

#[test]
fn native_8148_2a_independent_reader_survives_activity_tail_and_small_polls() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::at(
        runtime.path().join("runtime/injected_frames.json"),
        CHANNEL,
        false,
    );
    let source = f.ticket.source.as_ref().unwrap();
    let _limits = small_tail_limits(CHANNEL);
    std::fs::write(&source.transcript, "").unwrap();
    let id = SourceId {
        session_id: source.session_id.clone(),
        path: source.transcript.clone(),
        dev: source.file[0],
        ino: source.file[1],
    };
    let mut rows = vec![
        json!({"type":"assistant","uuid":"prefix","apiBlockIndex":0,"message":{"id":"prefix","content":[{"type":"text","text":"prefix".repeat(1000)}]}}),
    ];
    rows.extend([json!({"type":"queue-operation","operation":"enqueue","content":"한국어 원본 입력"}),json!({"type":"queue-operation","operation":"dequeue"}),json!({"type":"user","uuid":"own","sessionId":"native","message":{"content":"한국어 원본 입력"}}),end()]);
    write(&source.transcript, &rows);
    let probe = Probe::new(
        ShadowProvider::Claude,
        vec![bound(1, SESSION, &id, ShadowProvider::Claude)],
    );
    let _ = probe.ask();
    assert_eq!(probe.run_jobs(), 1);
    let mut reader = native_input::Reader::new(
        SourceBinding {
            channel_id: CHANNEL,
            provider: ShadowProvider::Claude,
            source: id,
        },
        f.history.clone(),
    );
    let mut observed = None;
    for _ in 0..128 {
        if let Some(batch) = reader.poll(512).unwrap() {
            observed = Some(batch);
            break;
        }
    }
    let batch = observed.expect("bounded replay reaches EOF independently of tail");
    assert_eq!(batch.evidence.len(), 1);
    assert_eq!(batch.evidence[0].membership_row.as_deref(), Some("own"));
    assert!(batch.through > 4096);
}
