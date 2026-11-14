use super::*;
use std::time::Instant;

const PID_PATH: &str = "ADK_RELAY_REAP_TEST_PID_PATH";

pub(super) fn record_worker_pid() {
    if let Some(path) = std::env::var_os(PID_PATH) {
        std::fs::write(path, format!("{}\n", std::process::id())).unwrap();
    }
}

fn isolated(test: &str, run: impl FnOnce(&Path)) {
    if let Some(path) = std::env::var_os(PID_PATH) {
        run(Path::new(&path));
        println!("ADK_RELAY_REAP_TEST_EXECUTED");
        return;
    }
    let (_, test) = test.split_once("::").expect("crate-qualified test name");
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(PID_PATH, dir.path().join("worker.pid"))
        .env("AGENTDESK_ROOT_DIR", dir.path())
        .env_remove(FAILURE_MARKER_WORKER_ENV)
        .env_remove(NON_WAIT_RELAY_WORKER_ENV)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ADK_RELAY_REAP_TEST_EXECUTED"),
        "isolated subprocess must execute the reaping oracle"
    );
}

fn assert_worker_reaped(pid_path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid: u32 = loop {
        if let Ok(pid) = std::fs::read_to_string(pid_path)
            && let Some(pid) = pid.strip_suffix('\n').and_then(|pid| pid.parse().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "worker did not record its PID");
        std::thread::sleep(Duration::from_millis(10));
    };
    let stat_path = format!("/proc/{pid}/stat");
    loop {
        let stat = match std::fs::read_to_string(&stat_path) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("read {stat_path}: {error}"),
        };
        if Instant::now() >= deadline {
            let state = stat
                .rsplit_once(") ")
                .unwrap()
                .1
                .split_whitespace()
                .next()
                .unwrap();
            // SAFETY: PID belongs to this fixture; a null status pointer is allowed.
            // Reap only after recording the failed /proc oracle.
            unsafe {
                libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG);
            }
            panic!("relay child {pid} was not reaped: /proc state={state}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn ordered_relay_worker_is_reaped_after_exit() {
    isolated(
        concat!(
            module_path!(),
            "::ordered_relay_worker_is_reaped_after_exit"
        ),
        |pid_path| {
            let queue = pid_path.parent().unwrap().join("queue");
            std::fs::create_dir(&queue).unwrap();
            ordered_queue::start_ordered_hook_relay_worker(&queue).unwrap();
            assert_worker_reaped(pid_path);
        },
    );
}

#[test]
fn failure_marker_worker_is_reaped_after_exit() {
    isolated(
        concat!(
            module_path!(),
            "::failure_marker_worker_is_reaped_after_exit"
        ),
        |pid_path| {
            handoff_hook_relay_failure(
                "http://127.0.0.1:1",
                "claude",
                "Stop",
                "reap-fixture",
                "fixture error",
            )
            .unwrap();
            assert_worker_reaped(pid_path);
            let markers = failure_marker_dir("claude").unwrap();
            assert_eq!(std::fs::read_dir(markers).unwrap().count(), 1);
        },
    );
}
