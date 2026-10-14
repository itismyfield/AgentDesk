#![cfg(unix)]

use super::*;
use crate::config::TestEnvVarGuard as Env;
use crate::services::claude_tui::host_input::InputTransport;
use crate::services::session_host::HostKey;
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
  if [ "$arg" = load-buffer ] && [ "$(cat {mode} 2>/dev/null)" = load_nonzero ]; then
    {real} -L {socket} -f /dev/null "$@" >/dev/null
    exit 1
  fi
  if [ "$arg" = capture-pane ] && [ "$(cat {mode} 2>/dev/null)" = capture_resize ]; then
    {real} -L {socket} resize-window -t =capture-race: -x 80 -y 24
  fi
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
        assert_eq!(
            SubmissionTmux.pin_guarded("control").unwrap().enter(),
            EffectOutcome::Applied
        );
        self.assert_enter("control", 1);
    }

    pub(crate) fn calls(&self) -> usize {
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

    fn attach(&self, session: &str) {
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
        if let Ok(output) = Command::new(&self.real)
            .args(["-L", &self.socket, "list-sessions", "-F", "#{session_name}"])
            .output()
        {
            for session in String::from_utf8_lossy(&output.stdout).lines() {
                crate::services::claude_tui::guarded_outcome::forget_test_hold(session);
            }
        }

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
fn cg_b2b_modes_and_attached_writer_refuse_pin_and_enter() {
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
        assert!(!pinned.geometry_matches(), "snapshot admitted {mode}");
        refused(pinned.enter());
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
fn cg_b2b_payload_literal_quotes_and_bracketed_paste_preserve_frame() {
    let f = Fixture::new();
    f.control();
    f.tmux(&["set-buffer", "-b", "foreign", "keep"]);
    for (label, action, expected) in [
        (
            "literal",
            TuiInputAction::Literal(" a  '\"; $HOME \\ #{pane_id} ~ 가😀 trailing  ".into()),
            b" a  '\"; $HOME \\ #{pane_id} ~ \xea\xb0\x80\xf0\x9f\x98\x80 trailing  ".to_vec(),
        ),
        (
            "paste",
            TuiInputAction::PasteBuffer("a  b\n\"'\\; $HOME #{pane_id}\n가 😀\n".into()),
            b"\x1b[200~a  b\n\"'\\; $HOME #{pane_id}\n\xea\xb0\x80 \xf0\x9f\x98\x80\n\x1b[201~"
                .to_vec(),
        ),
    ] {
        f.session(label);
        let pinned = SubmissionTmux.pin_guarded(label).unwrap();
        assert!(pinned.session_matches(label));
        assert!(!pinned.session_matches("another-session"));
        let before = f.calls();
        assert_eq!(pinned.payload(&action), EffectOutcome::Applied);
        wait_until(|| f.received(label).len() >= expected.len());
        assert_eq!(
            f.received(label),
            expected,
            "actual frame bytes for {label}"
        );
        assert_eq!(f.calls() - before, 1, "one conditional payload for {label}");
        f.assert_enter(label, 0);
        assert_eq!(f.tmux(&["list-buffers", "-F", "#{buffer_name}"]), "foreign");
        assert_eq!(pinned.enter(), EffectOutcome::Applied);
        f.assert_enter(label, 1);
    }
}

#[test]
fn cg_b2b_payload_false_branch_sends_zero_and_cleans_only_owned_buffer() {
    let f = Fixture::new();
    f.control();
    f.tmux(&["set-buffer", "-b", "foreign", "keep"]);
    for (label, action) in [
        ("literal-held", TuiInputAction::Literal("unsent".into())),
        (
            "paste-held",
            TuiInputAction::PasteBuffer("unsent\nframe".into()),
        ),
    ] {
        let pane = f.session(label);
        let pinned = SubmissionTmux.pin_guarded(label).unwrap();
        f.tmux(&["resize-window", "-t", &pane, "-x", "80", "-y", "24"]);
        let before = f.calls();
        assert_eq!(pinned.payload(&action), EffectOutcome::Held);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            f.received(label).is_empty(),
            "false branch wrote payload for {label}"
        );
        assert_eq!(f.calls() - before, 1);
        assert_eq!(f.tmux(&["list-buffers", "-F", "#{buffer_name}"]), "foreign");
        assert_eq!(f.tmux(&["show-buffer", "-b", "foreign"]), "keep");
        f.assert_enter(label, 0);
    }
}

#[test]
fn cg_b2b_payload_unknown_ack_is_not_retried_and_owned_buffer_is_cleaned() {
    let f = Fixture::new();
    f.control();
    f.tmux(&["set-buffer", "-b", "foreign", "keep"]);
    for (label, action, expected) in [
        (
            "literal-lost",
            TuiInputAction::Literal("once".into()),
            b"once".to_vec(),
        ),
        (
            "paste-lost",
            TuiInputAction::PasteBuffer("once\nframe".into()),
            b"\x1b[200~once\nframe\x1b[201~".to_vec(),
        ),
    ] {
        f.session(label);
        let pinned = SubmissionTmux.pin_guarded(label).unwrap();
        std::fs::write(f.root.path().join("mode"), "drop").unwrap();
        let before = f.calls();
        assert!(matches!(pinned.payload(&action), EffectOutcome::Unknown(_)));
        wait_until(|| f.received(label).len() >= expected.len());
        assert_eq!(f.received(label), expected);
        assert_eq!(f.calls() - before, 1);
        assert_eq!(f.tmux(&["list-buffers", "-F", "#{buffer_name}"]), "foreign");
        f.assert_enter(label, 0);
    }
}

#[test]
fn cg_b2b_unknown_buffer_load_cleans_only_its_uuid_and_never_sends() {
    let f = Fixture::new();
    f.control();
    f.tmux(&["set-buffer", "-b", "foreign", "keep"]);
    f.session("load-lost");
    let pinned = SubmissionTmux.pin_guarded("load-lost").unwrap();
    std::fs::write(f.root.path().join("mode"), "load_nonzero").unwrap();
    let before = f.calls();
    assert!(matches!(
        pinned.payload(&TuiInputAction::PasteBuffer("unsent\nframe".into())),
        EffectOutcome::Unknown(_)
    ));
    assert_eq!(
        f.calls(),
        before,
        "load failure reached no conditional pane mutation"
    );
    assert!(f.received("load-lost").is_empty());
    assert_eq!(f.tmux(&["list-buffers", "-F", "#{buffer_name}"]), "foreign");
    assert_eq!(f.tmux(&["show-buffer", "-b", "foreign"]), "keep");
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
    assert!(!pinned.geometry_matches());
    assert_eq!(
        pinned.payload(&TuiInputAction::Literal("unsent".into())),
        EffectOutcome::Held
    );
    refused(pinned.enter());
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

#[test]
fn cg_b2b_observer_keeps_original_identity_after_consumed_enter() {
    let f = Fixture::new();
    f.control();
    let pane = f.session("observe-original");
    let pinned = SubmissionTmux.pin_guarded("observe-original").unwrap();
    let observer = pinned.observer();
    assert_eq!(pinned.enter(), EffectOutcome::Applied);
    f.assert_enter("observe-original", 1);
    assert!(
        observer
            .capture_draft()
            .unwrap()
            .contains("observe-original")
    );
    let command = f.receiver("observe-replacement");
    f.tmux(&["respawn-pane", "-k", "-t", &pane, &command]);
    f.wait_ready("observe-replacement");
    assert!(
        observer.capture_draft().is_none(),
        "passive observer accepted a replaced process"
    );
    f.assert_enter("observe-replacement", 0);
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(3);
    while !ready() {
        assert!(Instant::now() < until, "fixture did not become ready");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn refused(run: EffectOutcome) {
    assert_eq!(run, EffectOutcome::Held);
}

#[test]
fn cg_b2a_single_live_manual_160x40_enter_once() {
    let f = Fixture::new();
    f.session("positive");
    let pinned = SubmissionTmux.pin_guarded("positive").unwrap();
    assert!(pinned.geometry_matches());
    assert!(pinned.capture_draft().unwrap().contains("positive"));
    assert_eq!(pinned.enter(), EffectOutcome::Applied);
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
    assert_ne!(pinned.enter(), EffectOutcome::Applied);
    f.assert_enter("new-receiver", 0);
    let pane = f.session("post-dead");
    let pinned = SubmissionTmux.pin_guarded("post-dead").unwrap();
    f.tmux(&["set-window-option", "-t", &pane, "remain-on-exit", "on"]);
    f.tmux(&["respawn-pane", "-k", "-t", &pane, "true"]);
    wait_until(|| f.tmux(&["display-message", "-p", "-t", &pane, "#{pane_dead}"]) == "1");
    assert!(!pinned.geometry_matches());
    assert_ne!(pinned.enter(), EffectOutcome::Applied);
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
    assert_eq!(pinned.enter(), EffectOutcome::Applied);
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
            matches!(pinned.enter(), EffectOutcome::Unknown(_)),
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
