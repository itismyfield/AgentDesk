//! Descriptor-relative secret storage. Directory traversal and files use O_NOFOLLOW/openat.
#![cfg(unix)]
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Component, Path};

#[derive(Debug)]
pub(crate) enum StoreError {
    Open,
    Write,
    FileSync,
    Rename,
    ParentSync,
    Read,
    Permissions,
}
pub(crate) struct PrivateDirectory {
    directory: File,
}

fn open_at(parent: i32, name: &CString, flags: i32) -> io::Result<File> {
    // SAFETY: CString is terminated; mode is provided for O_CREAT; successful fd is adopted once.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is newly opened and ownership transfers into File.
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn name(value: &str) -> Result<CString, StoreError> {
    if value.is_empty() || value.contains('/') || value == "." || value == ".." {
        return Err(StoreError::Open);
    }
    CString::new(value).map_err(|_| StoreError::Open)
}
fn private_file(file: &File) -> Result<(), StoreError> {
    let meta = file.metadata().map_err(|_| StoreError::Read)?;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_file()
        || meta.mode() & 0o777 != 0o600
        || meta.nlink() != 1
        || meta.uid() != unsafe { libc::geteuid() }
    {
        return Err(StoreError::Permissions);
    }
    Ok(())
}
impl PrivateDirectory {
    pub(crate) fn open(path: &Path) -> Result<Self, StoreError> {
        if !path.is_absolute() {
            return Err(StoreError::Open);
        }
        let mut directory = File::open("/").map_err(|_| StoreError::Open)?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(part) => {
                    let part = CString::new(part.as_bytes()).map_err(|_| StoreError::Open)?;
                    directory = open_at(
                        directory.as_raw_fd(),
                        &part,
                        libc::O_RDONLY | libc::O_DIRECTORY,
                    )
                    .map_err(|_| StoreError::Open)?;
                }
                _ => return Err(StoreError::Open),
            }
        }
        let meta = directory.metadata().map_err(|_| StoreError::Read)?;
        // SAFETY: geteuid has no preconditions.
        if meta.mode() & 0o777 != 0o700 || meta.uid() != unsafe { libc::geteuid() } {
            return Err(StoreError::Permissions);
        }
        Ok(Self { directory })
    }
    pub(crate) fn lock(&self, filename: &str) -> Result<File, StoreError> {
        let file = open_at(
            self.directory.as_raw_fd(),
            &name(filename)?,
            libc::O_RDWR | libc::O_CREAT | libc::O_NONBLOCK,
        )
        .map_err(|_| StoreError::Open)?;
        private_file(&file)?;
        file.try_lock().map_err(|_| StoreError::Open)?;
        Ok(file)
    }
    pub(crate) fn read(&self, filename: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let file = match open_at(
            self.directory.as_raw_fd(),
            &name(filename)?,
            libc::O_RDONLY | libc::O_NONBLOCK,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(StoreError::Open),
        };
        private_file(&file)?;
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| StoreError::Read)?;
        if bytes.len() > 65536 {
            return Err(StoreError::Read);
        }
        Ok(Some(bytes))
    }
    pub(crate) fn write_atomic(&self, filename: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.write_stages(filename, bytes, None)
    }
    fn write_stages(
        &self,
        filename: &str,
        bytes: &[u8],
        fail: Option<u8>,
    ) -> Result<(), StoreError> {
        if bytes.len() > 65536 {
            return Err(StoreError::Write);
        }
        let target = name(filename)?;
        let temporary = name(&format!(".{filename}.{}.tmp", uuid::Uuid::new_v4()))?;
        let fd = self.directory.as_raw_fd();
        let result = (|| {
            let mut file = open_at(
                fd,
                &temporary,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            )
            .map_err(|_| StoreError::Open)?;
            if fail == Some(1) {
                return Err(StoreError::Write);
            }
            file.write_all(bytes).map_err(|_| StoreError::Write)?;
            if fail == Some(2) {
                return Err(StoreError::FileSync);
            }
            file.sync_all().map_err(|_| StoreError::FileSync)?;
            if fail == Some(3) {
                return Err(StoreError::Rename);
            }
            // SAFETY: single component names relative to a retained directory descriptor.
            if unsafe { libc::renameat(fd, temporary.as_ptr(), fd, target.as_ptr()) } != 0 {
                return Err(StoreError::Rename);
            }
            if fail == Some(4) {
                return Err(StoreError::ParentSync);
            }
            self.directory
                .sync_all()
                .map_err(|_| StoreError::ParentSync)
        })();
        // SAFETY: removes only the unique temporary filename in the retained directory.
        unsafe { libc::unlinkat(fd, temporary.as_ptr(), 0) };
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn directory() -> (tempfile::TempDir, PrivateDirectory) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        // Resolve the OS temporary-directory root, not the symlinks exercised below.
        let dir = PrivateDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
        (temp, dir)
    }
    #[test]
    fn failed_writes_preserve_phase_and_remove_secret_temporaries() {
        let (temp, dir) = directory();
        for phase in 1..=4 {
            dir.write_atomic("token", b"old").unwrap();
            assert!(dir.write_stages("token", b"new", Some(phase)).is_err());
            assert_eq!(
                dir.read("token").unwrap().unwrap(),
                if phase == 4 { b"new" } else { b"old" }
            );
            assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
        }
    }
    #[test]
    fn symlink_permissions_and_second_owner_are_rejected() {
        let (temp, dir) = directory();
        let other = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(other.path(), temp.path().join("link")).unwrap();
        assert!(PrivateDirectory::open(&temp.path().join("link")).is_err());
        std::os::unix::fs::symlink(other.path().join("outside"), temp.path().join("token"))
            .unwrap();
        assert!(dir.read("token").is_err());
        dir.write_atomic("token", b"new").unwrap();
        assert!(!other.path().join("outside").exists());
        let _lock = dir.lock("owner").unwrap();
        assert!(dir.lock("owner").is_err());
        std::fs::set_permissions(
            temp.path().join("token"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(dir.read("token").is_err());
    }
}
