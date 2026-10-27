#![cfg(unix)]

use super::*;
use crate::config::TestEnvVarGuard as Env;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub(crate) struct Fixture {
    _env: [Env; 4],
    pub(crate) root: tempfile::TempDir,
    pub(crate) real: PathBuf,
    pub(crate) socket: String,
    _lock: Option<crate::config::test_env_lock::SharedTestEnvLockGuard>,
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

impl Fixture {
    pub(crate) fn new() -> Self {
        Self::with_lock(Some(
            crate::config::test_env_lock::acquire_shared_test_env_lock(),
        ))
    }

    pub(crate) fn after_shared_test_env_lock() -> Self {
        Self::with_lock(None)
    }

    fn with_lock(lock: Option<crate::config::test_env_lock::SharedTestEnvLockGuard>) -> Self {
        let root = tempfile::Builder::new()
            .prefix("cgb2-")
            .tempdir_in("/tmp")
            .unwrap();
        let real = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join("tmux"))
            .find(|p| p.is_file())
            .expect("real tmux required");
        let socket = format!("cgb2-{}", std::process::id());
        let shim = root.path().join("tmux");
        std::fs::write(
            &shim,
            format!(
                r#"#!/bin/sh
for arg in "$@"; do
  if [ "$arg" = capture-pane ] && [ "$(cat {mode} 2>/dev/null)" = capture_resize ]; then
    {real} -L {socket} resize-window -t =capture-race: -x 80 -y 24
  fi
done
exec {real} -L {socket} -f /dev/null "$@"
"#,
                mode = quoted(&root.path().join("mode")),
                real = quoted(&real)
            ),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            root.path().join("receiver.py"),
            r#"import os, pathlib, sys, tty
tty.setraw(0)
root = pathlib.Path(sys.argv[1])
screen = (root / 'screen').read_bytes().decode() if (root / 'screen').exists() else sys.argv[2] + '\r\n'
os.write(1, ('\x1b[?2004h\x1b[2J\x1b[H' + screen).encode())
(root / 'ready').write_text('ready')
while True:
    with (root / 'received').open('ab') as f:
        f.write(os.read(0, 65536))
"#,
        )
        .unwrap();
        let env = [
            Env::prepend_path_after_shared_test_env_lock(root.path()),
            Env::set_path_after_shared_test_env_lock("TMUX_TMPDIR", root.path()),
            Env::capture_after_shared_test_env_lock("TMUX"),
            Env::capture_after_shared_test_env_lock("TMUX_PANE"),
        ];
        // Host admission must observe the fixture socket, even in an attached tmux lane.
        unsafe {
            std::env::remove_var("TMUX");
            std::env::remove_var("TMUX_PANE");
        }
        Self {
            _lock: lock,
            root,
            _env: env,
            real,
            socket,
        }
    }

    pub(crate) fn tmux(&self, args: &[&str]) -> String {
        let output = Command::new(&self.real)
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "tmux {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    pub(crate) fn receiver(&self, label: &str) -> String {
        let dir = self.root.path().join(label);
        std::fs::create_dir(&dir).unwrap();
        format!(
            "python3 {} {} {label}",
            quoted(&self.root.path().join("receiver.py")),
            quoted(&dir)
        )
    }

    pub(crate) fn session(&self, name: &str) -> String {
        self.session_with_screen(name, &format!("{name}\r\n"))
    }

    pub(crate) fn session_with_screen(&self, name: &str, screen: &str) -> String {
        let command = self.receiver(name);
        std::fs::write(self.root.path().join(name).join("screen"), screen).unwrap();
        let pane = self.tmux(&[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-s",
            name,
            "-x",
            "160",
            "-y",
            "40",
            &command,
        ]);
        self.tmux(&["set-window-option", "-t", &pane, "window-size", "manual"]);
        self.wait_ready(name);
        wait_until(|| {
            self.tmux(&["capture-pane", "-p", "-t", &pane])
                .contains(screen.lines().next().unwrap().trim_end())
        });
        pane
    }

    pub(crate) fn wait_ready(&self, label: &str) {
        wait_until(|| self.root.path().join(label).join("ready").exists());
    }

    pub(crate) fn count(&self, label: &str) -> usize {
        self.received(label).iter().filter(|b| **b == b'\r').count()
    }

    pub(crate) fn received(&self, label: &str) -> Vec<u8> {
        std::fs::read(self.root.path().join(label).join("received")).unwrap_or_default()
    }

    pub(crate) fn assert_enter(&self, label: &str, expected: usize) {
        if expected > 0 {
            wait_until(|| self.count(label) >= expected);
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(self.count(label), expected, "actual CR receipt for {label}");
    }

    fn control(&self) {
        self.session("control");
        let pinned = SubmissionTmux.pin_guarded("control").unwrap();
        assert!(pinned.capture_draft().unwrap().contains("control"));
    }

    /// `kill-server` returns before the server process exits; a client started in that gap
    /// fails with "server exited unexpectedly", so wait until the old server is gone.
    fn kill_server(&self) {
        let pid = self.tmux(&["display-message", "-p", "#{pid}"]);
        self.tmux(&["kill-server"]);
        wait_until(|| {
            !Command::new("kill")
                .args(["-0", &pid])
                .output()
                .unwrap()
                .status
                .success()
        });
    }

    pub(crate) fn attach(&self, session: &str) {
        self.attach_with_target(session, session, &format!("{}-driver", self.socket));
    }

    fn attach_with_target(&self, session: &str, target: &str, driver: &str) {
        let stderr_path = self.root.path().join("attach.stderr");
        let command = format!(
            "{{ env -u TMUX {} -L {} -f /dev/null attach -t '={target}'; }} 2>{}",
            quoted(&self.real),
            self.socket,
            quoted(&stderr_path)
        );
        let started = Command::new(&self.real)
            .args([
                "-L",
                driver,
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "driver",
                "sleep 30",
                ";",
                "set-window-option",
                "-t",
                "=driver:",
                "remain-on-exit",
                "on",
                ";",
                "respawn-pane",
                "-k",
                "-t",
                "=driver:",
                &command,
            ])
            .output()
            .unwrap();
        assert!(
            started.status.success(),
            "attach failed: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let state = Command::new(&self.real)
                .args([
                    "-L",
                    driver,
                    "-f",
                    "/dev/null",
                    "display-message",
                    "-p",
                    "-t",
                    "=driver:",
                    "#{pane_dead}|#{pane_dead_status}|#{pane_tty}",
                ])
                .output()
                .unwrap();
            let pane_status = String::from_utf8_lossy(&state.stdout);
            if !state.status.success() || pane_status.trim().split('|').next() != Some("0") {
                let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
                let diagnostic = if stderr.trim().is_empty() {
                    format!(
                        "driver pane dead/status={:?}; {}",
                        pane_status.trim(),
                        String::from_utf8_lossy(&state.stderr).trim()
                    )
                } else {
                    stderr.trim().to_owned()
                };
                panic!("attach failed: {diagnostic}");
            }
            let tty = pane_status
                .trim()
                .split('|')
                .nth(2)
                .filter(|tty| !tty.is_empty())
                .expect("driver tty unavailable");
            if self
                .tmux(&[
                    "list-clients",
                    "-t",
                    &format!("={session}:"),
                    "-F",
                    "#{client_tty}",
                ])
                .lines()
                .any(|client| client == tty)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "attach failed: driver did not attach before deadline"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new(&self.real)
            .args(["-L", &format!("{}-bad-driver", self.socket), "kill-server"])
            .output();
        let _ = Command::new(&self.real)
            .args(["-L", &format!("{}-driver", self.socket), "kill-server"])
            .output();
        let _ = Command::new(&self.real)
            .args(["-L", &self.socket, "kill-server"])
            .output();
    }
}

#[test]
fn cg_b2b_modes_and_attached_writer_refuse_pin_and_capture() {
    let f = Fixture::new();
    f.control();
    for mode in ["copy", "input-off", "attached"] {
        let pane = f.session(mode);
        let pinned = SubmissionTmux.pin_guarded(mode).unwrap();
        match mode {
            "copy" => {
                f.tmux(&["copy-mode", "-t", &pane]);
            }
            "input-off" => {
                f.tmux(&["select-pane", "-d", "-t", &pane]);
            }
            _ => f.attach(mode),
        }
        assert!(
            SubmissionTmux.pin_guarded(mode).is_err(),
            "pin admitted {mode}"
        );
        assert!(pinned.capture_draft().is_none(), "snapshot admitted {mode}");
        f.assert_enter(mode, 0);
    }
}

#[test]
fn cg_b2b_capture_rechecks_snapshot_in_same_tmux_command() {
    let f = Fixture::new();
    f.control();
    f.session("capture-race");
    let pinned = SubmissionTmux.pin_guarded("capture-race").unwrap();
    std::fs::write(f.root.path().join("mode"), "capture_resize").unwrap();
    assert!(
        pinned.capture_draft().is_none(),
        "changed geometry was used as ownership capture"
    );
    f.assert_enter("capture-race", 0);
}

#[test]
fn cg_b2b_linked_window_writer_is_never_admitted() {
    let f = Fixture::new();
    f.control();
    f.session("linked-original");
    let pinned = SubmissionTmux.pin_guarded("linked-original").unwrap();
    f.tmux(&["new-session", "-d", "-s", "linked-peer", "sleep 30"]);
    f.tmux(&[
        "link-window",
        "-s",
        "=linked-original:",
        "-t",
        "=linked-peer:",
        "-a",
    ]);
    f.tmux(&["select-window", "-t", "=linked-peer:1"]);
    f.attach("linked-peer");
    assert_eq!(
        f.tmux(&[
            "display-message",
            "-p",
            "-t",
            "=linked-original:",
            "#{session_attached}"
        ]),
        "0"
    );
    assert!(SubmissionTmux.pin_guarded("linked-original").is_err());
    assert!(pinned.capture_draft().is_none());
    f.assert_enter("linked-original", 0);
    assert!(f.received("linked-original").is_empty());
}

#[test]
fn cg_b2b_capture_joins_only_soft_wrap_and_keeps_spaces() {
    let f = Fixture::new();
    f.control();
    let wrapped = format!("wrapped {} tail  ", "a  ".repeat(100));
    f.session_with_screen(
        "soft-wrap",
        &format!("heading\r\n{wrapped}\r\nexplicit one\r\nexplicit two\r\n"),
    );
    let pinned = SubmissionTmux.pin_guarded("soft-wrap").unwrap();
    let capture = pinned.capture_draft().unwrap();
    assert!(
        capture.contains(&wrapped),
        "soft wrap changed spaces: {capture:?}"
    );
    assert!(
        capture.contains("explicit one\nexplicit two"),
        "explicit LF was joined: {capture:?}"
    );
    f.assert_enter("soft-wrap", 0);
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(3);
    while !ready() {
        assert!(Instant::now() < until, "fixture did not become ready");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn cg_b2a_pin_rejects_missing_small_nonmanual_split_and_dead() {
    let f = Fixture::new();
    f.control();
    assert!(SubmissionTmux.pin_guarded("missing").is_err());
    for (name, width, height) in [("narrow", "159", "40"), ("short", "160", "39")] {
        let pane = f.session(name);
        f.tmux(&["resize-window", "-t", &pane, "-x", width, "-y", height]);
        assert!(SubmissionTmux.pin_guarded(name).is_err());
        f.assert_enter(name, 0);
    }
    let pane = f.session("latest");
    f.tmux(&["set-window-option", "-t", &pane, "window-size", "latest"]);
    assert!(SubmissionTmux.pin_guarded("latest").is_err());
    f.assert_enter("latest", 0);
    let pane = f.session("split");
    f.tmux(&["split-window", "-h", "-d", "-t", &pane, "sleep 30"]);
    f.tmux(&["resize-window", "-t", &pane, "-x", "321", "-y", "40"]);
    f.tmux(&["resize-pane", "-t", &pane, "-x", "160"]);
    assert_eq!(
        f.tmux(&[
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{pane_width} #{pane_height} #{window_panes} #{window-size}"
        ]),
        "160 40 2 manual"
    );
    assert!(SubmissionTmux.pin_guarded("split").is_err());
    f.assert_enter("split", 0);
    let pane = f.session("dead");
    f.tmux(&["set-window-option", "-t", &pane, "remain-on-exit", "on"]);
    f.tmux(&["respawn-pane", "-k", "-t", &pane, "true"]);
    wait_until(|| f.tmux(&["display-message", "-p", "-t", &pane, "#{pane_dead}"]) == "1");
    assert!(SubmissionTmux.pin_guarded("dead").is_err());
    f.assert_enter("dead", 0);
}

#[test]
fn cg_b2a_replaced_same_name_and_dead_identity_never_capture() {
    let f = Fixture::new();
    f.control();
    let old = f.session("replacement");
    let pinned = SubmissionTmux.pin_guarded("replacement").unwrap();
    // Keep this server alive so replacing a session cannot recycle pane ids.
    f.session("keepalive");
    f.tmux(&["kill-session", "-t", "=replacement"]);
    let command = f.receiver("new-receiver");
    let new = f.tmux(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-s",
        "replacement",
        "-x",
        "160",
        "-y",
        "40",
        &command,
    ]);
    f.tmux(&["set-window-option", "-t", &new, "window-size", "manual"]);
    f.wait_ready("new-receiver");
    assert_ne!(old, new);
    assert!(pinned.capture_draft().is_none());
    f.assert_enter("new-receiver", 0);
    let pane = f.session("post-dead");
    let pinned = SubmissionTmux.pin_guarded("post-dead").unwrap();
    f.tmux(&["set-window-option", "-t", &pane, "remain-on-exit", "on"]);
    f.tmux(&["respawn-pane", "-k", "-t", &pane, "true"]);
    wait_until(|| f.tmux(&["display-message", "-p", "-t", &pane, "#{pane_dead}"]) == "1");
    assert!(pinned.capture_draft().is_none());
    f.assert_enter("post-dead", 0);
    let pane = f.session("respawn-original");
    let pinned = SubmissionTmux.pin_guarded("respawn-original").unwrap();
    let command = f.receiver("respawn-new");
    f.tmux(&["respawn-pane", "-k", "-t", &pane, &command]);
    f.wait_ready("respawn-new");
    assert!(pinned.capture_draft().is_none());
    f.assert_enter("respawn-new", 0);
    f.kill_server();
    let old = f.session("restart");
    let pinned = SubmissionTmux.pin_guarded("restart").unwrap();
    f.kill_server();
    let command = f.receiver("restart-new");
    let new = f.tmux(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-s",
        "restart",
        "-x",
        "160",
        "-y",
        "40",
        &command,
    ]);
    f.tmux(&["set-window-option", "-t", &new, "window-size", "manual"]);
    f.wait_ready("restart-new");
    assert_eq!(old, new, "server restart recycles textual pane ids");
    assert!(pinned.capture_draft().is_none());
    f.assert_enter("restart-new", 0);
}

#[test]
fn cg_b2a_capture_stays_on_pinned_pane_after_active_window_change() {
    let f = Fixture::new();
    f.session("original");
    let pinned = SubmissionTmux.pin_guarded("original").unwrap();
    let command = f.receiver("other-window");
    let other = f.tmux(&[
        "new-window",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "=original:",
        &command,
    ]);
    f.wait_ready("other-window");
    let command = f.receiver("other-pane");
    f.tmux(&["split-window", "-t", &other, &command]);
    f.wait_ready("other-pane");
    let capture = pinned.capture_draft().unwrap();
    assert!(capture.contains("original"), "{capture:?}");
    assert!(!capture.contains("other-"), "{capture:?}");
    f.assert_enter("original", 0);
    f.assert_enter("other-window", 0);
    f.assert_enter("other-pane", 0);
}

#[test]
fn cg_b2c_attach_requires_this_driver_even_with_another_client() {
    let f = Fixture::new();
    f.session("attach-identity");
    f.attach("attach-identity");
    assert_eq!(
        f.tmux(&[
            "display-message",
            "-p",
            "-t",
            "=attach-identity:",
            "#{session_attached}"
        ]),
        "1"
    );
    let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        f.attach_with_target(
            "attach-identity",
            "absent-target",
            &format!("{}-bad-driver", f.socket),
        );
    }));
    assert!(
        stopped.is_err(),
        "a different client's attachment cannot prove this driver succeeded"
    );
}
