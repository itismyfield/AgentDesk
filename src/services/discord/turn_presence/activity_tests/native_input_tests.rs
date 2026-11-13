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
    let initial = native_input::snapshot(&lock(&probe.watch), &history)
        .unwrap()
        .unwrap();
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
    let observed = native_input::snapshot(&lock(&probe.watch), &history)
        .unwrap()
        .unwrap();
    assert_eq!(observed.evidence.len(), 1);
    assert_eq!(observed.evidence[0].membership_row.as_deref(), Some("own"));
    assert_eq!(observed.evidence[0].ticket, f.ticket);
    assert!(observed.revision > initial.revision);
    assert!(observed.through > initial.through);
    assert_eq!(observed.binding_seq, 1);
    assert!(
        history.json().unwrap()["live"][0]["registration"]["native_settlement"]["outcome"]
            .is_null()
    );
    *lock(&probe.fake.seq) = Ok(2);
    let _ = probe.ask();
    assert_eq!(probe.run_jobs(), 1);
    let replay = native_input::snapshot(&lock(&probe.watch), &history)
        .unwrap()
        .unwrap();
    assert!(replay.generation > observed.generation);
    assert_eq!(replay.binding_seq, 2);
    assert_eq!(replay.evidence, observed.evidence);
    let unknown = InputFacts::open_observation_at(observed.binding.clone(), 0).unwrap();
    let mut unknown = unknown;
    assert!(
        unknown.observe_native().is_err(),
        "tail/observation has no full prefix authority"
    );
}
