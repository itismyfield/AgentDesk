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
    fn recovery_action(&self, reason: &str) -> String {
        match reason {
            "approval_snapshot_not_durable" => "Restart to retry an interrupted approval; repair approval-store permissions and storage before retrying if it fails again.",
            "approval_snapshot_corrupt" | "approval_snapshot_unreadable" => "Repair or restore readable compatible approval snapshots, then restart.",
            "approval_snapshot_newer" => "Run the newer binary or restore compatible approval snapshots, then restart.",
            "session_hosts_migration_refused" => "Resolve the listed live rows and holds at the approved endpoints before restarting.",
            "session_hosts_migration_unverified" => "Restore database and hold-store availability before restarting to verify the proposed migration.",
            _ => "Run the server to verify the proposed migration before updating approval snapshots.",
        }.into()
    }
}

pub(crate) struct FileApprovalStore(pub(crate) Option<PathBuf>);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingMarker {
    state: String,
    had_previous: bool,
}

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
            Ok(meta) if meta.is_file() => match std::fs::read(&pending) {
                Ok(bytes) if bytes == b"committed" => false,
                Ok(bytes) => {
                    let initial_seed = serde_json::from_slice::<PendingMarker>(&bytes)
                        .is_ok_and(|marker| marker.state == "pending" && !marker.had_previous);
                    // Only a known first seed may ignore its uncommitted current file.
                    if initial_seed
                        && matches!(std::fs::symlink_metadata(root.join("previous.json")),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound)
                    {
                        return Loaded::Missing;
                    }
                    true
                }
                Err(_) => true,
            },
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
        let previous = match self.load() {
            Loaded::Known(previous, _) => Some(previous),
            _ => None,
        };
        let had_previous = previous.is_some();
        if let Some(previous) = previous {
            self.write("previous.json", &previous)?;
        }
        let marker = serde_json::to_string(&serde_json::json!({
            "state": "pending", "had_previous": had_previous
        }))
        .map_err(|error| error.to_string())?;
        let root = self.0.as_ref().ok_or("runtime root unavailable")?;
        std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
        let pending = root.join("pending");
        runtime_store::critical_atomic_write(
            &pending,
            &marker,
            AtomicWriteContext::new("session_hosts_approval"),
        )?;
        runtime_store::fsync_parent_dir(&pending).map_err(|error| error.to_string())?;
        self.write("current.json", hosts)?;
        // Clear pending state durably without unlinking store files.
        runtime_store::critical_atomic_write(
            &pending,
            "committed",
            AtomicWriteContext::new("session_hosts_approval"),
        )?;
        if let Err(error) = cleanup_sync(&pending) {
            runtime_store::critical_atomic_write(
                &root.join("pending"),
                &marker,
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

    fn recovery_action(&self, reason: &str) -> String {
        let Some(root) = &self.0 else {
            return "Restore AGENTDESK_ROOT_DIR so the approval-store path can be resolved, repair permissions or storage, then restart.".into();
        };
        match reason {
            "approval_snapshot_not_durable" => format!(
                "Restart to retry the interrupted approval at {}. If it fails again, repair this directory's permissions or storage and restart; preserve previous.json while live rows still use it.",
                root.join("pending").display()
            ),
            "approval_snapshot_corrupt" | "approval_snapshot_unreadable" => format!(
                "Repair or restore a readable compatible snapshot at {} or {}; repair directory permissions or storage, preserve the previous generation, then restart.",
                root.join("current.json").display(),
                root.join("previous.json").display()
            ),
            "approval_snapshot_newer" => format!(
                "Run the newer binary or restore compatible approval snapshots at {}, then restart.",
                root.display()
            ),
            "session_hosts_migration_refused" => format!(
                "Keep the approved snapshots at {}; resolve the listed live rows and holds before restarting.",
                root.display()
            ),
            "session_hosts_migration_unverified" => format!(
                "Restore database and hold-store availability before restarting to verify migration; preserve the approved snapshots at {}.",
                root.display()
            ),
            _ => format!(
                "Run the server to verify migration before updating approval snapshots at {}.",
                root.display()
            ),
        }
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static FAIL_CLEANUP_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(crate) static FAIL_CURRENT_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
