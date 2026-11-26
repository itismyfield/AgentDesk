//! The dcserver singleton lock. Any caller may ask for it, but a recovery root exists only
//! for a root whose lock file this process really holds by flock, never from a PID file.
#[cfg(unix)]
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(unix)]
use crate::domain::replay_evidence::recovery::root::{OwnedRuntimeRoot, issuer};
#[cfg(unix)]
pub(crate) mod issuer_key;

#[cfg(unix)]
pub struct Singleton {
    file: Arc<File>,
    root: Option<PathBuf>,
}

#[cfg(unix)]
fn write_dcserver_lock_pid(mut file: &File) -> std::io::Result<()> {
    use std::io::Write;
    // Clear the previous PID only after flock succeeds so shorter PIDs leave no suffix.
    file.set_len(0)?;
    file.write_all(std::process::id().to_string().as_bytes())
}

#[cfg(unix)]
impl Singleton {
    pub(crate) fn acquire(root: Option<&Path>) -> Result<Self, String> {
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
        let _ = write_dcserver_lock_pid(&file);
        Ok(Self {
            file: Arc::new(file),
            root: root.and_then(|root| root.canonicalize().ok()),
        })
    }

    pub(crate) fn recovery_root(&self) -> Option<OwnedRuntimeRoot> {
        Some(issuer::owned(
            issuer_key::SingletonRootKey(()),
            self.root.clone()?,
            self.file.clone(),
        ))
    }
}

#[cfg(all(any(test, feature = "test-support"), unix))]
pub(crate) fn acquire_for_tests(root: &Path) -> Result<OwnedRuntimeRoot, String> {
    Singleton::acquire(Some(root))?
        .recovery_root()
        .ok_or_else(|| "root unavailable".into())
}

#[cfg(all(test, unix))]
mod tests;
