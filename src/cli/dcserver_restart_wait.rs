use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use super::dcserver_restart_marker::{
    MarkerOwnership, QuickRestartMarker, RestartMarkerCreateError, create_quick_restart_marker,
};

const DEFERRED_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitTermination {
    AlreadyOwned,
    CreateFailedForceKilled,
    MarkerAcknowledged,
    ProcessGone,
    RemovedOwned,
    MissingCommitted,
    Replaced,
    ResolveFailed,
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
    fn resolve_ownership(&self, marker: &QuickRestartMarker) -> io::Result<MarkerOwnership>;
    fn force_kill(&self);
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

    fn resolve_ownership(&self, marker: &QuickRestartMarker) -> io::Result<MarkerOwnership> {
        marker.resolve_ownership(|| self.force_kill())
    }

    fn force_kill(&self) {
        super::dcserver::kill_existing_dcserver_processes();
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
    let ownership = loop {
        if !marker.path().exists() {
            effects.stdout("   ✓ dcserver acknowledged restart marker".to_string());
            return WaitTermination::MarkerAcknowledged;
        }
        if process_is_gone(root, effects) {
            effects.stdout("   ✓ dcserver process exited gracefully".to_string());
            return WaitTermination::ProcessGone;
        }
        if effects.monotonic().saturating_sub(start) >= DEFERRED_TIMEOUT {
            break match effects.resolve_ownership(marker) {
                Ok(ownership) => ownership,
                Err(error) => {
                    effects.stderr(format!(
                        "   ⚠ Failed to resolve restart marker ownership: {error}; refusing force-kill"
                    ));
                    return WaitTermination::ResolveFailed;
                }
            };
        }
        effects.sleep(POLL_INTERVAL);
    };

    match ownership {
        MarkerOwnership::RemovedOwned => {
            effects.stderr(
                "   ⚠ Deferred restart timeout — force-kill fallback completed".to_string(),
            );
            WaitTermination::RemovedOwned
        }
        MarkerOwnership::MissingCommitted => {
            effects.stdout("   ✓ dcserver acknowledged restart marker".to_string());
            WaitTermination::MissingCommitted
        }
        MarkerOwnership::Replaced(owner) => {
            effects.stderr(format!(
                "   ⚠ restart already owned ({owner}); preserving the replacement and refusing force-kill"
            ));
            WaitTermination::Replaced
        }
    }
}

fn run_quick_restart(
    root: Option<&Path>,
    version: &str,
    effects: &impl QuickRestartEffects,
) -> QuickRestartRun {
    let Some(root) = root else {
        effects.force_kill();
        return QuickRestartRun::NoRuntimeRoot;
    };

    let marker = match create_quick_restart_marker(root, version) {
        Ok(marker) => marker,
        Err(RestartMarkerCreateError::AlreadyOwned(owner)) => {
            effects.stderr(format!(
                "   ⚠ restart already owned ({owner}); preserving the existing restart and refusing force-kill"
            ));
            return QuickRestartRun::Handled(WaitTermination::AlreadyOwned);
        }
        Err(error) => {
            effects.stderr(format!(
                "   ⚠ Failed to write restart marker {}: {error} — falling back to force-kill",
                root.join("restart_pending").display()
            ));
            effects.force_kill();
            return QuickRestartRun::Handled(WaitTermination::CreateFailedForceKilled);
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
    use std::path::PathBuf;

    use super::*;

    #[derive(Debug)]
    enum ProbeAction {
        Alive,
        Gone,
        RemoveAndReachTimeout(PathBuf),
        ReplaceAndReachTimeout(PathBuf),
    }

    struct Recorder {
        now: Cell<Duration>,
        sleep_advance: Duration,
        sleeps: RefCell<Vec<Duration>>,
        stdout: RefCell<Vec<String>>,
        stderr: RefCell<Vec<String>>,
        force_kills: Cell<usize>,
        resolve_calls: Cell<usize>,
        probe_action: RefCell<ProbeAction>,
        resolve_error: Cell<bool>,
    }

    impl Default for Recorder {
        fn default() -> Self {
            Self {
                now: Cell::new(Duration::ZERO),
                sleep_advance: DEFERRED_TIMEOUT,
                sleeps: RefCell::new(Vec::new()),
                stdout: RefCell::new(Vec::new()),
                stderr: RefCell::new(Vec::new()),
                force_kills: Cell::new(0),
                resolve_calls: Cell::new(0),
                probe_action: RefCell::new(ProbeAction::Alive),
                resolve_error: Cell::new(false),
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
                ProbeAction::RemoveAndReachTimeout(path) => {
                    fs::remove_file(path).unwrap();
                    self.now.set(DEFERRED_TIMEOUT);
                    true
                }
                ProbeAction::ReplaceAndReachTimeout(path) => {
                    fs::remove_file(&path).unwrap();
                    fs::write(
                        path,
                        "nonce=replacement-owner\nsource=deploy-release\nscope=release\n",
                    )
                    .unwrap();
                    self.now.set(DEFERRED_TIMEOUT);
                    true
                }
            }
        }

        fn stdout(&self, line: String) {
            self.stdout.borrow_mut().push(line);
        }

        fn stderr(&self, line: String) {
            self.stderr.borrow_mut().push(line);
        }

        fn resolve_ownership(&self, marker: &QuickRestartMarker) -> io::Result<MarkerOwnership> {
            self.resolve_calls.set(self.resolve_calls.get() + 1);
            if self.resolve_error.get() {
                return Err(io::Error::other("injected resolve failure"));
            }
            marker.resolve_ownership(|| self.force_kill())
        }

        fn force_kill(&self) {
            self.force_kills.set(self.force_kills.get() + 1);
        }
    }

    fn write_pid(root: &Path) {
        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        fs::write(runtime.join("dcserver.pid"), "4242\n").unwrap();
    }

    fn start_line() -> String {
        "   ⏳ Restart requested — waiting for dcserver quick-exit (max 30s)".to_string()
    }

    #[test]
    fn s5a_no_runtime_root_force_kills_once_and_falls_through() {
        let recorder = Recorder::default();

        let result = run_quick_restart(None, "test", &recorder);

        assert_eq!(result, QuickRestartRun::NoRuntimeRoot);
        assert_eq!(result.outcome(), QuickRestartOutcome::NoRuntimeRoot);
        assert!(recorder.stdout.borrow().is_empty());
        assert!(recorder.stderr.borrow().is_empty());
        assert!(recorder.sleeps.borrow().is_empty());
        assert_eq!(recorder.resolve_calls.get(), 0);
        assert_eq!(recorder.force_kills.get(), 1);
    }

    #[test]
    fn s5a_already_owned_preserves_owner_and_refuses_force_kill() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("restart_pending"),
            "nonce=owner-nonce\nsource=deploy-release\nscope=release\n",
        )
        .unwrap();
        let recorder = Recorder::default();

        let result = run_quick_restart(Some(root.path()), "test", &recorder);

        assert_eq!(
            result,
            QuickRestartRun::Handled(WaitTermination::AlreadyOwned)
        );
        assert!(recorder.stdout.borrow().is_empty());
        assert_eq!(
            *recorder.stderr.borrow(),
            vec!["   ⚠ restart already owned (source=deploy-release, scope=release, nonce=owner-nonce); preserving the existing restart and refusing force-kill".to_string()]
        );
        assert_eq!(recorder.resolve_calls.get(), 0);
        assert_eq!(recorder.force_kills.get(), 0);
    }

    #[test]
    fn s5a_create_io_failure_force_kills_once() {
        let root_file = tempfile::NamedTempFile::new().unwrap();
        let recorder = Recorder::default();

        let result = run_quick_restart(Some(root_file.path()), "test", &recorder);

        assert_eq!(
            result,
            QuickRestartRun::Handled(WaitTermination::CreateFailedForceKilled)
        );
        assert!(recorder.stdout.borrow().is_empty());
        assert_eq!(recorder.stderr.borrow().len(), 1);
        assert!(recorder.stderr.borrow()[0].starts_with(&format!(
            "   ⚠ Failed to write restart marker {}: ",
            root_file.path().join("restart_pending").display()
        )));
        assert!(recorder.stderr.borrow()[0].ends_with(" — falling back to force-kill"));
        assert_eq!(recorder.resolve_calls.get(), 0);
        assert_eq!(recorder.force_kills.get(), 1);
    }

    #[test]
    fn s5a_marker_absence_acknowledges_immediately() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "test").unwrap();
        fs::remove_file(marker.path()).unwrap();
        let recorder = Recorder::default();

        let result = wait_for_created_marker(root.path(), &marker, &recorder);

        assert_eq!(result, WaitTermination::MarkerAcknowledged);
        assert_eq!(
            *recorder.stdout.borrow(),
            vec![
                start_line(),
                "   ✓ dcserver acknowledged restart marker".to_string()
            ]
        );
        assert!(recorder.stderr.borrow().is_empty());
        assert!(recorder.sleeps.borrow().is_empty());
        assert_eq!(recorder.resolve_calls.get(), 0);
        assert_eq!(recorder.force_kills.get(), 0);
    }

    #[test]
    fn s5a_process_gone_reports_graceful_exit_without_force_kill() {
        let root = tempfile::tempdir().unwrap();
        write_pid(root.path());
        let recorder = Recorder::default();
        *recorder.probe_action.borrow_mut() = ProbeAction::Gone;

        let result = run_quick_restart(Some(root.path()), "test", &recorder);

        assert_eq!(
            result,
            QuickRestartRun::Handled(WaitTermination::ProcessGone)
        );
        assert_eq!(
            *recorder.stdout.borrow(),
            vec![
                start_line(),
                "   ✓ dcserver process exited gracefully".to_string()
            ]
        );
        assert!(recorder.stderr.borrow().is_empty());
        assert_eq!(recorder.resolve_calls.get(), 0);
        assert_eq!(recorder.force_kills.get(), 0);
    }

    #[test]
    fn s5a_timeout_removed_owned_force_kills_once() {
        let root = tempfile::tempdir().unwrap();
        let recorder = Recorder::default();

        let result = run_quick_restart(Some(root.path()), "test", &recorder);

        assert_eq!(
            result,
            QuickRestartRun::Handled(WaitTermination::RemovedOwned)
        );
        assert_eq!(*recorder.stdout.borrow(), vec![start_line()]);
        assert_eq!(
            *recorder.stderr.borrow(),
            vec!["   ⚠ Deferred restart timeout — force-kill fallback completed".to_string()]
        );
        assert_eq!(*recorder.sleeps.borrow(), vec![Duration::from_millis(500)]);
        assert_eq!(recorder.resolve_calls.get(), 1);
        assert_eq!(recorder.force_kills.get(), 1);
    }

    #[test]
    fn s5a_timeout_missing_marker_preserves_acknowledgement() {
        let root = tempfile::tempdir().unwrap();
        write_pid(root.path());
        let recorder = Recorder::default();
        *recorder.probe_action.borrow_mut() =
            ProbeAction::RemoveAndReachTimeout(root.path().join("restart_pending"));

        let result = run_quick_restart(Some(root.path()), "test", &recorder);

        assert_eq!(
            result,
            QuickRestartRun::Handled(WaitTermination::MissingCommitted)
        );
        assert_eq!(
            *recorder.stdout.borrow(),
            vec![
                start_line(),
                "   ✓ dcserver acknowledged restart marker".to_string()
            ]
        );
        assert!(recorder.stderr.borrow().is_empty());
        assert_eq!(recorder.resolve_calls.get(), 1);
        assert_eq!(recorder.force_kills.get(), 0);
    }

    #[test]
    fn s5a_timeout_replacement_is_preserved_without_force_kill() {
        let root = tempfile::tempdir().unwrap();
        write_pid(root.path());
        let recorder = Recorder::default();
        *recorder.probe_action.borrow_mut() =
            ProbeAction::ReplaceAndReachTimeout(root.path().join("restart_pending"));

        let result = run_quick_restart(Some(root.path()), "test", &recorder);

        assert_eq!(result, QuickRestartRun::Handled(WaitTermination::Replaced));
        assert_eq!(*recorder.stdout.borrow(), vec![start_line()]);
        assert_eq!(
            *recorder.stderr.borrow(),
            vec!["   ⚠ restart already owned (source=deploy-release, scope=release, nonce=replacement-owner); preserving the replacement and refusing force-kill".to_string()]
        );
        assert_eq!(recorder.resolve_calls.get(), 1);
        assert_eq!(recorder.force_kills.get(), 0);
    }

    #[test]
    fn s5a_resolve_error_refuses_force_kill() {
        let root = tempfile::tempdir().unwrap();
        let recorder = Recorder::default();
        recorder.resolve_error.set(true);

        let result = run_quick_restart(Some(root.path()), "test", &recorder);

        assert_eq!(
            result,
            QuickRestartRun::Handled(WaitTermination::ResolveFailed)
        );
        assert_eq!(*recorder.stdout.borrow(), vec![start_line()]);
        assert_eq!(
            *recorder.stderr.borrow(),
            vec!["   ⚠ Failed to resolve restart marker ownership: injected resolve failure; refusing force-kill".to_string()]
        );
        assert_eq!(recorder.resolve_calls.get(), 1);
        assert_eq!(recorder.force_kills.get(), 0);
    }
}
