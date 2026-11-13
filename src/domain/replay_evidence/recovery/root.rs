//! A recovery root retains the real singleton lock, never a PID-file assertion.
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

#[path = "../../../cli/dcserver/replay_root_issuer.rs"]
pub(crate) mod issuer;

#[derive(Clone, Debug)]
pub(crate) struct OwnedRuntimeRoot {
    root: PathBuf,
    _lock: Arc<File>,
}

impl OwnedRuntimeRoot {
    pub(crate) fn covers(&self, root: &Path) -> bool {
        root.canonicalize().is_ok_and(|root| root == self.root)
    }
}
