use super::*;

static PREPARED_RUNTIME_PATH: OnceLock<OsString> = OnceLock::new();

/// Read a completed PATH snapshot without starting or waiting for shell discovery.
pub(crate) fn prepared_runtime_path() -> Option<&'static OsStr> {
    PREPARED_RUNTIME_PATH.get().map(OsString::as_os_str)
}

pub(super) fn runtime_path_entries() -> Vec<PathBuf> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();

    extend_split_paths(std::env::var_os("PATH"), &mut entries, &mut seen);
    extend_split_paths(resolve_login_shell_path_os(), &mut entries, &mut seen);
    for dir in standard_fallback_dirs() {
        push_unique_path(dir, &mut entries, &mut seen);
    }

    if let Some(path) = join_paths_lossy(entries.clone()) {
        let _ = PREPARED_RUNTIME_PATH.set(path);
    }
    entries
}
