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
fn b2_seq_admission_is_read_only_and_refuses_missing_or_unhealthy_log() {
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
