use super::super::tests::fixture;
use super::*;

struct Broken;
impl Write for Broken {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("injected sink failure"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn n1_sink_write_failure_marks_health_and_disconnects_without_propagation() {
    let (observer, rx) = fixture(16);
    observer.emit(
        "codex",
        7,
        super::super::Kind::ModeConfirmed {
            confirmation_window_start: None,
            actual_confirmed_at_us: None,
            boundary_rule: "unknown",
        },
    );
    consume(&observer, rx, Broken);
    assert_eq!(observer.health().1, 1);
    observer.emit(
        "codex",
        7,
        super::super::Kind::ModeConfirmed {
            confirmation_window_start: None,
            actual_confirmed_at_us: None,
            boundary_rule: "unknown",
        },
    );
    assert_eq!(observer.health().0, 1);
}

#[test]
fn n1_sink_open_and_rotation_failures_are_observation_only() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, b"block directory").unwrap();
    assert!(Log::open(&blocker).is_err());
    let mut log = Log::open(dir.path()).unwrap();
    std::fs::create_dir(log.path.with_extension("jsonl.3")).unwrap();
    assert!(log.rotate().is_err());
}

#[test]
fn n1_sink_rotation_keeps_complete_jsonl_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path()).unwrap();
    log.bytes = FILE_CAP;
    log.write_all(b"{\"event\":\"one\"}").unwrap();
    log.write_all(b"\n").unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("n1-observation.jsonl.1")).unwrap(),
        b"{\"event\":\"one\"}\n"
    );
    assert_eq!(std::fs::read(&log.path).unwrap(), b"");
}
