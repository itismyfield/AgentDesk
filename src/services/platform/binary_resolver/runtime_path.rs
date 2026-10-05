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
pub(crate) fn runtime_command(program: impl AsRef<OsStr>) -> std::io::Result<Command> {
    let path = merged_runtime_path().map(OsString::from);
    command_with_path(program.as_ref(), path.as_deref())
}

/// Build a `Command` for `program` with `path` as its PATH. A bare name must resolve to an
/// absolute path first: std forks instead of posix_spawn when PATH changes for a bare name.
pub(crate) fn command_with_path(program: &OsStr, path: Option<&OsStr>) -> std::io::Result<Command> {
    let mut command = Command::new(spawn_program(program, path)?);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    Ok(command)
}

#[cfg(unix)]
fn spawn_program(program: &OsStr, path: Option<&OsStr>) -> std::io::Result<OsString> {
    use std::os::unix::ffi::OsStrExt;

    let Some(path) = path else {
        return Ok(program.to_os_string());
    };
    if program.as_bytes().contains(&b'/') {
        return Ok(program.to_os_string());
    }
    let resolved = resolve_in_paths(program, Some(path.to_os_string()), &current_dir_fallback())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} is not on the runtime PATH", program.to_string_lossy()),
            )
        })?;
    if resolved.is_absolute() {
        return Ok(resolved.into_os_string());
    }
    // A relative PATH entry is searched from the cwd the child inherits from this process.
    Ok(std::env::current_dir()?.join(resolved).into_os_string())
}

// Windows has no fork, so the bare name keeps its existing lookup there.
#[cfg(not(unix))]
fn spawn_program(program: &OsStr, _path: Option<&OsStr>) -> std::io::Result<OsString> {
    Ok(program.to_os_string())
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
    fn command_with_path_spawns_only_absolute_programs_and_keeps_path() {
        let bin = tempfile::TempDir::new().expect("temp dir");
        let tool = bin.path().join("adk-runtime-path-probe");
        std::fs::write(&tool, "#!/bin/sh\nexit 0\n").expect("write stub");
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let path = bin.path().as_os_str().to_os_string();

        let command =
            command_with_path(OsStr::new("adk-runtime-path-probe"), Some(&path)).expect("resolved");
        assert_eq!(command.get_program(), tool.as_os_str());
        assert!(Path::new(command.get_program()).is_absolute());
        assert_eq!(command_path_env(&command), Some(path.as_os_str()));

        let missing = command_with_path(OsStr::new("adk-runtime-path-missing"), Some(&path))
            .expect_err("an unresolved bare name must not yield a spawnable command");
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);

        let cwd = std::env::current_dir().expect("cwd");
        let relative: PathBuf = cwd
            .components()
            .skip(1)
            .map(|_| Path::new(".."))
            .chain(
                bin.path()
                    .components()
                    .skip(1)
                    .map(|part| Path::new(part.as_os_str())),
            )
            .collect();
        let relative_path = relative.as_os_str().to_os_string();
        let command = command_with_path(OsStr::new("adk-runtime-path-probe"), Some(&relative_path))
            .expect("resolved from a relative entry");
        assert_eq!(
            command.get_program(),
            cwd.join(&relative)
                .join("adk-runtime-path-probe")
                .as_os_str()
        );
        assert!(Path::new(command.get_program()).is_absolute());
        assert_eq!(command_path_env(&command), Some(relative_path.as_os_str()));
    }
}
