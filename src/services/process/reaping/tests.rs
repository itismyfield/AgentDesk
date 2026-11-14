use super::*;
use crate::test_support::wait::join_within;
use std::io::Write;
use std::time::Instant;

fn command(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

async fn assert_reaped(pid: u32) {
    let path = format!("/proc/{pid}/stat");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match std::fs::read_to_string(&path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => panic!("read {path}: {error}"),
                Ok(_) => tokio::time::sleep(POLL).await,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("child not reaped: {:?}", std::fs::read_to_string(path)));
}

#[tokio::test(flavor = "current_thread")]
async fn live_child_drop_is_nonblocking_and_does_not_delay_short_child_reaping() {
    let mut live = command("read line").spawn_reaping().unwrap();
    let live_pid = live.id();
    let input = live.stdin.take().unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _ = released.recv_timeout(Duration::from_secs(15));
        drop(input);
    });
    let started = Instant::now();
    drop(live);
    assert!(started.elapsed() < Duration::from_millis(500));
    let mut short = command("read line").spawn_reaping().unwrap();
    let short_pid = short.id();
    let short_input = short.stdin.take().unwrap();
    drop(short);
    drop(short_input);
    assert_reaped(short_pid).await;
    let stat = std::fs::read_to_string(format!("/proc/{live_pid}/stat")).unwrap();
    assert!(
        !stat.rsplit_once(") ").unwrap().1.starts_with('Z'),
        "live child was killed: {stat}"
    );
    release.send(()).unwrap();
    join_within(holder, Duration::from_secs(5));
    assert_reaped(live_pid).await;
}

#[tokio::test]
async fn stdin_error_before_wait_still_reaps_the_child() {
    let mut pid = 0;
    let result = (|| -> io::Result<()> {
        let mut child = command("exec 0<&-; printf r")
            .stdout(std::process::Stdio::piped())
            .spawn_reaping()?;
        pid = child.id();
        let mut ready = [0];
        child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"r");
        child.stdin.take().unwrap().write_all(b"prompt")?;
        child.wait()?;
        Ok(())
    })();
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    assert_reaped(pid).await;
}

#[tokio::test]
async fn unwinding_child_owner_still_reaps_natural_exit() {
    let mut child = command("read line").spawn_reaping().unwrap();
    let pid = child.id();
    let input = child.stdin.take().unwrap();
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _child = child;
        panic!("fixture error before wait");
    }));
    assert!(unwind.is_err());
    drop(input);
    assert_reaped(pid).await;
}

#[test]
fn output_wait_keeps_both_pipes_and_the_requested_exit_status() {
    let mut command = command("head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2; exit 37");
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = command.spawn_reaping().unwrap().wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(37));
    assert_eq!(output.stdout.len(), 131072);
    assert_eq!(output.stderr.len(), 131072);
}

#[test]
fn retained_child_owner_keeps_its_exit_status() {
    let mut child = command("exit 23").spawn_reaping().unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(23));
    assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(23));
}
