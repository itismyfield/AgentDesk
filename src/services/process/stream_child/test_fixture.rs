use super::super::ProcessIdentity;
use crate::services::provider::CancelToken;
use crate::test_support::wait::{WAIT_CAP, join_within};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

pub(crate) struct ProviderFixture {
    dir: tempfile::TempDir,
    pub(crate) cli: PathBuf,
    parent_group: i32,
    cleanup: mpsc::Receiver<(u32, ProcessIdentity)>,
    stop_owner_probe: mpsc::Sender<()>,
    delay: Option<super::stream_queue::test_delay::Guard>,
}
pub(crate) const CASES: [&str; 6] = [
    "error",
    "normal",
    "continuous",
    "escaped",
    "escaped_continuous",
    "quiet",
];
impl ProviderFixture {
    pub(crate) fn new(provider: &str, mode: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("provider");
        let progress = match provider {
            "codex" => r#"{"type":"thread.started","thread_id":"fixture"}"#,
            "gemini" => r#"{"type":"message","role":"assistant","content":"working"}"#,
            _ => r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
        };
        let terminal = match provider {
            "codex" => {
                "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"done\"}}\n{\"type\":\"turn.completed\"}"
            }
            "gemini" => r#"{"type":"result","status":"success","result":"done"}"#,
            _ => r#"{"type":"result","subtype":"success","result":"done"}"#,
        };
        let source = dir.path().join("provider.src");
        std::fs::write(&source, format!(r#"#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys, time
root = pathlib.Path(__file__).parent
mode = {mode:?}
print({progress:?}, flush=True)
code = '''import pathlib, sys, time
root = pathlib.Path(sys.argv[1])
(root / "ready").touch()
end = time.monotonic() + 8
while time.monotonic() < end:
    if sys.argv[2].endswith("continuous"): print('{{}}', flush=True)
    time.sleep(.005)
(root / "fd-closed").touch()
'''
child = subprocess.Popen([sys.executable, '-c', code, str(root), mode], start_new_session=mode.startswith('escaped'), stdout=subprocess.DEVNULL if mode == 'delayed_normal' else None, stderr=subprocess.DEVNULL if mode == 'delayed_normal' else None)
(root / 'identity').write_text(json.dumps([os.getpid(), os.getpgrp(), child.pid, os.getpgid(child.pid)]))
owner_deadline = time.monotonic() + 5
while not (root / 'ready').exists() or not (root / 'owner-ack').exists():
    if time.monotonic() >= owner_deadline or child.poll() is not None:
        child.kill()
        child.wait(timeout=1)
        (root / 'owner-handshake-failed').touch()
        sys.exit("fake provider owner handshake failed")
    time.sleep(.001)
if mode == 'cancel': time.sleep(8)
if mode == 'quiet':
    time.sleep(.3)
    (root / 'quiet-finished').touch()
if mode in ('normal', 'quiet', 'delayed_normal'): print({terminal:?}, flush=True)
sys.exit(0 if mode in ('normal', 'quiet', 'delayed_normal') else 7)
"#)).unwrap();
        // A fork on another thread holds any writable fd on the script until its exec, failing
        // our exec with ETXTBSY; `cp` writes it in its own process so no such fd exists here.
        let copied = std::process::Command::new("/bin/cp")
            .arg(&source)
            .arg(&cli)
            .status()
            .unwrap();
        assert!(copied.success(), "cp provider script: {copied}");
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.path().to_owned();
        let (tx, cleanup) = mpsc::channel();
        let (stop_owner_probe, stop) = mpsc::channel();
        std::thread::spawn(move || {
            // Launch can include shell discovery before the provider publishes its identity.
            if let Some([_, _, pid, _]) = wait_for_fixture(&stop, || read_identity(&path))
                && tx.send((pid, ProcessIdentity::capture(pid))).is_ok()
            {
                let _ = std::fs::write(path.join("owner-ack"), "");
            }
        });
        Self {
            dir,
            cli,
            parent_group: unsafe { libc::getpgrp() },
            cleanup,
            stop_owner_probe,
            delay: (mode == "delayed_normal").then(super::stream_queue::test_delay::arm),
        }
    }
    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }
    pub(crate) fn run_with_cancel_after_accept<T>(
        &self,
        token: Arc<CancelToken>,
        run: impl FnOnce() -> T,
    ) -> T {
        let path = self.path().to_owned();
        let (finished, stop) = mpsc::channel();
        let cancel = std::thread::spawn(move || {
            let accepted = wait_for_fixture(&stop, || {
                (path.join("ready").is_file() && path.join("owner-ack").is_file())
                    .then(|| read_identity(&path))
                    .flatten()
            });
            let Some([pid, ..]) = accepted else {
                return false;
            };
            assert_eq!(token.child_pid_value(), Some(pid));
            token.publish_cancel("manual_cancel");
            true
        });
        let result = run();
        drop(finished);
        assert!(
            join_within(cancel, WAIT_CAP),
            "fake provider never accepted"
        );
        result
    }
    pub(crate) fn resolution(&self) -> crate::services::platform::BinaryResolution {
        crate::services::platform::BinaryResolution {
            requested_binary: self.cli.display().to_string(),
            resolved_path: Some(self.cli.display().to_string()),
            canonical_path: None,
            source: None,
            attempts: vec![],
            failure_kind: None,
            exec_path: None,
        }
    }
    pub(crate) fn verify_return(&self, mode: &str) {
        assert!(
            self.path().join("ready").is_file()
                && self.path().join("owner-ack").is_file()
                && !self.path().join("owner-handshake-failed").exists(),
            "fake provider owner handshake failed in {mode}"
        );
        if let Some(delay) = &self.delay {
            delay.verify();
        }
        let ids: Vec<i32> =
            serde_json::from_str(&std::fs::read_to_string(self.path().join("identity")).unwrap())
                .unwrap();
        assert_eq!(ids[0], ids[1], "provider must own its process group");
        assert_ne!(ids[1], self.parent_group);
        assert_eq!(unsafe { libc::getpgrp() }, self.parent_group);
        assert!(
            !self.path().join("fd-closed").exists(),
            "provider return waited for descendant-held stdout/stderr"
        );
        if mode == "quiet" {
            assert!(self.path().join("quiet-finished").exists());
        }
    }
}
impl Drop for ProviderFixture {
    fn drop(&mut self) {
        let _ = self.stop_owner_probe.send(());
        if let Ok((pid, identity)) = self.cleanup.recv_timeout(Duration::from_secs(1)) {
            if identity.persisted_starttime().is_some()
                || identity.persisted_macos_lstart_hash().is_some()
            {
                super::super::kill_pid_tree_if_identity_matches(pid, identity);
            }
        }
    }
}

fn read_identity(path: &Path) -> Option<[u32; 4]> {
    serde_json::from_str(&std::fs::read_to_string(path.join("identity")).ok()?).ok()
}

fn wait_for_fixture<T>(stop: &mpsc::Receiver<()>, probe: impl Fn() -> Option<T>) -> Option<T> {
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if stop.recv_timeout(Duration::from_millis(1)) != Err(mpsc::RecvTimeoutError::Timeout) {
            return None;
        }
    }
}
