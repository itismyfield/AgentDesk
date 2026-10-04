//! Herdr admission kill switch: `ADK_HERDR_ADMISSION`, read once, and a stop file that, once
//! seen, keeps admission stopped until restart. It only stops new Herdr work; nothing is torn down.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) const ADMISSION_ENV: &str = "ADK_HERDR_ADMISSION";
const STOP_FILE: [&str; 2] = ["herdr", "admission-off"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopCause {
    /// The env value is `off` or anything other than `on`.
    Env,
    /// The stop file exists now or existed earlier in this process.
    File,
    /// The stop file could not be checked (no runtime root, permission error).
    ProbeError,
}

impl StopCause {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::File => "file",
            Self::ProbeError => "probe_error",
        }
    }
}

#[derive(Debug)]
pub(crate) struct Admission {
    env_stopped: bool,
    stop_file: Option<PathBuf>,
    latched: AtomicBool,
}

impl Admission {
    /// An unset env value admits; `on` admits; any other value, a typo included, stops.
    pub(crate) fn new(env: Option<&std::ffi::OsStr>, stop_file: Option<PathBuf>) -> Self {
        Self {
            env_stopped: env.is_some_and(|value| value != "on"),
            stop_file,
            latched: AtomicBool::new(false),
        }
    }

    /// Admits only while no switch stops. A seen stop file latches; a failed check stops this call only.
    pub(crate) fn check(&self) -> Result<(), StopCause> {
        if self.env_stopped {
            return Err(StopCause::Env);
        }
        if self.latched.load(Ordering::SeqCst) {
            return Err(StopCause::File);
        }
        let Some(stop_file) = &self.stop_file else {
            return Err(StopCause::ProbeError);
        };
        match std::fs::metadata(stop_file) {
            Ok(_) => {
                self.latched.store(true, Ordering::SeqCst);
                Err(StopCause::File)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(StopCause::ProbeError),
        }
    }
}

/// `<runtime_root>/herdr/admission-off`; `None` without a runtime root.
pub(crate) fn stop_file_path() -> Option<PathBuf> {
    crate::config::runtime_root()
        .map(|root| STOP_FILE.iter().fold(root, |path, part| path.join(part)))
}

static PROCESS: OnceLock<Admission> = OnceLock::new();

/// The process switch, built on first use: only a configured channel's judgement reaches it.
pub(crate) fn check() -> Result<(), StopCause> {
    #[cfg(test)]
    if let Some(forced) = FORCED.with(|forced| forced.borrow().clone()) {
        return forced.check();
    }
    PROCESS
        .get_or_init(|| {
            Admission::new(std::env::var_os(ADMISSION_ENV).as_deref(), stop_file_path())
        })
        .check()
}

#[cfg(test)]
thread_local! {
    static FORCED: std::cell::RefCell<Option<std::sync::Arc<Admission>>> = const { std::cell::RefCell::new(None) };
}

/// Replaces the process switch on this thread until dropped.
#[cfg(test)]
pub(crate) struct ForcedAdmission(Option<std::sync::Arc<Admission>>);

#[cfg(test)]
pub(crate) fn force_for_test(admission: Admission) -> ForcedAdmission {
    let forced = Some(std::sync::Arc::new(admission));
    ForcedAdmission(FORCED.with(|cell| cell.replace(forced)))
}

#[cfg(test)]
impl Drop for ForcedAdmission {
    fn drop(&mut self) {
        FORCED.with(|cell| *cell.borrow_mut() = self.0.take());
    }
}

#[cfg(test)]
#[path = "herdr_admission_tests.rs"]
mod tests;
