use super::*;
use crate::services::process::reaping::CommandReapExt;
use crate::test_support::wait::{join_within, recv_within};
use std::process::{Command, Stdio};
use std::sync::Arc;

fn state(pid: u32) -> Option<char> {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => Some(stat.rsplit_once(") ").unwrap().1.chars().next().unwrap()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("read child state: {error}"),
    }
}

async fn wait_for_state(pid: u32, wanted: Option<char>) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while state(pid) != wanted {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("child {pid}: wanted {wanted:?}, got {:?}", state(pid)));
}

fn command(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[tokio::test]
async fn background_reap_respects_pid_dispatch_and_preserves_successor_binding() {
    for successor in [None, Some(42)] {
        let token = Arc::new(CancelToken::new());
        let mut child = command("exit 7").spawn_reaping().unwrap();
        let pid = child.id();
        token.store_child_pid(pid);
        child.track_cancel_token(Some(&token));
        let (held, ready) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let dispatch_token = token.clone();
        let dispatch = std::thread::spawn(move || {
            let mut registration = dispatch_token.child_pid.lock().unwrap();
            held.send(()).unwrap();
            let successor = recv_within(&released, std::time::Duration::from_secs(15));
            if let Some(pid) = successor {
                *registration = Some(CapturedProcess {
                    pid,
                    identity: None,
                });
            }
        });
        recv_within(&ready, std::time::Duration::from_secs(5));
        let started = std::time::Instant::now();
        drop(child);
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        wait_for_state(pid, Some('Z')).await;

        let mut unrelated = command("read line").spawn_reaping().unwrap();
        let unrelated_pid = unrelated.id();
        let input = unrelated.stdin.take().unwrap();
        drop(unrelated);
        drop(input);
        wait_for_state(unrelated_pid, None).await;
        assert_eq!(state(pid), Some('Z'), "dispatch still reserves the PID");
        release.send(successor).unwrap();
        join_within(dispatch, std::time::Duration::from_secs(5));
        wait_for_state(pid, None).await;
        assert_eq!(token.child_pid_value(), successor);
    }
}

#[tokio::test]
async fn old_reaped_owner_cannot_clear_a_new_registration_with_the_same_pid() {
    let token = Arc::new(CancelToken::new());
    let mut old = command("exit 7").spawn_reaping().unwrap();
    let pid = old.id();
    token.store_child_pid(pid);
    old.track_cancel_token(Some(&token));
    assert_eq!(old.wait().unwrap().code(), Some(7));
    token.store_child_pid_without_identity_for_test(pid);
    drop(old);

    let mut barrier = command("read line").spawn_reaping().unwrap();
    let barrier_pid = barrier.id();
    let input = barrier.stdin.take().unwrap();
    drop(barrier);
    drop(input);
    wait_for_state(barrier_pid, None).await;
    assert_eq!(token.child_pid_value(), Some(pid));
}
