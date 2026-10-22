//! Resource counts for the health deadline test: the process's threads as the OS reports them,
//! and the pool's and the proxy's connections, sampled on one warmed server between requests.

use std::io::{Read, Seek};
use std::net::SocketAddr;
use std::process::Command;
use std::time::{Duration, Instant};

use super::{Db, http_get, reasons};

/// Set in the rerun process, which runs the test body itself.
const ALONE: &str = "HEALTH_DB_DEADLINE_RESOURCE_RERUN";
/// A rerun still running after this is killed and fails the test rather than hanging it.
const RERUN_LIMIT: Duration = Duration::from_secs(120);
const SEGMENTS: usize = 3;
const REQUESTS: usize = 16;
const MAX_CONNECTIONS: usize = 8;
/// Blocking threads the runtime may still start lazily after the warm-up; a per-request leak
/// adds one per request, 16 a segment.
const THREAD_SLACK: usize = 4;

/// In the test's own process, reruns `test` (a full test path) alone in a new process of this
/// binary and checks that it ran and passed; returns false inside that rerun.
pub(super) fn rerun_alone(test: &str) -> bool {
    if std::env::var_os(ALONE).is_some() {
        return false;
    }
    // Test names drop the crate segment of the module path.
    let name = test.split_once("::").expect("a full test path").1;
    let mut log = tempfile::tempfile().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads=1", "--nocapture"])
        .env(ALONE, "1")
        .stdout(log.try_clone().unwrap())
        .stderr(log.try_clone().unwrap())
        .spawn()
        .expect("rerun the test binary");
    let deadline = Instant::now() + RERUN_LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut output = String::new();
    log.rewind().unwrap();
    log.read_to_string(&mut output).unwrap();
    assert!(
        status.is_some_and(|status| status.success())
            && output.contains("test result: ok. 1 passed"),
        "the rerun of {name} failed ({status:?}):\n{output}"
    );
    true
}

#[cfg(target_os = "linux")]
fn process_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .expect("read /proc/self/task")
        .count()
}

#[cfg(target_os = "macos")]
fn process_threads() -> usize {
    // SAFETY: `info` is a plain C struct the call fills, and `size` is its exact size.
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            std::process::id() as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    assert_eq!(read, size, "proc_pidinfo(PROC_PIDTASKINFO) failed");
    usize::try_from(info.pti_threadnum).expect("a thread count")
}

/// An unobservable thread count fails the test instead of passing it unmeasured.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_threads() -> usize {
    panic!("no process thread count on this platform")
}

#[derive(Debug)]
struct Sample {
    threads: usize,
    pool: usize,
    accepted: usize,
    open: usize,
}

/// Counts once every pooled connection is idle again.
async fn sample(db: &Db) -> Sample {
    db.settle().await;
    Sample {
        threads: process_threads(),
        pool: db.pool.size() as usize,
        accepted: db.proxy.accepted(),
        open: db.proxy.open(),
    }
}

/// Public and detail requests in turn, each answered by a database that kept up.
async fn requests(addr: SocketAddr) {
    for n in 0..REQUESTS {
        let path = if n % 2 == 0 {
            "/health"
        } else {
            "/health/detail"
        };
        let (status, body, _) = http_get(addr, path).await;
        assert_eq!(body["db"], true, "{path}: {status} {body}");
        assert!(!reasons(&body).iter().any(|r| r == "db_slow"), "{body}");
    }
}

/// After one warm-up round, no round adds a pool connection, an accepted or open proxy
/// connection, or a process thread per request; the pool may only fill to its size.
pub(super) async fn repeated_requests_keep_resources(db: &Db, addr: SocketAddr) {
    requests(addr).await;
    let base = sample(db).await;
    for segment in 1..=SEGMENTS {
        requests(addr).await;
        let now = sample(db).await;
        let context = format!("segment {segment}: {base:?} -> {now:?}");
        assert!(now.pool <= MAX_CONNECTIONS, "{context}");
        assert!(now.open <= MAX_CONNECTIONS, "{context}");
        assert!(
            now.accepted - base.accepted <= MAX_CONNECTIONS - base.pool,
            "{context}"
        );
        assert!(now.threads <= base.threads + THREAD_SLACK, "{context}");
    }
}

/// After cut and cancelled requests the pool may have dropped and reopened connections, but
/// never holds more than its size open and has every one idle again.
pub(super) async fn connections_return_after_cuts(db: &Db) {
    let now = sample(db).await;
    assert!(
        now.pool <= MAX_CONNECTIONS && now.open <= MAX_CONNECTIONS,
        "{now:?}"
    );
}
