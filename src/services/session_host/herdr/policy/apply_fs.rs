//! Every mutation is relative to one retained parent descriptor, including error cleanup.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub(super) struct Directory {
    file: File,
    path: PathBuf,
}
fn name(value: &str) -> Result<CString, String> {
    if value.is_empty() || value.contains('/') || matches!(value, "." | "..") {
        return Err("invalid relative apply name".into());
    }
    CString::new(value).map_err(|e| e.to_string())
}
impl Directory {
    pub(super) fn open(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            file,
            path: path.into(),
        })
    }
    pub(super) fn file(&self) -> &File {
        &self.file
    }
    fn open_at(&self, name: &CString, flags: i32) -> Result<File, String> {
        // SAFETY: the name is one NUL-terminated component; mode accompanies O_CREAT.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        // SAFETY: openat returned one newly owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    pub(super) fn lock(&self, target: &str) -> Result<File, String> {
        self.open_at(
            &name(&format!(".{target}.agentdesk-apply.lock"))?,
            libc::O_RDWR | libc::O_CREAT,
        )
    }
    pub(super) fn unique(&self, prefix: &str) -> Result<Entry<'_>, String> {
        let spelling = format!("{prefix}{}", uuid::Uuid::new_v4());
        let name = name(&spelling)?;
        let file = self.open_at(&name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL)?;
        Ok(Entry {
            directory: self,
            file,
            name,
            path: self.path.join(spelling),
            retained: false,
        })
    }
    /// The shared helper opens this path's parent, which resolves through the held descriptor.
    pub(super) fn sync_anchor(&self) -> PathBuf {
        let aliases = if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        Path::new(aliases)
            .join(self.file.as_raw_fd().to_string())
            .join("apply-entry")
    }
}

pub(super) struct Entry<'a> {
    directory: &'a Directory,
    file: File,
    name: CString,
    path: PathBuf,
    retained: bool,
}
impl Entry<'_> {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn as_file(&self) -> &File {
        &self.file
    }
    pub(super) fn keep(mut self) -> PathBuf {
        self.retained = true;
        self.path.clone()
    }
    pub(super) fn persist(mut self, target: &str) -> Result<(), String> {
        let target = name(target)?;
        let fd = self.directory.file.as_raw_fd();
        // SAFETY: both names are single components relative to the same retained directory.
        if unsafe { libc::renameat(fd, self.name.as_ptr(), fd, target.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        self.retained = true;
        Ok(())
    }
}
impl Deref for Entry<'_> {
    type Target = File;
    fn deref(&self) -> &Self::Target {
        &self.file
    }
}
impl DerefMut for Entry<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.file
    }
}
impl Drop for Entry<'_> {
    fn drop(&mut self) {
        if !self.retained {
            // SAFETY: removes only our unique scratch name from its original directory.
            let _ =
                unsafe { libc::unlinkat(self.directory.file.as_raw_fd(), self.name.as_ptr(), 0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn relative_entries_follow_the_descriptor_when_the_parent_is_renamed() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let held = root.path().join("held");
        std::fs::create_dir(&parent).unwrap();
        let directory = Directory::open(&parent).unwrap();
        let scratch = directory.unique("scratch-").unwrap();
        let scratch_name = scratch.path().file_name().unwrap().to_owned();
        std::fs::rename(&parent, &held).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join(&scratch_name), b"replacement scratch").unwrap();
        drop(scratch);
        assert!(
            !held.join(&scratch_name).exists(),
            "dirfd_cleanup_removes_held_scratch_contract"
        );
        assert_eq!(
            std::fs::read(parent.join(scratch_name)).unwrap(),
            b"replacement scratch",
            "dirfd_cleanup_preserves_replacement_contract"
        );
        let mut entry = directory.unique("new-").unwrap();
        let name = entry.path().file_name().unwrap().to_owned();
        entry.write_all(b"held candidate").unwrap();
        assert!(
            held.join(&name).exists() && !parent.join(name).exists(),
            "dirfd_create_stays_in_held_parent_contract"
        );
        entry.persist("config.toml").unwrap();
        assert_eq!(
            std::fs::read(held.join("config.toml")).unwrap(),
            b"held candidate",
            "dirfd_rename_stays_in_held_parent_contract"
        );
        assert!(!parent.join("config.toml").exists());
        let _lock = directory.lock("config.toml").unwrap();
        assert!(held.join(".config.toml.agentdesk-apply.lock").exists());
        assert!(
            !parent.join(".config.toml.agentdesk-apply.lock").exists(),
            "dirfd_lock_stays_in_held_parent_contract"
        );
        crate::services::discord::runtime_store::fsync_parent_dir(&directory.sync_anchor())
            .unwrap();
    }
}
