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

/// Build a `Command` for `program` that runs with the merged runtime PATH.
pub(crate) fn runtime_command(program: impl AsRef<OsStr>) -> Command {
    let path = merged_runtime_path().map(OsString::from);
    command_with_path(program.as_ref(), path.as_deref())
}

/// Build a `Command` for `program` with `path` as its PATH, naming a bare program by its
/// absolute path in `path`: std forks instead of posix_spawn when PATH changes for a bare name.
pub(crate) fn command_with_path(program: &OsStr, path: Option<&OsStr>) -> Command {
    let mut command = Command::new(spawn_program(program, path));
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command
}

#[cfg(unix)]
fn spawn_program(program: &OsStr, path: Option<&OsStr>) -> OsString {
    use std::os::unix::ffi::OsStrExt;

    let Some(path) = path else {
        return program.to_os_string();
    };
    if program.as_bytes().contains(&b'/') {
        return program.to_os_string();
    }
    let resolved = resolve_in_paths(program, Some(path.to_os_string()), &current_dir_fallback())
        .filter(|resolved| resolved.is_absolute());
    match resolved {
        Some(resolved) => resolved.into_os_string(),
        None => {
            warn_bare_spawn_once(program);
            program.to_os_string()
        }
    }
}

// Windows has no fork, so the bare name keeps its existing lookup there.
#[cfg(not(unix))]
fn spawn_program(program: &OsStr, _path: Option<&OsStr>) -> OsString {
    program.to_os_string()
}

#[cfg(unix)]
fn warn_bare_spawn_once(program: &OsStr) {
    static WARNED: Mutex<BTreeSet<OsString>> = Mutex::new(BTreeSet::new());
    let first = WARNED
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert(program.to_os_string());
    if first {
        tracing::warn!(
            program = %program.to_string_lossy(),
            "runtime PATH has no absolute match; spawning the bare name, which forks"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn command_path_env(command: &Command) -> Option<&OsStr> {
        command
            .get_envs()
            .find(|(key, _)| *key == "PATH")
            .and_then(|(_, value)| value)
    }

    #[test]
    fn command_with_path_resolves_bare_names_and_falls_back_keeping_path() {
        let bin = tempfile::TempDir::new().expect("temp dir");
        let tool = bin.path().join("adk-runtime-path-probe");
        std::fs::write(&tool, "#!/bin/sh\nexit 0\n").expect("write stub");
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let path = bin.path().as_os_str().to_os_string();

        let command = command_with_path(OsStr::new("adk-runtime-path-probe"), Some(&path));
        assert_eq!(command.get_program(), tool.as_os_str());
        assert!(Path::new(command.get_program()).is_absolute());
        assert_eq!(command_path_env(&command), Some(path.as_os_str()));

        let missing = command_with_path(OsStr::new("adk-runtime-path-missing"), Some(&path));
        assert_eq!(missing.get_program(), "adk-runtime-path-missing");
        assert_eq!(command_path_env(&missing), Some(path.as_os_str()));
    }
}
