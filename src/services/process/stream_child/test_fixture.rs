use super::super::ProcessIdentity;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

pub(crate) struct ProviderFixture {
    dir: tempfile::TempDir,
    pub(crate) cli: PathBuf,
    parent_group: i32,
    cleanup: mpsc::Receiver<(u32, ProcessIdentity)>,
}
pub(crate) const CASES: [&str; 5] = ["error", "normal", "continuous", "escaped", "quiet"];
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
        std::fs::write(&cli, format!(r#"#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys, time
root = pathlib.Path(__file__).parent
mode = {mode:?}
print({progress:?}, flush=True)
code = '''import pathlib, sys, time
root = pathlib.Path(sys.argv[1])
(root / "ready").touch()
end = time.monotonic() + 8
while time.monotonic() < end:
    if sys.argv[2] == "continuous": print('{{}}', flush=True)
    time.sleep(.005)
(root / "fd-closed").touch()
'''
child = subprocess.Popen([sys.executable, '-c', code, str(root), mode], start_new_session=mode == 'escaped')
(root / 'identity').write_text(json.dumps([os.getpid(), os.getpgrp(), child.pid, os.getpgid(child.pid)]))
while not (root / 'ready').exists() or not (root / 'owner-ack').exists(): time.sleep(.001)
if mode == 'cancel': time.sleep(8)
if mode == 'quiet':
    time.sleep(.3)
    (root / 'quiet-finished').touch()
if mode in ('normal', 'quiet'): print({terminal:?}, flush=True)
sys.exit(0 if mode in ('normal', 'quiet') else 7)
"#)).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.path().to_owned();
        let (tx, cleanup) = mpsc::channel();
        std::thread::spawn(move || {
            for _ in 0..5000 {
                if let Ok(text) = std::fs::read_to_string(path.join("identity")) {
                    if let Ok(ids) = serde_json::from_str::<Vec<u32>>(&text) {
                        let identity = ProcessIdentity::capture(ids[2]);
                        tx.send((ids[2], identity)).unwrap();
                        std::fs::write(path.join("owner-ack"), "").unwrap();
                        return;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
        Self {
            dir,
            cli,
            parent_group: unsafe { libc::getpgrp() },
            cleanup,
        }
    }
    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }
    pub(crate) fn cancel_after_accept(
        &self,
        token: std::sync::Arc<crate::services::provider::CancelToken>,
    ) -> std::thread::JoinHandle<()> {
        let path = self.path().to_owned();
        std::thread::spawn(move || {
            for _ in 0..5000 {
                if path.join("owner-ack").exists() {
                    let ids: Vec<u32> = serde_json::from_str(
                        &std::fs::read_to_string(path.join("identity")).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(token.child_pid_value(), Some(ids[0]));
                    token.publish_cancel("manual_cancel");
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            panic!("fake provider never accepted");
        })
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
        if let Ok((pid, identity)) = self.cleanup.recv_timeout(std::time::Duration::from_secs(1)) {
            if identity.persisted_starttime().is_some()
                || identity.persisted_macos_lstart_hash().is_some()
            {
                super::super::kill_pid_tree_if_identity_matches(pid, identity);
            }
        }
    }
}
