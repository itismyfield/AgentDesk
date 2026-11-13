//! A recovery root retains the real singleton lock, never a PID-file assertion.
#[cfg(unix)]
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) use crate::domain::replay_evidence::recovery::root::OwnedRuntimeRoot;
#[cfg(unix)]
use crate::domain::replay_evidence::recovery::root::issuer;
#[cfg(unix)]
pub(crate) mod issuer_key;

#[cfg(unix)]
pub(super) struct Singleton {
    file: Arc<File>,
    root: Option<PathBuf>,
}

#[cfg(unix)]
impl Singleton {
    pub(super) fn acquire(root: Option<&Path>) -> Result<Self, String> {
        use std::os::fd::AsRawFd;
        let path = root
            .map(|root| root.join("runtime/dcserver.lock"))
            .unwrap_or_else(|| PathBuf::from("/tmp/agentdesk-dcserver.lock"));
        let file = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("Failed to open lock file {path:?}: {error}"))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(format!(
                "Another dcserver is already running (lock held on {path:?}). Exiting."
            ));
        }
        let _ = super::write_dcserver_lock_pid(&file);
        Ok(Self {
            file: Arc::new(file),
            root: root.and_then(|root| root.canonicalize().ok()),
        })
    }

    pub(super) fn recovery_root(&self) -> Option<OwnedRuntimeRoot> {
        Some(issuer::owned(
            issuer_key::SingletonRootKey(()),
            self.root.clone()?,
            self.file.clone(),
        ))
    }
}

#[cfg(all(test, unix))]
pub(crate) fn acquire_for_tests(root: &Path) -> Result<OwnedRuntimeRoot, String> {
    Singleton::acquire(Some(root))?
        .recovery_root()
        .ok_or_else(|| "root unavailable".into())
}

#[cfg(all(test, unix))]
#[path = "replay_recovery_tests.rs"]
mod tests;
