use super::*;
use crate::services::process::reaping::CommandReapExt;
use crate::services::process::reaping::tests::observe_output_join;
use crate::services::process::{
    ProcessIdentity, configure_child_process_group, kill_child_tree,
    kill_pid_tree_if_identity_matches,
};
use crate::services::provider::cancel_token_cleanup::executor::with_real_pid_kill;
use crate::services::provider::{
    CancelToken, cancel_requested, register_child_pid, spawn_cancel_watchdog,
};
use crate::test_support::wait::{join_within, recv_within};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CAP: Duration = Duration::from_secs(5);

fn state(pid: u32) -> Option<char> {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => Some(stat.rsplit_once(") ").unwrap().1.chars().next().unwrap()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("read fixture process: {error}"),
    }
}

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + CAP;
    while !condition() {
        assert!(Instant::now() < deadline, "fixture handshake timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct Descendant {
    pid: u32,
    identity: ProcessIdentity,
}

impl Drop for Descendant {
    fn drop(&mut self) {
        kill_pid_tree_if_identity_matches(self.pid, self.identity);
    }
}

#[test]
fn cancel_after_stdout_eof_still_terminates_the_stderr_descendant() {
    let root = tempfile::tempdir().unwrap();
    let mut command = Command::new("python3");
    command
        .args([
            "-c",
            r#"
import pathlib, subprocess, sys
root = pathlib.Path(sys.argv[1])
assert sys.stdin.readline() == 'registered\n'
descendant = subprocess.Popen([sys.executable, '-c', '''
import pathlib, sys, time
sys.stderr.write('descendant stderr\\n')
sys.stderr.flush()
time.sleep(20)
pathlib.Path(sys.argv[1], 'natural-exit').touch()
''', str(root)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
(root / 'descendant.pid').write_text(str(descendant.pid) + '\n')
sys.exit(7)
"#,
        ])
        .arg(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_child_process_group(&mut command);
    let mut child = command.spawn_reaping().unwrap();
    let pid = child.id();
    let token = Arc::new(CancelToken::new());
    register_child_pid(Some(&token), pid);
    child.track_cancel_token(Some(&token));
    assert!(token.captured_child_process().unwrap().identity.is_some());
    assert!(token.replay_attempt().is_none());
    let tap = RawTap::for_codex(Some(&token));
    assert!(!tap.active());
    let watchdog = with_real_pid_kill(|| {
        spawn_cancel_watchdog(Some(token.clone()), "codex-direct-reap-test").unwrap()
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"registered\n")
        .unwrap();

    let pid_path = root.path().join("descendant.pid");
    let mut descendant_pid = None;
    until(|| {
        descendant_pid = std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|text| text.strip_suffix('\n')?.parse::<u32>().ok());
        descendant_pid.is_some()
    });
    let descendant = Descendant {
        pid: descendant_pid.unwrap(),
        identity: ProcessIdentity::capture(descendant_pid.unwrap()),
    };
    assert!(descendant.identity.persisted_starttime().is_some());
    // Observe the dead leader without consuming the PID reserved for cancellation.
    until(|| state(pid) == Some('Z'));
    assert!(!matches!(state(descendant.pid), None | Some('Z' | 'X')));
    let mut stdout = libc::pollfd {
        fd: child.stdout.as_ref().unwrap().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut stdout, 1, 0) }, 1);
    assert_ne!(stdout.revents & libc::POLLHUP, 0, "stdout must be at EOF");

    let (joining, joined) = mpsc::channel();
    let (completed, completion) = mpsc::channel();
    let (sender, messages) = mpsc::channel();
    let reader_token = token.clone();
    let reader = std::thread::spawn(move || {
        observe_output_join(joining);
        let result = read(child, sender, None, tap, |child| {
            if cancel_requested(Some(&reader_token)) {
                kill_child_tree(child);
                true
            } else {
                false
            }
        });
        completed.send(result).unwrap();
    });
    recv_within(&joined, CAP);
    let deadline = Instant::now() + Duration::from_secs(3);
    token.publish_cancel("manual_cancel");
    let mut result = None;
    let mut terminated = false;
    while Instant::now() < deadline {
        if result.is_none() {
            result = completion.try_recv().ok();
        }
        terminated = matches!(state(descendant.pid), None | Some('Z' | 'X'));
        if terminated && result.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let returned = result.is_some();
    let descendant_state = state(descendant.pid);
    // Cleanup follows the oracle, so a missed watchdog dispatch cannot make it pass.
    drop(descendant);
    join_within(reader, CAP);
    drop(watchdog);
    assert!(!root.path().join("natural-exit").exists());
    assert!(
        terminated && returned,
        "cancel missed stderr descendant: state={descendant_state:?}, reader_returned={returned}"
    );
    result.unwrap().unwrap();
    assert!(messages.try_iter().any(|message| matches!(
        message,
        StreamMessage::Error {
            exit_code: Some(7),
            ..
        }
    )));
}
