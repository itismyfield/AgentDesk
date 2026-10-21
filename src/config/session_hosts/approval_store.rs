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
        let mut problem = None;
        for name in ["current.json", "previous.json"] {
            match std::fs::read(root.join(name)) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
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
        self.write("current.json", hosts)
    }
}
