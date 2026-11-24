//! Only the singleton owner can assemble a root from its existing lock handle.
#![cfg(unix)]
use super::OwnedRuntimeRoot;
use crate::services::dcserver_singleton::issuer_key::SingletonRootKey;
use std::{fs::File, path::PathBuf, sync::Arc};

pub(crate) fn owned(_key: SingletonRootKey, root: PathBuf, lock: Arc<File>) -> OwnedRuntimeRoot {
    OwnedRuntimeRoot { root, _lock: lock }
}
