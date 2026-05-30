use std::fs;
use std::io::{self, Write};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[cfg(unix)]
const SECRET_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const SECRET_DIR_MODE: u32 = 0o700;

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(unix)]
fn ensure_secret_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_mode(parent, SECRET_DIR_MODE)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_secret_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Write a file that stores credential material without inheriting a permissive
/// process umask. On Unix, parent directories are owner-only and files are
/// created or rewritten as 0600.
pub(crate) fn write_secret_file(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    ensure_secret_parent(path)?;
    write_secret_file_after_parent(path, contents)
}

/// Write owner-only secret material without changing the parent directory mode.
/// Use this for root-level artifacts where hardening the containing runtime
/// directory would have a broader operational effect than intended.
pub(crate) fn write_secret_file_preserving_parent_mode(
    path: &Path,
    contents: impl AsRef<[u8]>,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_secret_file_after_parent(path, contents)
}

fn write_secret_file_after_parent(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    #[cfg(unix)]
    {
        if path.exists() {
            set_mode(path, SECRET_FILE_MODE)?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(SECRET_FILE_MODE)
            .open(path)?;
        file.write_all(contents.as_ref())?;
        file.sync_all()?;
        set_mode(path, SECRET_FILE_MODE)?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        fs::write(path, contents)
    }
}

/// Correct unsafe existing secret-file modes on Unix. Symlinks and non-files
/// are refused so a read-side audit cannot chmod an arbitrary target. The
/// warning is path- and label-only; secret contents are never read or logged by
/// this helper.
pub(crate) fn audit_or_harden_secret_file(path: &Path, label: &str) -> bool {
    #[cfg(unix)]
    {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return false;
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            tracing::warn!(
                credential_file = %path.display(),
                label,
                "refusing to harden secret symlink"
            );
            return false;
        }
        if !file_type.is_file() {
            tracing::warn!(
                credential_file = %path.display(),
                label,
                "refusing to harden non-file secret path"
            );
            return false;
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode != SECRET_FILE_MODE {
            match set_mode(path, SECRET_FILE_MODE) {
                Ok(()) => tracing::warn!(
                    credential_file = %path.display(),
                    label,
                    previous_mode = format_args!("{mode:03o}"),
                    corrected_mode = "600",
                    "corrected unsafe secret file permissions"
                ),
                Err(error) => tracing::warn!(
                    credential_file = %path.display(),
                    label,
                    previous_mode = format_args!("{mode:03o}"),
                    "secret file permissions are unsafe and could not be corrected: {error}"
                ),
            }
        }
        true
    }

    #[cfg(not(unix))]
    {
        let _ = (path, label);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn write_secret_file_creates_owner_only_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("credential").join("token");
        write_secret_file(&path, "secret\n").unwrap();

        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn audit_or_harden_secret_file_corrects_existing_mode() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("token");
        fs::write(&path, "secret\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(audit_or_harden_secret_file(&path, "test-token"));

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn audit_or_harden_secret_file_refuses_symlink_without_chmoding_target() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("outside-token");
        let link = temp.path().join("token-link");
        fs::write(&target, "secret\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(!audit_or_harden_secret_file(&link, "test-token"));

        let target_mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(target_mode, 0o644);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_secret_file_preserving_parent_mode_keeps_existing_parent_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime-root");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let path = root.join("docker-compose.postgres.yml");

        write_secret_file_preserving_parent_mode(&path, "secret\n").unwrap();

        let parent_mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(parent_mode, 0o755);
        assert_eq!(file_mode, 0o600);
    }
}
