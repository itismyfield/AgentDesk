use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use super::dcserver_restart_marker::{
    QuickRestartMarker, RestartMarkerCreateError, create_quick_restart_marker,
};
#[cfg(test)]
use super::restart_terminal_proof::TerminalProof;
use super::restart_terminal_proof::{RestartTerminalProof, terminal_proof};

const DEFERRED_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitTermination {
    AlreadyOwned,
    CreateFailed,
    Persisted,
    Cancelled,
    ProcessGoneWithoutProof,
    TimeoutWithoutProof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuickRestartRun {
    Handled(WaitTermination),
    NoRuntimeRoot,
}

impl QuickRestartRun {
    fn outcome(self) -> QuickRestartOutcome {
        match self {
            Self::Handled(_) => QuickRestartOutcome::Handled,
            Self::NoRuntimeRoot => QuickRestartOutcome::NoRuntimeRoot,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QuickRestartOutcome {
    Handled,
    NoRuntimeRoot,
}

trait QuickRestartEffects {
    fn monotonic(&self) -> Duration;
    fn sleep(&self, duration: Duration);
    fn process_alive(&self, pid: u32) -> bool;
    fn stdout(&self, line: String);
    fn stderr(&self, line: String);
    fn terminal_proof(&self, root: &Path, nonce: &str) -> RestartTerminalProof;
}

struct ProductionEffects {
    origin: Instant,
}

impl ProductionEffects {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl QuickRestartEffects for ProductionEffects {
    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn process_alive(&self, pid: u32) -> bool {
        #[cfg(unix)]
        {
            let status = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            matches!(status, Ok(status) if status.success())
        }
        #[cfg(not(unix))]
        {
            let status = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output();
            matches!(status, Ok(output) if String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
        }
    }

    fn stdout(&self, line: String) {
        println!("{line}");
    }

    fn stderr(&self, line: String) {
        eprintln!("{line}");
    }

    fn terminal_proof(&self, root: &Path, nonce: &str) -> RestartTerminalProof {
        terminal_proof(root, nonce)
    }
}

fn process_is_gone(root: &Path, effects: &impl QuickRestartEffects) -> bool {
    let pid_file = root.join("runtime").join("dcserver.pid");
    let Ok(pid_str) = fs::read_to_string(pid_file) else {
        return false;
    };
    let Ok(pid) = pid_str.trim().parse::<u32>() else {
        return false;
    };
    !effects.process_alive(pid)
}

fn wait_for_created_marker(
    root: &Path,
    marker: &QuickRestartMarker,
    effects: &impl QuickRestartEffects,
) -> WaitTermination {
    effects.stdout(format!(
        "   ⏳ Restart requested — waiting for dcserver quick-exit (max {}s)",
        DEFERRED_TIMEOUT.as_secs()
    ));

    let start = effects.monotonic();
    let mut reported_missing_marker = false;
    loop {
        match effects.terminal_proof(root, marker.nonce()) {
            RestartTerminalProof::Persisted => {
                effects.stdout("   ✓ dcserver acknowledged restart marker".to_string());
                return WaitTermination::Persisted;
            }
            RestartTerminalProof::Cancelled => {
                effects.stderr("   ⚠ dcserver cancelled this restart".to_string());
                return WaitTermination::Cancelled;
            }
            RestartTerminalProof::Pending => {}
        }

        if !marker.path().exists() && !reported_missing_marker {
            effects.stdout(
                "   … restart marker disappeared without terminal proof; still waiting".to_string(),
            );
            reported_missing_marker = true;
        }

        if process_is_gone(root, effects) {
            match effects.terminal_proof(root, marker.nonce()) {
                RestartTerminalProof::Persisted => {
                    effects.stdout("   ✓ dcserver acknowledged restart marker".to_string());
                    return WaitTermination::Persisted;
                }
                RestartTerminalProof::Cancelled => {
                    effects.stderr("   ⚠ dcserver cancelled this restart".to_string());
                    return WaitTermination::Cancelled;
                }
                RestartTerminalProof::Pending => {
                    effects.stderr(
                        "   ⚠ dcserver process exited without terminal proof for this restart"
                            .to_string(),
                    );
                    return WaitTermination::ProcessGoneWithoutProof;
                }
            }
        }

        if effects.monotonic().saturating_sub(start) >= DEFERRED_TIMEOUT {
            effects.stderr(
                "   ⚠ Deferred restart timeout without terminal proof; preserving restart marker"
                    .to_string(),
            );
            return WaitTermination::TimeoutWithoutProof;
        }
        effects.sleep(POLL_INTERVAL);
    }
}

fn run_quick_restart(
    root: Option<&Path>,
    version: &str,
    effects: &impl QuickRestartEffects,
) -> QuickRestartRun {
    let Some(root) = root else {
        return QuickRestartRun::NoRuntimeRoot;
    };

    let marker = match create_quick_restart_marker(root, version) {
        Ok(marker) => marker,
        Err(RestartMarkerCreateError::AlreadyOwned(owner)) => {
            effects.stderr(format!(
                "   ⚠ restart already owned ({owner}); preserving the existing restart"
            ));
            return QuickRestartRun::Handled(WaitTermination::AlreadyOwned);
        }
        Err(error) => {
            effects.stderr(format!(
                "   ⚠ Failed to write restart marker {}: {error}",
                root.join("restart_pending").display()
            ));
            return QuickRestartRun::Handled(WaitTermination::CreateFailed);
        }
    };

    QuickRestartRun::Handled(wait_for_created_marker(root, &marker, effects))
}

pub(super) fn run_quick_restart_with_production_effects(
    root: Option<&Path>,
    version: &str,
) -> QuickRestartOutcome {
    let effects = ProductionEffects::new();
    run_quick_restart(root, version, &effects).outcome()
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    #[derive(Debug)]
    enum ProbeAction {
        Alive,
        Gone,
        PublishPersistedAndGone(std::path::PathBuf, String),
    }

    struct Recorder {
        now: Cell<Duration>,
        sleep_advance: Duration,
        sleeps: RefCell<Vec<Duration>>,
        stdout: RefCell<Vec<String>>,
        stderr: RefCell<Vec<String>>,
        probe_action: RefCell<ProbeAction>,
    }

    impl Default for Recorder {
        fn default() -> Self {
            Self {
                now: Cell::new(Duration::ZERO),
                sleep_advance: DEFERRED_TIMEOUT,
                sleeps: RefCell::new(Vec::new()),
                stdout: RefCell::new(Vec::new()),
                stderr: RefCell::new(Vec::new()),
                probe_action: RefCell::new(ProbeAction::Alive),
            }
        }
    }

    impl QuickRestartEffects for Recorder {
        fn monotonic(&self) -> Duration {
            self.now.get()
        }

        fn sleep(&self, duration: Duration) {
            self.sleeps.borrow_mut().push(duration);
            self.now.set(self.now.get() + self.sleep_advance);
        }

        fn process_alive(&self, _pid: u32) -> bool {
            match std::mem::replace(&mut *self.probe_action.borrow_mut(), ProbeAction::Alive) {
                ProbeAction::Alive => true,
                ProbeAction::Gone => false,
                ProbeAction::PublishPersistedAndGone(root, nonce) => {
                    write_terminal(&root, "restart_persisted", &nonce);
                    false
                }
            }
        }

        fn stdout(&self, line: String) {
            self.stdout.borrow_mut().push(line);
        }

        fn stderr(&self, line: String) {
            self.stderr.borrow_mut().push(line);
        }

        fn terminal_proof(&self, root: &Path, nonce: &str) -> RestartTerminalProof {
            terminal_proof(root, nonce)
        }
    }

    fn write_pid(root: &Path) {
        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        fs::write(runtime.join("dcserver.pid"), "4242\n").unwrap();
    }

    fn write_terminal(root: &Path, name: &str, nonce: &str) {
        fs::write(
            root.join(format!("{name}.{nonce}")),
            format!("nonce={nonce}\n"),
        )
        .unwrap();
    }

    fn start_line() -> String {
        "   ⏳ Restart requested — waiting for dcserver quick-exit (max 30s)".to_string()
    }

    #[test]
    fn s5b_no_runtime_root_uses_standard_fallback_without_direct_kill() {
        let recorder = Recorder::default();
        let result = run_quick_restart(None, "test", &recorder);
        assert_eq!(result, QuickRestartRun::NoRuntimeRoot);
        assert_eq!(result.outcome(), QuickRestartOutcome::NoRuntimeRoot);
        assert!(recorder.stdout.borrow().is_empty());
        assert!(recorder.stderr.borrow().is_empty());
    }

    #[test]
    fn s5b_already_owned_preserves_owner_and_refuses_restart() {
        let root = tempfile::tempdir().unwrap();
        let existing = "nonce=owner-nonce\nsource=deploy-release\nscope=release\n";
        fs::write(root.path().join("restart_pending"), existing).unwrap();
        let recorder = Recorder::default();
        let result = run_quick_restart(Some(root.path()), "test", &recorder);
        assert_eq!(
            result,
            QuickRestartRun::Handled(WaitTermination::AlreadyOwned)
        );
        assert_eq!(
            fs::read_to_string(root.path().join("restart_pending")).unwrap(),
            existing
        );
        assert_eq!(recorder.stderr.borrow().len(), 1);
    }

    #[test]
    fn s5b_create_io_failure_is_handled_without_direct_kill() {
        let root_file = tempfile::NamedTempFile::new().unwrap();
        let recorder = Recorder::default();
        let result = run_quick_restart(Some(root_file.path()), "test", &recorder);
        assert_eq!(
            result,
            QuickRestartRun::Handled(WaitTermination::CreateFailed)
        );
        assert!(recorder.stderr.borrow()[0].starts_with("   ⚠ Failed to write restart marker "));
    }

    #[test]
    fn s5b_persisted_proof_acknowledges_with_marker_present() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "test").unwrap();
        write_terminal(root.path(), "restart_persisted", marker.nonce());
        let recorder = Recorder::default();
        let result = wait_for_created_marker(root.path(), &marker, &recorder);
        assert_eq!(result, WaitTermination::Persisted);
        assert!(marker.path().exists());
        assert_eq!(
            *recorder.stdout.borrow(),
            vec![
                start_line(),
                "   ✓ dcserver acknowledged restart marker".to_string()
            ]
        );
    }

    #[test]
    fn s5b_persisted_proof_acknowledges_after_marker_disappears() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "test").unwrap();
        fs::remove_file(marker.path()).unwrap();
        write_terminal(root.path(), "restart_persisted", marker.nonce());
        let recorder = Recorder::default();
        let result = wait_for_created_marker(root.path(), &marker, &recorder);
        assert_eq!(result, WaitTermination::Persisted);
        assert!(recorder.stderr.borrow().is_empty());
    }

    #[test]
    fn s5b_cancelled_proof_is_non_success() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "test").unwrap();
        write_terminal(root.path(), "restart_cancelled", marker.nonce());
        let recorder = Recorder::default();
        let result = wait_for_created_marker(root.path(), &marker, &recorder);
        assert_eq!(result, WaitTermination::Cancelled);
        assert!(marker.path().exists());
        assert!(
            recorder
                .stdout
                .borrow()
                .iter()
                .all(|line| !line.contains("acknowledged"))
        );
    }

    #[test]
    fn s5b_disappeared_pending_is_diagnostic_then_times_out() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "test").unwrap();
        fs::remove_file(marker.path()).unwrap();
        let recorder = Recorder::default();
        let result = wait_for_created_marker(root.path(), &marker, &recorder);
        assert_eq!(result, WaitTermination::TimeoutWithoutProof);
        assert!(
            recorder
                .stdout
                .borrow()
                .iter()
                .any(|line| line.contains("disappeared without terminal proof"))
        );
        assert!(
            recorder
                .stdout
                .borrow()
                .iter()
                .all(|line| !line.contains("acknowledged"))
        );
    }

    #[test]
    fn s5b_process_gone_pending_is_non_success() {
        let root = tempfile::tempdir().unwrap();
        write_pid(root.path());
        let marker = create_quick_restart_marker(root.path(), "test").unwrap();
        let recorder = Recorder::default();
        *recorder.probe_action.borrow_mut() = ProbeAction::Gone;
        let result = wait_for_created_marker(root.path(), &marker, &recorder);
        assert_eq!(result, WaitTermination::ProcessGoneWithoutProof);
        assert!(marker.path().exists());
    }

    #[test]
    fn s5b_persisted_proof_published_during_liveness_probe_wins() {
        let root = tempfile::tempdir().unwrap();
        write_pid(root.path());
        let marker = create_quick_restart_marker(root.path(), "test").unwrap();
        let recorder = Recorder::default();
        *recorder.probe_action.borrow_mut() = ProbeAction::PublishPersistedAndGone(
            root.path().to_path_buf(),
            marker.nonce().to_string(),
        );

        let result = wait_for_created_marker(root.path(), &marker, &recorder);

        assert_eq!(result, WaitTermination::Persisted);
        assert!(marker.path().exists());
        assert_eq!(
            recorder
                .stdout
                .borrow()
                .iter()
                .filter(|line| line.contains("acknowledged"))
                .count(),
            1
        );
    }

    #[test]
    fn s5b_timeout_preserves_canonical_marker_bytes() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "test").unwrap();
        let before = fs::read(marker.path()).unwrap();
        let recorder = Recorder::default();
        let result = wait_for_created_marker(root.path(), &marker, &recorder);
        assert_eq!(result, WaitTermination::TimeoutWithoutProof);
        assert_eq!(fs::read(marker.path()).unwrap(), before);
        assert_eq!(*recorder.sleeps.borrow(), vec![POLL_INTERVAL]);
    }

    #[test]
    fn s5b_legacy_fixed_index_is_not_terminal_proof() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "test").unwrap();
        fs::write(
            root.path().join("restart_persisted"),
            format!("nonce={}\n", marker.nonce()),
        )
        .unwrap();
        let recorder = Recorder::default();
        let result = wait_for_created_marker(root.path(), &marker, &recorder);
        assert_eq!(result, WaitTermination::TimeoutWithoutProof);
        assert!(marker.path().exists());

        write_terminal(root.path(), "restart_cancelled", marker.nonce());
        assert_eq!(
            terminal_proof(root.path(), marker.nonce()),
            RestartTerminalProof::Pending,
            "a legacy persisted index suppresses cancellation without becoming success proof"
        );

        let mut persisted_reads = [TerminalProof::Absent, TerminalProof::Proven].into_iter();
        assert_eq!(
            super::super::restart_terminal_proof::terminal_proof_with_test_readers(
                || persisted_reads.next().expect("two persisted reads"),
                || TerminalProof::Proven,
            ),
            RestartTerminalProof::Persisted,
            "persisted published after the first read must win over cancellation"
        );
        assert!(persisted_reads.next().is_none());
    }
}
