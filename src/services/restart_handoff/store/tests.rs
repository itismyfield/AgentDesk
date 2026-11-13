use super::*;
use crate::test_support::wait::{WAIT_CAP, join_within, recv_within};

fn context(message: Option<u64>) -> RestartReportContext {
    RestartReportContext::from_bridge(
        ProviderKind::Claude,
        42,
        message,
        Some("adk-restart".into()),
    )
}
fn metadata(generation: u64) -> RestartMetadata {
    RestartMetadata {
        generation,
        doctor_summary: serde_json::json!({"generation": generation}),
    }
}

#[cfg(unix)]
fn assert_locked(path: &Path) {
    use std::os::fd::AsRawFd;
    let contender = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("json.lock"))
        .unwrap();
    assert_ne!(
        unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert_eq!(
        std::io::Error::last_os_error().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[cfg(unix)]
#[test]
fn metadata_is_sampled_with_the_record_flock_held() {
    let root = tempfile::tempdir().unwrap();
    let ctx = context(Some(7));
    let path = restart_report_path(root.path(), &ctx.provider, ctx.channel_id);
    let calls = std::cell::Cell::new(0);
    let source = || {
        assert_locked(&path);
        calls.set(calls.get() + 1);
        metadata(41)
    };
    let claim = announce_restart_in_root(root.path(), &ctx, &source).unwrap();
    assert_eq!(calls.get(), 1);
    let execution =
        prepare_restart_handoff_in_root(root.path(), root.path(), &ctx, Some(&claim), &source)
            .unwrap();
    assert_eq!(calls.get(), 2);
    let stored = load_report(&restart_attempt_report_path(root.path(), &ctx, &execution))
        .unwrap()
        .unwrap();
    assert_eq!(stored.generation, 41);
    assert_eq!(
        stored.doctor_summary,
        Some(serde_json::json!({"generation": 41}))
    );
}

#[cfg(unix)]
#[test]
fn two_writers_share_the_flock_and_preserve_predecessor_fields() {
    let root = tempfile::tempdir().unwrap();
    let first_root = root.path().to_owned();
    let second_root = first_root.clone();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let first = std::thread::spawn(move || {
        let ctx = context(Some(7));
        let path = restart_report_path(&first_root, &ctx.provider, ctx.channel_id);
        announce_restart_in_root(&first_root, &ctx, &|| {
            assert_locked(&path);
            entered_tx.send(()).unwrap();
            recv_within(&release_rx, WAIT_CAP);
            metadata(11)
        })
        .unwrap()
    });
    recv_within(&entered_rx, WAIT_CAP);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let second = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let ctx = context(None);
        let path = restart_report_path(&second_root, &ctx.provider, ctx.channel_id);
        announce_restart_in_root(&second_root, &ctx, &|| {
            assert_locked(&path);
            metadata(12)
        })
        .unwrap()
    });
    recv_within(&started_rx, WAIT_CAP);
    release_tx.send(()).unwrap();
    let a = join_within(first, WAIT_CAP);
    let b = join_within(second, WAIT_CAP);
    assert_ne!(a, b);
    let path = restart_report_path(root.path(), &ProviderKind::Claude, 42);
    let current = load_report(&path).unwrap().unwrap();
    assert_eq!(current.attempt, b);
    assert_eq!(current.current_msg_id, Some(7));
    assert_eq!(current.generation, 12);
    assert_eq!(compare_delete(&path, &a).unwrap(), SettleOutcome::Stale);
    assert_eq!(
        settle_restart_path(&path, &a, RestartReportState::Succeeded).unwrap(),
        SettleOutcome::Stale
    );
    assert_eq!(load_report(&path).unwrap().unwrap().attempt, b);
}
