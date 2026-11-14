use super::*;
use crate::test_support::wait::join_within;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

static POLLS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static OUTPUT_JOIN: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const {
        std::cell::RefCell::new(None)
    };
}

pub(super) fn record_poll() {
    POLLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn observe_output_join(sender: std::sync::mpsc::Sender<()>) {
    OUTPUT_JOIN.with(|observer| *observer.borrow_mut() = Some(sender));
}

pub(super) fn output_join_started() {
    OUTPUT_JOIN.with(|observer| {
        if let Some(sender) = observer.borrow_mut().take() {
            sender.send(()).expect("output join observer alive");
        }
    });
}

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
    for (stdout, stderr) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut command =
            command("head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2; exit 37");
        let pipe = |enabled| {
            if enabled {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            }
        };
        command.stdout(pipe(stdout)).stderr(pipe(stderr));
        let output = command.spawn_reaping().unwrap().wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(37));
        assert_eq!(output.stdout.len(), if stdout { 131072 } else { 0 });
        assert_eq!(output.stderr.len(), if stderr { 131072 } else { 0 });
    }
}

#[test]
fn retained_child_owner_keeps_its_exit_status() {
    let mut child = command("exit 23").spawn_reaping().unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(23));
    assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(23));
}

#[tokio::test]
async fn output_read_error_keeps_natural_exit_reaping_owned() {
    use std::os::fd::AsRawFd;
    let mut child = command("read line")
        .stdout(std::process::Stdio::piped())
        .spawn_reaping()
        .unwrap();
    let pid = child.id();
    let input = child.stdin.take().unwrap();
    let fd = child.stdout.as_ref().unwrap().as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let waiter = std::thread::spawn(move || child.wait_with_output());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !waiter.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let returned_before_exit = waiter.is_finished();
    let live = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    drop(input);
    let error = join_within(waiter, Duration::from_secs(5)).unwrap_err();
    assert!(returned_before_exit, "read error waited for the live child");
    assert!(!live.rsplit_once(") ").unwrap().1.starts_with('Z'));
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_reaped(pid).await;
}

#[test]
fn output_reader_panic_is_returned_as_an_io_error() {
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| -> io::Result<Vec<u8>> { panic!("fixture reader panic") });
        let error = join_pipe(Some(reader), "stdout").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "stdout reader panicked");
    });
}

#[tokio::test]
async fn empty_reaper_stops_polling_and_enqueue_restarts_it() {
    const ISOLATED: &str = "ADK_REAPER_IDLE_TEST_CHILD";
    if std::env::var_os(ISOLATED).is_none() {
        let root = tempfile::tempdir().unwrap();
        let test = concat!(
            module_path!(),
            "::empty_reaper_stops_polling_and_enqueue_restarts_it"
        );
        let test = test.split_once("::").unwrap().1;
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(ISOLATED, "1")
            .env("AGENTDESK_ROOT_DIR", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("REAPER_IDLE_ORACLE_EXECUTED"));
        return;
    }
    for turn in 0..2 {
        let mut child = command("read line").spawn_reaping().unwrap();
        let pid = child.id();
        let input = child.stdin.take().unwrap();
        let before_enqueue = POLLS.load(Ordering::Relaxed);
        drop(child);
        drop(input);
        assert_reaped(pid).await;
        let idle = POLLS.load(Ordering::Relaxed);
        assert!(
            idle > before_enqueue,
            "enqueue {turn} did not resume polling"
        );
        tokio::time::sleep(POLL * 5).await;
        assert_eq!(
            POLLS.load(Ordering::Relaxed),
            idle,
            "empty queue kept polling"
        );
    }
    println!("REAPER_IDLE_ORACLE_EXECUTED");
}
