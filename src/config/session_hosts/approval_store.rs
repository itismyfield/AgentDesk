//! Two durable generations, with absence distinguished from unreadable or unknown snapshots.

use std::path::PathBuf;

use super::BootSessionHosts;
use super::projection::{self, Decoded, Projection};
use crate::services::discord::runtime_store::{self, AtomicWriteContext};

pub(crate) enum Loaded {
    Missing,
    Known(BootSessionHosts, Option<&'static str>),
    Unavailable(&'static str),
}

pub(crate) trait ApprovalStore {
    fn load(&self) -> Loaded;
    fn save(&self, hosts: &BootSessionHosts) -> Result<(), String>;
}

pub(crate) struct FileApprovalStore(pub(crate) Option<PathBuf>);

impl FileApprovalStore {
    pub(crate) fn runtime() -> Self {
        Self(crate::config::runtime_root().map(|root| root.join("runtime/session_hosts_approval")))
    }

    fn write(&self, name: &str, hosts: &BootSessionHosts) -> Result<(), String> {
        let root = self.0.as_ref().ok_or("runtime root unavailable")?;
        std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
        runtime_store::fsync_parent_dir(root).map_err(|error| error.to_string())?;
        let path = root.join(name);
        let bytes = serde_json::to_string(&serde_json::json!({
            "schema": projection::SCHEMA, "projection": Projection::of(hosts)
        }))
        .map_err(|error| error.to_string())?;
        runtime_store::critical_atomic_write(
            &path,
            &bytes,
            AtomicWriteContext::new("session_hosts_approval"),
        )?;
        #[cfg(test)]
        if name == "current.json" && FAIL_CURRENT_SYNC.with(|fault| fault.replace(false)) {
            return Err("injected current snapshot directory sync failure".into());
        }
        runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string())?;
        if !runtime_store::PARENT_DIR_FSYNC_FLUSHES {
            return Err("approval snapshot directory durability unavailable".into());
        }
        Ok(())
    }
}

impl ApprovalStore for FileApprovalStore {
    fn load(&self) -> Loaded {
        let Some(root) = &self.0 else {
            return Loaded::Unavailable("approval_snapshot_unreadable");
        };
        if std::fs::symlink_metadata(root).is_ok_and(|meta| !meta.is_dir()) {
            return Loaded::Unavailable("approval_snapshot_unreadable");
        }
        let pending = root.join("pending");
        let interrupted = match std::fs::symlink_metadata(&pending) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return Loaded::Unavailable("approval_snapshot_unreadable"),
        };
        let mut problem = interrupted.then_some("approval_snapshot_not_durable");
        for name in ["current.json", "previous.json"] {
            if interrupted && name == "current.json" {
                continue;
            }
            let path = root.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if !meta.is_file() => {
                    problem.get_or_insert("approval_snapshot_unreadable");
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    problem.get_or_insert("approval_snapshot_unreadable");
                    continue;
                }
                _ => {}
            }
            match std::fs::read(path) {
                Err(_) => {
                    problem.get_or_insert("approval_snapshot_unreadable");
                }
                Ok(bytes) => match projection::decode(&bytes) {
                    Ok(Decoded::Known(hosts)) => return Loaded::Known(hosts, problem),
                    Ok(Decoded::Newer) => {
                        problem = Some("approval_snapshot_newer");
                    }
                    Err(_) => {
                        problem.get_or_insert("approval_snapshot_corrupt");
                    }
                },
            }
        }
        problem.map_or(Loaded::Missing, Loaded::Unavailable)
    }

    fn save(&self, hosts: &BootSessionHosts) -> Result<(), String> {
        if let Loaded::Known(previous, _) = self.load() {
            self.write("previous.json", &previous)?;
        }
        let root = self.0.as_ref().ok_or("runtime root unavailable")?;
        std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
        let pending = root.join("pending");
        runtime_store::critical_atomic_write(
            &pending,
            "pending",
            AtomicWriteContext::new("session_hosts_approval"),
        )?;
        runtime_store::fsync_parent_dir(&pending).map_err(|error| error.to_string())?;
        self.write("current.json", hosts)?;
        std::fs::remove_file(&pending).map_err(|error| error.to_string())?;
        if let Err(error) = cleanup_sync(&pending) {
            runtime_store::critical_atomic_write(
                &root.join("pending"),
                "pending",
                AtomicWriteContext::new("session_hosts_approval"),
            )
            .map_err(|restore| {
                format!("approval marker sync: {error}; restore pending: {restore}")
            })?;
            runtime_store::fsync_parent_dir(&pending).map_err(|restore| {
                format!("approval marker sync: {error}; restore sync: {restore}")
            })?;
            return Err(format!("approval marker sync: {error}"));
        }
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static FAIL_CLEANUP_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(super) static FAIL_CURRENT_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn cleanup_sync(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_CLEANUP_SYNC.with(|fault| fault.replace(false)) {
        return Err(std::io::Error::other(
            "injected marker cleanup sync failure",
        ));
    }
    runtime_store::fsync_parent_dir(path)
}
