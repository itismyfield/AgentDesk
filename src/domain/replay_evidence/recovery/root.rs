//! A recovery root retains the real singleton lock, never a PID-file assertion.
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

#[path = "../../../services/replay_evidence/root_issuer.rs"]
pub(crate) mod issuer;

#[derive(Clone, Debug)]
pub struct OwnedRuntimeRoot {
    root: PathBuf,
    _lock: Arc<File>,
}

impl OwnedRuntimeRoot {
    pub(crate) fn covers(&self, root: &Path) -> bool {
        root.canonicalize().is_ok_and(|root| root == self.root)
    }
}
