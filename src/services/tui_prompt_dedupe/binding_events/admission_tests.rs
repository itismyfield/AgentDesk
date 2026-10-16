use std::sync::mpsc;
use std::time::Duration;

use super::*;

struct BindingRoot(Option<PathBuf>);

impl BindingRoot {
    fn enter(root: &Path) -> Self {
        let saved = test_root();
        set_test_root(Some(root));
        Self(saved)
    }
}

impl Drop for BindingRoot {
    fn drop(&mut self) {
        set_test_root(self.0.as_deref());
    }
}

fn pending(channel: u64, path: &Path) {
    let proposal = Proposal {
        channel_id: channel,
        provider: "claude",
        tmux_session: "p5-admission",
        session_id: path.file_stem().and_then(|stem| stem.to_str()),
        path: path.to_str().unwrap(),
        replaced: None,
        cause: CauseSource::Observed,
        hook: None,
    };
    assert_eq!(record_pending(&proposal).unwrap(), PendingRecord::Recorded);
}

#[test]
fn b2_warm_seq_admission_refuses_missing_or_unhealthy_log_without_writes() {
    let root = tempfile::tempdir().unwrap();
    let _scope = BindingRoot::enter(root.path());
    let channel = 6_325_801;
    let candidate = root.path().join("parent.jsonl");
    assert_eq!(admit_committed_seq(channel, 0, || 1).unwrap(), None);
    pending(channel, &candidate);
    let path = log_path(channel).unwrap().unwrap();
    let before = fs::read(&path).unwrap();
    assert_eq!(admit_committed_seq(channel, 1, || 2).unwrap(), Some(2));
    assert_eq!(admit_committed_seq(channel, 0, || 3).unwrap(), None);
    assert_eq!(fs::read(&path).unwrap(), before);
    lock_logs()
        .get_mut(&path)
        .unwrap()
        .writer
        .as_mut()
        .unwrap()
        .tainted = true;
    assert_eq!(admit_committed_seq(channel, 1, || 4).unwrap(), None);
    lock_logs()
        .get_mut(&path)
        .unwrap()
        .writer
        .as_mut()
        .unwrap()
        .tainted = false;
    lock_logs()
        .get_mut(&path)
        .unwrap()
        .writer
        .as_mut()
        .unwrap()
        .poisoned = true;
    assert_eq!(admit_committed_seq(channel, 1, || 5).unwrap(), None);
}

#[test]
fn b2_first_poll_handoff_and_real_commit_share_the_log_mutex() {
    let root = tempfile::tempdir().unwrap();
    let _scope = BindingRoot::enter(root.path());
    let channel = 6_325_802;
    pending(channel, &root.path().join("a.jsonl"));
    let committed_seq = subscribe_binding_events(channel).unwrap();
    let (start, begin) = mpsc::channel();
    let (done, committed) = mpsc::channel();
    let root_path = root.path().to_path_buf();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let _scope = BindingRoot::enter(&root_path);
            begin.recv_timeout(Duration::from_secs(5)).unwrap();
            pending(channel, &root_path.join("b.jsonl"));
            done.send(()).unwrap();
        });
        assert_eq!(
            admit_committed_seq(channel, 1, || {
                assert!(matches!(
                    LOGS.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                start.send(()).unwrap();
                assert_eq!(*committed_seq.borrow(), 1);
                assert!(matches!(
                    committed.try_recv(),
                    Err(mpsc::TryRecvError::Empty)
                ));
                7
            })
            .unwrap(),
            Some(7)
        );
        committed.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    assert_eq!(admit_committed_seq(channel, 1, || 8).unwrap(), None);
    assert_eq!(admit_committed_seq(channel, 2, || 9).unwrap(), Some(9));
}

#[test]
fn b2b1_seq_admission_loads_a_cold_writer_before_handoff() {
    let root = tempfile::tempdir().unwrap();
    let channel = 6_325_803;
    let path = {
        let _scope = BindingRoot::enter(root.path());
        pending(channel, &root.path().join("a.jsonl"));
        let path = log_path(channel).unwrap().unwrap();
        assert_eq!(admit_committed_seq(channel, 1, || 1).unwrap(), Some(1));
        // Dropping the writer requires a disk reload before admitting its cached seq.
        lock_logs().get_mut(&path).unwrap().writer = None;
        assert_eq!(admit_committed_seq(channel, 1, || 2).unwrap(), Some(2));
        path
    };
    // A restored log reads its seq back, but no writer has loaded it yet.
    let restored = tempfile::tempdir().unwrap();
    let _scope = BindingRoot::enter(restored.path());
    let copy = restored
        .path()
        .join(path.strip_prefix(root.path()).unwrap());
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    fs::copy(&path, &copy).unwrap();
    assert_eq!(*subscribe_binding_events(channel).unwrap().borrow(), 1);
    assert_eq!(admit_committed_seq(channel, 1, || 3).unwrap(), Some(3));
    // The subsequent real commit still advances the same loaded writer.
    pending(channel, &restored.path().join("b.jsonl"));
    assert_eq!(admit_committed_seq(channel, 2, || 4).unwrap(), Some(4));
}

#[test]
fn cold_admission_uses_loaded_seq_and_rejects_load_errors_or_taint() {
    let root = tempfile::tempdir().unwrap();
    let _scope = BindingRoot::enter(root.path());
    let channel = 6_325_804;
    pending(channel, &root.path().join("a.jsonl"));
    let path = log_path(channel).unwrap().unwrap();
    let bytes = fs::read(&path).unwrap();
    forget_channel_for_tests(channel);
    APPEND_FAULT.with(|fault| fault.set(Some("reload")));
    let watch = subscribe_binding_events(channel).unwrap();
    assert_eq!(*watch.borrow(), 1);
    assert!(lock_logs().get(&path).unwrap().writer.is_none());
    let called = std::cell::Cell::new(false);
    let failed = admit_committed_seq(channel, 1, || called.set(true));
    APPEND_FAULT.with(|fault| fault.set(None));
    assert!(!called.get(), "load failure cannot hand off");
    assert!(failed.unwrap_err().to_string().contains("injected reload"));
    assert!(lock_logs().get(&path).unwrap().writer.is_none());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(admit_committed_seq(channel, 1, || 1).unwrap(), Some(1));
    {
        let mut logs = lock_logs();
        let log = logs.get_mut(&path).unwrap();
        log.writer = None;
        log.notify.send_replace(2);
    }
    assert_eq!(admit_committed_seq(channel, 2, || 2).unwrap(), None);
    assert_eq!(admit_committed_seq(channel, 1, || 3).unwrap(), Some(3));
    forget_channel_for_tests(channel);
    let mut tainted = bytes;
    tainted.extend_from_slice(b"unreadable complete record\n");
    fs::write(&path, &tainted).unwrap();
    assert_eq!(*subscribe_binding_events(channel).unwrap().borrow(), 1);
    for seq in [1, 2] {
        let admitted = admit_committed_seq(channel, seq, || called.set(true)).unwrap();
        assert!(!called.get(), "tainted log cannot hand off");
        assert_eq!(admitted, None);
    }
    assert!(!called.get());
    assert!(
        lock_logs()
            .get(&path)
            .unwrap()
            .writer
            .as_ref()
            .unwrap()
            .tainted
    );
    assert_eq!(fs::read(&path).unwrap(), tainted);
}

#[test]
fn cold_admission_recovers_only_the_uncommitted_tail() {
    let root = tempfile::tempdir().unwrap();
    let _scope = BindingRoot::enter(root.path());
    let channel = 6_325_805;
    pending(channel, &root.path().join("a.jsonl"));
    let path = log_path(channel).unwrap().unwrap();
    let bytes = fs::read(&path).unwrap();
    forget_channel_for_tests(channel);
    let mut torn = bytes.clone();
    torn.extend_from_slice(b"{\"seq\":2");
    fs::write(&path, &torn).unwrap();
    assert_eq!(*subscribe_binding_events(channel).unwrap().borrow(), 1);
    assert!(lock_logs().get(&path).unwrap().writer.is_none());
    assert_eq!(fs::read(&path).unwrap(), torn);
    assert_eq!(admit_committed_seq(channel, 1, || 1).unwrap(), Some(1));
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(binding_events_since(channel, 0).unwrap().len(), 1);
}
