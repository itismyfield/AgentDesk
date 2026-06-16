//! OS-level PID-exit observation for the tmux turn runtime (#3479 split).
//!
//! Behavior-preserving extraction from `tmux_runtime.rs`: the `#2426`
//! `wait_for_pid_exit` async family that subscribes to a provider PID's exit
//! (kqueue `EVFILT_PROC|NOTE_EXIT` on macOS, `pidfd_open` + `poll` on Linux)
//! instead of busy-polling, with a single bounded `sleep` fallback. Depends
//! only on `libc`/`std`/`tokio`, so it lives in this leaf module. The
//! interrupt/cancel orchestration that drives it stays in the parent module.

use super::*;

/// #2426: wait until the given PID exits, with an upper bound. Uses OS-level
/// exit signals (kqueue `EVFILT_PROC|NOTE_EXIT` on macOS, `pidfd_open` +
/// `poll` on Linux) so we never busy-poll. When the PID does not exist at
/// call time, returns immediately. When the OS signal API is unavailable or
/// fails (e.g. permission, pre-5.3 kernel), falls back to a single bounded
/// `tokio::sleep` so callers still observe a deterministic upper bound.
///
/// Returns `true` if the process exited within the deadline, `false` if the
/// upper bound elapsed first.
#[cfg(unix)]
pub(super) async fn wait_for_pid_exit(pid: u32, deadline: Duration) -> bool {
    if pid == 0 {
        // libc::kill(0, ...) targets the whole process group; treat as "no
        // PID to observe" and just honour the deadline.
        tokio::time::sleep(deadline).await;
        return false;
    }

    // Fast-path: process already gone (or never existed). `kill(pid, 0)`
    // delivers no signal but tells us whether the PID is reachable.
    #[allow(unsafe_code)]
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !alive {
        return true;
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    let join = tokio::task::spawn_blocking(move || {
        let exited = wait_for_pid_exit_blocking(pid, deadline);
        let _ = tx.send(exited);
    });

    match tokio::time::timeout(deadline, rx).await {
        Ok(Ok(exited)) => {
            // Allow the blocking task to settle. It already finished (rx
            // resolved), so this is a cheap join.
            let _ = join.await;
            exited
        }
        _ => {
            // Outer guard fired or the blocking task panicked / dropped tx.
            // Either way the upper bound has elapsed.
            false
        }
    }
}

#[cfg(not(unix))]
pub(super) async fn wait_for_pid_exit(_pid: u32, deadline: Duration) -> bool {
    tokio::time::sleep(deadline).await;
    false
}

#[cfg(target_os = "macos")]
fn wait_for_pid_exit_blocking(pid: u32, deadline: Duration) -> bool {
    // kqueue + EVFILT_PROC|NOTE_EXIT: kernel notifies us when the PID exits.
    // We arm a single ONESHOT kevent and block in `kevent()` until either
    // NOTE_EXIT fires or the supplied timespec deadline elapses.
    #[allow(unsafe_code)]
    unsafe {
        let kq = libc::kqueue();
        if kq < 0 {
            return wait_for_pid_exit_kill_fallback(pid, deadline);
        }
        let change = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // Register the watch. A zero timeout means "register only, don't
        // block". If the PID already exited or doesn't exist, `kevent` sets
        // EV_ERROR with ESRCH and we treat that as "exited".
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let reg = libc::kevent(
            kq,
            &change as *const _,
            1,
            std::ptr::null_mut(),
            0,
            &zero as *const _,
        );
        if reg < 0 {
            libc::close(kq);
            return wait_for_pid_exit_kill_fallback(pid, deadline);
        }

        let mut event: libc::kevent = std::mem::zeroed();
        let ts = libc::timespec {
            tv_sec: deadline.as_secs() as libc::time_t,
            tv_nsec: deadline.subsec_nanos() as libc::c_long,
        };
        let n = libc::kevent(
            kq,
            std::ptr::null(),
            0,
            &mut event as *mut _,
            1,
            &ts as *const _,
        );
        libc::close(kq);
        if n < 0 {
            return wait_for_pid_exit_kill_fallback(pid, deadline);
        }
        if n == 0 {
            // Timeout — deadline elapsed without NOTE_EXIT.
            return false;
        }
        // One event delivered. NOTE_EXIT fired or EV_ERROR/ESRCH (already
        // gone). Both mean "PID is no longer running".
        true
    }
}

#[cfg(all(unix, target_os = "linux"))]
fn wait_for_pid_exit_blocking(pid: u32, deadline: Duration) -> bool {
    // pidfd_open(pid, 0) returns an fd that becomes readable when the
    // process exits. We poll(POLLIN) it with the deadline. On kernels
    // without pidfd_open (<5.3) the syscall returns ENOSYS and we fall
    // back to a single-shot bounded wait.
    #[allow(unsafe_code)]
    unsafe {
        let fd = libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0_u32) as libc::c_int;
        if fd < 0 {
            return wait_for_pid_exit_kill_fallback(pid, deadline);
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms: libc::c_int = deadline.as_millis().try_into().unwrap_or(libc::c_int::MAX);
        let n = libc::poll(&mut pfd as *mut _, 1, timeout_ms);
        libc::close(fd);
        if n < 0 {
            return wait_for_pid_exit_kill_fallback(pid, deadline);
        }
        n > 0
    }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn wait_for_pid_exit_blocking(pid: u32, deadline: Duration) -> bool {
    wait_for_pid_exit_kill_fallback(pid, deadline)
}

/// Last-resort fallback when the OS-native exit notifier is unavailable
/// (kqueue creation failure, pre-5.3 Linux kernel without `pidfd_open`,
/// other Unix). A single bounded sleep keeps the upper bound deterministic
/// without introducing a polling loop.
#[cfg(unix)]
fn wait_for_pid_exit_kill_fallback(_pid: u32, deadline: Duration) -> bool {
    std::thread::sleep(deadline);
    false
}

// #2426: tests for the PID-exit observation helper. These do not require the
// `legacy-sqlite-tests` feature because they exercise only the
// `wait_for_pid_exit` path and do not touch the SQLite test scaffolding.
#[cfg(all(test, unix))]
mod pid_exit_tests {
    use super::wait_for_pid_exit;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// A PID we are extremely unlikely to ever be reachable: well above the
    /// typical max PID and never spawned by this test process. The kernel
    /// reports ESRCH from `kill(pid, 0)` and `wait_for_pid_exit` should
    /// short-circuit to `true` without blocking for the deadline.
    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_pid_exit_returns_immediately_for_nonexistent_pid() {
        let started = Instant::now();
        let exited = wait_for_pid_exit(4_000_000, Duration::from_secs(5)).await;
        let elapsed = started.elapsed();
        assert!(
            exited,
            "nonexistent PID must be reported as already-exited (kill(pid,0) -> ESRCH)"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "nonexistent PID short-circuit must not honour the full deadline (took {elapsed:?})"
        );
    }

    /// Spawn a real child, kill it, then call `wait_for_pid_exit`. The
    /// kqueue/pidfd path must signal exit well before the 2s upper bound.
    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_pid_exit_observes_child_termination() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();

        // Schedule a kill 50ms in the future so the helper actually has to
        // wait for the exit signal rather than short-circuit via the
        // `kill(pid,0)` fast path.
        let killer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            // SIGKILL = unconditional, no handler chance.
            #[allow(unsafe_code)]
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        });

        let started = Instant::now();
        let exited = wait_for_pid_exit(pid, Duration::from_secs(2)).await;
        let elapsed = started.elapsed();

        // Reap the zombie regardless of test outcome so we don't leak.
        let _ = child.wait();
        let _ = killer.await;

        assert!(
            exited,
            "wait_for_pid_exit must report exit for killed child"
        );
        assert!(
            elapsed < Duration::from_millis(1500),
            "OS-level exit notification must beat the upper bound by a comfortable margin (took {elapsed:?})"
        );
    }

    /// A still-running child must drive `wait_for_pid_exit` to the upper
    /// bound and report `false`.
    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_pid_exit_times_out_when_child_keeps_running() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();

        let started = Instant::now();
        let exited = wait_for_pid_exit(pid, Duration::from_millis(200)).await;
        let elapsed = started.elapsed();

        // Tear down the still-running child.
        #[allow(unsafe_code)]
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        let _ = child.wait();

        assert!(
            !exited,
            "long-running child must not be reported as exited within the upper bound"
        );
        assert!(
            elapsed >= Duration::from_millis(180),
            "the upper bound must actually be honoured (took only {elapsed:?})"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "the upper bound must not be exceeded by more than scheduler jitter (took {elapsed:?})"
        );
    }
}
