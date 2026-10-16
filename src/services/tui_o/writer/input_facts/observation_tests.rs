use super::resume_tests::{bind, blocked, event, header, lines};
use super::*;
use crate::services::tui_o::shadow::capture::io_trace;

#[test]
fn s1_checked_resume_rejects_mutation_between_guard_read_and_open() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    lines(
        &path,
        &[
            header(root.path()),
            event("task_started", "A"),
            blocked(),
            event("task_complete", "A"),
        ],
    );
    let mut facts = InputFacts::open_observation_at(bind(&path, ShadowProvider::Codex), 0).unwrap();
    assert!(facts.poll(u64::MAX).is_err());
    assert!(facts.resume_point().is_some());
    let changed = path.clone();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    io_trace::after_read_at(0, move || {
        let mut bytes = std::fs::read(&changed).unwrap();
        let at = bytes.windows(3).position(|w| w == b"\"A\"").unwrap();
        bytes[at + 1] = b'B';
        std::fs::write(changed, bytes).unwrap();
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    assert!(
        facts.resume_observation_checked().is_err(),
        "post-open continuity check accepted changed opener bytes"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn s0o_observation_records_filtered_native_boundaries_even_before_block() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    lines(&path, &[header(root.path())]);
    let offset = std::fs::metadata(&path).unwrap().len();
    lines(
        &path,
        &[
            event("task_started", "A"),
            event("task_complete", "B"),
            blocked(),
            event("task_complete", "A"),
        ],
    );
    let binding = bind(&path, ShadowProvider::Codex);
    io_trace::take();
    let mut facts = InputFacts::open_observation_at(binding.clone(), offset).unwrap();
    assert!(
        io_trace::take().iter().all(|(_, _, hash)| !hash),
        "observation open hashed the prefix"
    );
    assert!(facts.poll(u64::MAX).is_err());
    assert_eq!(facts.boundaries().len(), 1);
    assert_eq!(facts.boundaries()[0].native_turn_id.as_deref(), Some("A"));
    let resume = facts.resume_point().cloned().unwrap();
    facts = InputFacts::resume_observation(binding, &resume).unwrap();
    assert!(
        io_trace::take().iter().all(|(_, _, hash)| !hash),
        "observation resume hashed the prefix"
    );
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Idle);
    assert_eq!(facts.boundaries().len(), 1);
    assert_eq!(facts.boundaries()[0].kind, BoundaryKind::Close);
    assert_eq!(facts.boundaries()[0].native_turn_id.as_deref(), Some("A"));
    assert!(io_trace::take().iter().all(|(_, _, hash)| !hash));
    facts.poll(u64::MAX).unwrap();
    assert!(facts.boundaries().is_empty());
}

#[test]
fn s0o_full_equivalence_includes_events_resume_and_durable_prefix() {
    use crate::services::tui_o::shadow::capture::SourceCapture;
    use sha2::{Digest, Sha256};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    lines(
        &path,
        &[
            header(root.path()),
            event("task_started", "A"),
            blocked(),
            event("task_complete", "A"),
            event("task_complete", "A"),
        ],
    );
    let binding = bind(&path, ShadowProvider::Codex);
    for budget in [1, 71, u64::MAX] {
        let mut full = InputFacts::open(binding.clone()).unwrap();
        let mut observed = InputFacts::open_observation_at(binding.clone(), 0).unwrap();
        for _ in 0..1000 {
            assert_eq!(full.poll(budget), observed.poll(budget));
            assert_eq!(full.events(), observed.events());
            assert_eq!(full.resume_point(), observed.resume_point());
            assert!(full.boundaries().is_empty());
            if let Some(resume) = full.resume_point().cloned() {
                full = InputFacts::resume(binding.clone(), &resume).unwrap();
                observed = InputFacts::resume_observation(binding.clone(), &resume).unwrap();
            }
            if full.caught_up().unwrap() {
                break;
            }
        }
        assert!(full.caught_up().unwrap());
    }
    let bytes = std::fs::read(&path).unwrap();
    let through = bytes.len() as u64;
    let hash = hex::encode(Sha256::digest(&bytes));
    let capture = SourceCapture::reopen(binding.source.clone(), through, &hash).unwrap();
    assert_eq!(capture.prefix_hash(), hash);
    assert!(capture.verify_eof(through, &hash).is_ok());
    let mut changed = bytes;
    changed[1] ^= 1;
    std::fs::write(&path, changed).unwrap();
    assert!(!capture.verify_prefix().unwrap());
    assert!(SourceCapture::reopen(binding.source, through, &hash).is_err());
}
