#![cfg(unix)]

use super::*;
use crate::config::TestEnvVarGuard as Env;
use crate::services::claude_tui::host_input::{InputTransport, StopCause};
use crate::services::session_host::HostKey;
use crate::services::tui_input::submission::Refusal;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct Fixture {
    _env: [Env; 2],
    root: tempfile::TempDir,
    real: PathBuf,
    socket: String,
    _lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

impl Fixture {
    fn new() -> Self {
        let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
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
  if [ "$arg" = if-shell ]; then
    printf x >> {calls}
    mode=$(cat {mode} 2>/dev/null)
    case "$mode" in
      drop|nonzero|timeout)
        {real} -L {socket} -f /dev/null "$@" >/dev/null
        case "$mode" in nonzero) exit 1;; timeout) sleep 6;; esac
        exit 0;;
    esac
  fi
done
exec {real} -L {socket} -f /dev/null "$@"
"#,
                calls = quoted(&root.path().join("calls")),
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
os.write(1, ('\x1b[2J\x1b[H' + sys.argv[2] + '\r\n').encode())
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
        ];
        Self {
            _lock: lock,
            root,
            _env: env,
            real,
            socket,
        }
    }

    fn tmux(&self, args: &[&str]) -> String {
        let output = Command::new(&self.real)
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "tmux {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn receiver(&self, label: &str) -> String {
        let dir = self.root.path().join(label);
        std::fs::create_dir(&dir).unwrap();
        format!(
            "python3 {} {} {label}",
            quoted(&self.root.path().join("receiver.py")),
            quoted(&dir)
        )
    }

    fn session(&self, name: &str) -> String {
        let command = self.receiver(name);
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
                .contains(name)
        });
        pane
    }

    fn wait_ready(&self, label: &str) {
        wait_until(|| self.root.path().join(label).join("ready").exists());
    }

    fn count(&self, label: &str) -> usize {
        std::fs::read(self.root.path().join(label).join("received"))
            .unwrap_or_default()
            .iter()
            .filter(|b| **b == b'\r')
            .count()
    }

    fn assert_enter(&self, label: &str, expected: usize) {
        if expected > 0 {
            wait_until(|| self.count(label) >= expected);
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(self.count(label), expected, "actual CR receipt for {label}");
    }

    fn control(&self) {
        self.session("control");
        assert_eq!(
            SubmissionTmux.pin_guarded("control").unwrap().enter(),
            InputRun::Applied
        );
        self.assert_enter("control", 1);
    }

    fn calls(&self) -> usize {
        std::fs::read(self.root.path().join("calls"))
            .unwrap_or_default()
            .len()
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new(&self.real)
            .args(["-L", &self.socket, "kill-server"])
            .output();
    }
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(3);
    while !ready() {
        assert!(Instant::now() < until, "fixture did not become ready");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn refused(run: InputRun) {
    assert_eq!(
        run,
        InputRun::Refused(InputRefusal::Composer(Refusal::UnpredictableRender))
    );
}

#[test]
fn cg_b2a_single_live_manual_160x40_enter_once() {
    let f = Fixture::new();
    f.session("positive");
    let pinned = SubmissionTmux.pin_guarded("positive").unwrap();
    assert!(pinned.geometry_matches());
    assert!(pinned.capture_draft().unwrap().contains("positive"));
    assert_eq!(pinned.enter(), InputRun::Applied);
    f.assert_enter("positive", 1);
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
    assert_eq!(f.calls(), 1);
}

#[test]
fn cg_b2a_snapshot_resize_manual_and_split_false_branch_enter_zero() {
    let f = Fixture::new();
    f.control();
    for mode in ["resize", "grow", "manual", "split"] {
        let pane = f.session(mode);
        let pinned = SubmissionTmux.pin_guarded(mode).unwrap();
        match mode {
            "resize" => {
                f.tmux(&["resize-window", "-t", &pane, "-x", "80", "-y", "24"]);
            }
            "grow" => {
                f.tmux(&["resize-window", "-t", &pane, "-x", "161", "-y", "40"]);
            }
            "manual" => {
                f.tmux(&["set-window-option", "-t", &pane, "window-size", "latest"]);
            }
            _ => {
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
            }
        }
        assert!(!pinned.geometry_matches(), "snapshot change: {mode}");
        refused(pinned.enter());
        f.assert_enter(mode, 0);
    }
    assert_eq!(f.calls(), 5);
}

#[test]
fn cg_b2a_replaced_same_name_and_dead_identity_never_enter() {
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
    assert!(!pinned.geometry_matches());
    assert!(pinned.capture_draft().is_none());
    assert_ne!(pinned.enter(), InputRun::Applied);
    f.assert_enter("new-receiver", 0);
    let pane = f.session("post-dead");
    let pinned = SubmissionTmux.pin_guarded("post-dead").unwrap();
    f.tmux(&["set-window-option", "-t", &pane, "remain-on-exit", "on"]);
    f.tmux(&["respawn-pane", "-k", "-t", &pane, "true"]);
    wait_until(|| f.tmux(&["display-message", "-p", "-t", &pane, "#{pane_dead}"]) == "1");
    assert!(!pinned.geometry_matches());
    assert_ne!(pinned.enter(), InputRun::Applied);
    f.assert_enter("post-dead", 0);
    let pane = f.session("respawn-original");
    let pinned = SubmissionTmux.pin_guarded("respawn-original").unwrap();
    let command = f.receiver("respawn-new");
    f.tmux(&["respawn-pane", "-k", "-t", &pane, &command]);
    f.wait_ready("respawn-new");
    assert!(!pinned.geometry_matches());
    refused(pinned.enter());
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
    assert!(!pinned.geometry_matches());
    refused(pinned.enter());
    f.assert_enter("restart-new", 0);
}

#[test]
fn cg_b2a_capture_and_enter_stay_on_pinned_pane_after_active_window_change() {
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
    assert!(pinned.geometry_matches());
    let capture = pinned.capture_draft().unwrap();
    assert!(capture.contains("original"), "{capture:?}");
    assert!(!capture.contains("other-"), "{capture:?}");
    assert_eq!(pinned.enter(), InputRun::Applied);
    f.assert_enter("original", 1);
    f.assert_enter("other-window", 0);
    f.assert_enter("other-pane", 0);
}

#[test]
fn cg_b2a_lost_nonzero_and_timeout_ack_are_indeterminate_without_retry() {
    let f = Fixture::new();
    f.control();
    for mode in ["drop", "nonzero", "timeout"] {
        f.session(mode);
        let pinned = SubmissionTmux.pin_guarded(mode).unwrap();
        std::fs::write(f.root.path().join("mode"), mode).unwrap();
        let before = f.calls();
        assert!(
            matches!(
                pinned.enter(),
                InputRun::Indeterminate {
                    confirmed: 0,
                    cause: StopCause::Send(_)
                }
            ),
            "ACK failure: {mode}"
        );
        f.assert_enter(mode, 1);
        assert_eq!(f.calls() - before, 1, "conditional Enter must never retry");
    }
}

#[test]
fn cg_b2a_dormant_default_and_empty_allowlist_keep_legacy_transport() {
    let f = Fixture::new();
    let defaults = crate::config::session_hosts::BootSessionHosts::default();
    assert!(!defaults.claude_guarded(1));
    let _empty = crate::config::session_hosts::force_for_test(None, &[]);
    assert!(!crate::config::session_hosts::claude_guarded(Some(1)));
    assert!(!crate::config::session_hosts::claude_guarded(None));
    f.session("legacy");
    let mut transport = SubmissionTmux;
    assert!(
        transport
            .capture_draft("legacy")
            .unwrap()
            .contains("legacy")
    );
    assert!(
        transport
            .send_keys("legacy", &[HostKey::Enter])
            .unwrap()
            .status
            .success()
    );
    f.assert_enter("legacy", 1);
    assert_eq!(f.calls(), 0);
}
