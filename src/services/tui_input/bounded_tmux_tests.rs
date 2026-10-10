#![cfg(any(target_os = "macos", target_os = "linux"))]

use super::bounded_tmux::{
    BoundedTmuxError, run_bounded_tmux, run_with_budget, spawn_bounded, spawn_with_budget,
};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

struct Fixture {
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
        fs::create_dir_all(&root).unwrap();
        Self {
            directory: tempfile::tempdir_in(root).unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn command(&self, body: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                &format!("printf '%s\\n' \"$$\" > \"$1\"; {body}"),
                "bounded-tmux-test",
            ])
            .arg(self.path("pids"))
            .arg(self.path("effect"));
        command
    }

    fn has_pid(&self) -> bool {
        fs::read_to_string(self.path("pids"))
            .ok()
            .is_some_and(|text| {
                text.lines()
                    .next()
                    .is_some_and(|pid| pid.parse::<libc::pid_t>().is_ok())
            })
    }

    fn pid(&self) -> libc::pid_t {
        fs::read_to_string(self.path("pids"))
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    fn assert_reaped(&self) {
        let mut status = 0;
        let result = unsafe { libc::waitpid(self.pid(), &mut status, libc::WNOHANG) };
        let error = std::io::Error::last_os_error();
        assert_eq!(result, -1, "owned child must already be reaped");
        assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let Ok(text) = fs::read_to_string(self.path("pids")) else {
            return;
        };
        let Some(pid) = text
            .lines()
            .next()
            .and_then(|pid| pid.parse::<libc::pid_t>().ok())
        else {
            return;
        };
        let mut status = 0;
        if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == 0 {
            // Failed mutants may leave only this fixture's owned process group alive.
            unsafe { libc::killpg(pid, libc::SIGKILL) };
            while unsafe { libc::waitpid(pid, &mut status, 0) } < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
            }
        }
    }
}

#[test]
fn bounded_tmux_scoped_current_thread_runtime_drop_reaps_child() {
    for _ in 0..5 {
        let fixture = Fixture::new();
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let mut command = fixture.command("exec /bin/sleep 30");
                    runtime.spawn(async move {
                        run_with_budget(&mut command, Duration::from_secs(5)).await
                    });
                    runtime.block_on(async {
                        let until = Instant::now() + Duration::from_secs(1);
                        while !fixture.has_pid() {
                            assert!(Instant::now() < until, "controlled child did not start");
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                    });
                    let dropping = Instant::now();
                    drop(runtime);
                    assert!(dropping.elapsed() < Duration::from_millis(2250));
                })
                .join()
                .unwrap();
        });
        std::thread::sleep(Duration::from_millis(20));
        fixture.assert_reaped();
    }
}

#[test]
fn bounded_tmux_scoped_current_thread_timeout_kills_and_reaps() {
    let fixture = Fixture::new();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let mut command = fixture.command("while :; do /bin/sleep 30; done");
                let started = Instant::now();
                let result =
                    runtime.block_on(run_with_budget(&mut command, Duration::from_millis(75)));
                assert!(
                    matches!(
                        result,
                        Err(BoundedTmuxError::Timeout {
                            killed: true,
                            reaped: true
                        })
                    ),
                    "{result:?}"
                );
                assert!(started.elapsed() < Duration::from_millis(2250));
                fixture.assert_reaped();
            })
            .join()
            .unwrap();
    });
}

#[test]
fn bounded_tmux_scoped_pending_drop_still_runs_deadline_cleanup() {
    let fixture = Fixture::new();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let mut command = fixture.command("exec /bin/sleep 30");
                let pending =
                    spawn_with_budget(runtime.handle(), &mut command, Duration::from_millis(75))
                        .unwrap();
                drop(pending);
                runtime.block_on(async { tokio::time::sleep(Duration::from_millis(250)).await });
                fixture.assert_reaped();
            })
            .join()
            .unwrap();
    });
}

#[test]
fn bounded_tmux_explicit_handle_without_context_keeps_spawn_deadline_on_delayed_wait() {
    let fixture = Fixture::new();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                assert!(tokio::runtime::Handle::try_current().is_err());
                let mut command = fixture.command("exec /bin/sleep 30");
                let pending =
                    spawn_with_budget(runtime.handle(), &mut command, Duration::from_millis(500))
                        .unwrap();
                std::thread::sleep(Duration::from_millis(750));
                let waiting = Instant::now();
                let result = runtime.block_on(pending.wait());
                assert!(
                    matches!(
                        result,
                        Err(BoundedTmuxError::Timeout {
                            killed: true,
                            reaped: true
                        })
                    ),
                    "{result:?}"
                );
                assert!(
                    waiting.elapsed() < Duration::from_millis(250),
                    "wait renewed the spawn budget"
                );
                fixture.assert_reaped();
            })
            .join()
            .unwrap();
    });
}

#[test]
fn bounded_tmux_runtime_drop_reaps_before_cleanup_first_poll() {
    let fixture = Fixture::new();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let mut command = fixture.command("exec /bin/sleep 30");
                let pending = spawn_bounded(runtime.handle(), &mut command).unwrap();
                let until = Instant::now() + Duration::from_secs(1);
                while !fixture.has_pid() {
                    assert!(Instant::now() < until, "controlled child did not start");
                    std::thread::sleep(Duration::from_millis(1));
                }
                drop(runtime);
                fixture.assert_reaped();
                drop(pending);
            })
            .join()
            .unwrap();
    });
}

#[tokio::test]
async fn bounded_tmux_preserves_output_exit_status_and_closes_stdin() {
    let fixture = Fixture::new();
    let mut command =
        fixture.command("printf stdout; printf stderr >&2; if read input; then exit 9; fi; exit 7");
    let output = run_bounded_tmux(&mut command).await.unwrap();
    assert_eq!(output.stdout, b"stdout");
    assert_eq!(output.stderr, b"stderr");
    assert_eq!(output.status.code(), Some(7));
    fixture.assert_reaped();
    let mut missing = Command::new(fixture.path("missing-tmux"));
    let error = run_bounded_tmux(&mut missing).await.unwrap_err();
    assert!(matches!(error, BoundedTmuxError::Spawn(_)));
    assert!(!error.may_have_effect());
}

#[tokio::test]
async fn bounded_tmux_timeout_kills_descendants_before_reaping_live_or_exited_leader() {
    for ending in ["wait", "exit 0"] {
        let fixture = Fixture::new();
        let mut command = fixture.command(&format!(
            "(sleep 0.3; printf late > \"$2\") & printf '%s\\n' \"$!\" >> \"$1\"; {ending}"
        ));
        let started = Instant::now();
        let result = run_with_budget(&mut command, Duration::from_millis(75)).await;
        let elapsed = started.elapsed();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(result.as_ref().unwrap_err().may_have_effect());
        assert!(
            matches!(
                result,
                Err(BoundedTmuxError::Timeout {
                    killed: true,
                    reaped: true
                })
            ),
            "{ending}: {result:?}"
        );
        assert!(elapsed < Duration::from_millis(2250), "{elapsed:?}");
        assert!(
            !fixture.path("effect").exists(),
            "{ending}: descendant escaped"
        );
        assert_eq!(
            fs::read_to_string(fixture.path("pids"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        fixture.assert_reaped();
    }
}

#[tokio::test]
async fn bounded_tmux_caller_cancellation_preserves_deadline_cleanup() {
    let fixture = Fixture::new();
    let mut command = fixture
        .command("(sleep 0.3; printf late > \"$2\") & printf '%s\\n' \"$!\" >> \"$1\"; wait");
    let caller =
        tokio::spawn(async move { run_with_budget(&mut command, Duration::from_millis(75)).await });
    let deadline = Instant::now() + Duration::from_secs(1);
    while fs::read_to_string(fixture.path("pids"))
        .map(|text| text.lines().count() < 2)
        .unwrap_or(true)
    {
        assert!(Instant::now() < deadline, "controlled child did not start");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!fixture.path("effect").exists());
    fixture.assert_reaped();
}

#[tokio::test]
async fn bounded_tmux_excess_output_fails_with_cleanup_for_both_streams() {
    for redirect in ["", ">&2"] {
        let fixture = Fixture::new();
        let mut command = fixture.command(&format!(
            "printf applied > \"$2\"; \
             chunk=x; i=0; while [ \"$i\" -lt 13 ]; do chunk=\"$chunk$chunk\"; i=$((i+1)); done; \
             i=0; while [ \"$i\" -lt 129 ]; do printf '%s' \"$chunk\"; i=$((i+1)); done {redirect}; sleep 0.3"
        ));
        let result = run_bounded_tmux(&mut command).await;
        assert_eq!(fs::read(fixture.path("effect")).unwrap(), b"applied");
        assert!(result.as_ref().unwrap_err().may_have_effect());
        match result {
            Err(BoundedTmuxError::Io {
                source,
                killed,
                reaped,
            }) => {
                assert!(
                    source
                        .to_string()
                        .contains("output exceeds 1 MiB per stream")
                );
                assert!(killed);
                assert!(reaped);
            }
            other => panic!(
                "expected output limit failure: {:?}",
                other.map(|output| (output.status, output.stdout.len(), output.stderr.len()))
            ),
        }
        fixture.assert_reaped();
    }
}

#[test]
fn bounded_tmux_effect_uncertainty_is_independent_of_cleanup() {
    for killed in [false, true] {
        for reaped in [false, true] {
            assert!(BoundedTmuxError::Timeout { killed, reaped }.may_have_effect());
            assert!(
                BoundedTmuxError::Io {
                    source: std::io::Error::other("post-spawn failure"),
                    killed,
                    reaped,
                }
                .may_have_effect()
            );
        }
    }
    assert!(!BoundedTmuxError::Spawn(std::io::Error::other("spawn failed")).may_have_effect());
    assert!(!BoundedTmuxError::UnsupportedPlatform.may_have_effect());
}
