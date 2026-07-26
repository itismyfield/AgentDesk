use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
#[cfg(any(not(unix), test))]
use std::path::PathBuf;

use uuid::Uuid;

use super::StoreError;

const CHANNEL_LOCK: &str = "channel.lock";
const CHANNEL_FILE: &str = "channel.json";
const QUARANTINE_MARKER: &str = "channel.quarantined";
const QUARANTINE_MARKER_PREFIX: &[u8] = b"agentdesk.status-panel-journal.v2\nquarantined\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord::status_panel_transition_v2) enum WriteStage {
    CreateTemp,
    WriteAll,
    SyncFile,
    Rename,
    SyncParent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteTarget {
    Operation,
    Channel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Failpoint {
    pub target: WriteTarget,
    pub stage: WriteStage,
}

pub(super) struct ChannelLock {
    file: File,
}

impl Drop for ChannelLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

pub(super) struct ChannelStorage {
    #[cfg(unix)]
    channel: File,
    #[cfg(unix)]
    operations: File,
    #[cfg(unix)]
    quarantine: File,
    #[cfg(not(unix))]
    channel_path: PathBuf,
    #[cfg(test)]
    initial_channel_path: PathBuf,
}

impl ChannelStorage {
    pub(super) fn open(
        root: &Path,
        provider: &str,
        canonical_token_hash: &str,
        channel_id: u64,
    ) -> Result<Self, StoreError> {
        #[cfg(unix)]
        {
            let root_dir = open_or_create_root_directory(root)?;
            let provider_dir = open_or_create_directory(&root_dir, provider)?;
            let token_dir = open_or_create_directory(&provider_dir, canonical_token_hash)?;
            let channel_name = channel_id.to_string();
            let channel = open_or_create_directory(&token_dir, &channel_name)?;
            let operations = open_or_create_directory(&channel, "operations")?;
            let quarantine = open_or_create_directory(&channel, "quarantine")?;
            return Ok(Self {
                channel,
                operations,
                quarantine,
                #[cfg(test)]
                initial_channel_path: root
                    .join(provider)
                    .join(canonical_token_hash)
                    .join(channel_name),
            });
        }

        #[cfg(not(unix))]
        {
            let channel_path = root
                .join(provider)
                .join(canonical_token_hash)
                .join(channel_id.to_string());
            fs::create_dir_all(channel_path.join("operations"))
                .map_err(|_| StoreError::WriteFailed(WriteStage::CreateTemp))?;
            fs::create_dir_all(channel_path.join("quarantine"))
                .map_err(|_| StoreError::WriteFailed(WriteStage::CreateTemp))?;
            Ok(Self {
                #[cfg(test)]
                initial_channel_path: channel_path.clone(),
                channel_path,
            })
        }
    }

    pub(super) fn lock(&self) -> Result<ChannelLock, StoreError> {
        self.lock_with_mode(false)
    }

    pub(super) fn read_channel(&self) -> Result<Option<Vec<u8>>, StoreError> {
        self.read_entry(Directory::Channel, CHANNEL_FILE)
    }

    pub(super) fn read_operation(&self, name: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.read_entry(Directory::Operations, name)
    }

    pub(super) fn read_operation_records(&self) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
        #[cfg(unix)]
        {
            let mut records = Vec::new();
            for name in list_regular_files(&self.operations)? {
                let Some(bytes) = self.read_operation(&name)? else {
                    continue;
                };
                records.push((name, bytes));
            }
            return Ok(records);
        }

        #[cfg(not(unix))]
        {
            let mut records = Vec::new();
            for entry in fs::read_dir(self.directory_path(Directory::Operations))
                .map_err(|_| StoreError::ReadFailed)?
            {
                let entry = entry.map_err(|_| StoreError::ReadFailed)?;
                if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    continue;
                }
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| StoreError::UnexpectedFileType)?;
                let Some(bytes) = self.read_operation(&name)? else {
                    continue;
                };
                records.push((name, bytes));
            }
            Ok(records)
        }
    }

    pub(super) fn read_quarantine_marker(&self) -> Result<Option<Vec<u8>>, StoreError> {
        self.read_entry(Directory::Channel, QUARANTINE_MARKER)
    }

    #[cfg(test)]
    pub(super) fn read_quarantine_records(&self) -> Result<Vec<Vec<u8>>, StoreError> {
        #[cfg(unix)]
        {
            let mut records = Vec::new();
            for name in list_regular_files(&self.quarantine)? {
                if let Some(bytes) = self.read_entry(Directory::Quarantine, &name)? {
                    records.push(bytes);
                }
            }
            return Ok(records);
        }

        #[cfg(not(unix))]
        {
            let mut records = Vec::new();
            for entry in fs::read_dir(self.directory_path(Directory::Quarantine))
                .map_err(|_| StoreError::ReadFailed)?
            {
                let entry = entry.map_err(|_| StoreError::ReadFailed)?;
                if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    records.push(fs::read(entry.path()).map_err(|_| StoreError::ReadFailed)?);
                }
            }
            Ok(records)
        }
    }

    pub(super) fn write_operation(
        &self,
        name: &str,
        body: &[u8],
        failpoint: Option<Failpoint>,
    ) -> Result<(), StoreError> {
        self.atomic_write(
            Directory::Operations,
            name,
            body,
            WriteTarget::Operation,
            failpoint,
        )
    }

    pub(super) fn write_channel(
        &self,
        body: &[u8],
        failpoint: Option<Failpoint>,
    ) -> Result<(), StoreError> {
        self.atomic_write(
            Directory::Channel,
            CHANNEL_FILE,
            body,
            WriteTarget::Channel,
            failpoint,
        )
    }

    pub(super) fn quarantine_marker_present(&self) -> Result<bool, StoreError> {
        self.entry_exists(Directory::Channel, QUARANTINE_MARKER)
    }

    pub(super) fn quarantine_channel(
        &self,
        body: &[u8],
        high_watermark: Option<(u64, u64)>,
    ) -> Result<(), StoreError> {
        if !self.quarantine_marker_present()? {
            let mut marker = QUARANTINE_MARKER_PREFIX.to_vec();
            if let Some((revision, generation)) = high_watermark {
                marker.extend_from_slice(
                    format!("revision={revision}\ngeneration={generation}\n").as_bytes(),
                );
            }
            self.atomic_write(
                Directory::Channel,
                QUARANTINE_MARKER,
                &marker,
                WriteTarget::Channel,
                None,
            )?;
        }
        let archive_name = format!("{}-{CHANNEL_FILE}", Uuid::new_v4().simple());
        self.atomic_write(
            Directory::Quarantine,
            &archive_name,
            body,
            WriteTarget::Channel,
            None,
        )
    }

    pub(super) fn prune(&self, directory: Directory, keep: usize) -> Result<(), StoreError> {
        #[cfg(unix)]
        {
            let dir = self.directory(directory);
            let mut names = list_regular_files(dir)?;
            names.sort();
            let remove_count = names.len().saturating_sub(keep);
            for name in names.into_iter().take(remove_count) {
                unlink_file_at(dir, &name)
                    .map_err(|_| StoreError::WriteFailed(WriteStage::SyncParent))?;
            }
            if remove_count > 0 {
                dir.sync_all()
                    .map_err(|_| StoreError::WriteFailed(WriteStage::SyncParent))?;
            }
            return Ok(());
        }

        #[cfg(not(unix))]
        {
            let path = self.directory_path(directory);
            let mut files: Vec<_> = fs::read_dir(path)
                .map_err(|_| StoreError::ReadFailed)?
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .collect();
            files.sort_by_key(|entry| entry.file_name());
            let remove_count = files.len().saturating_sub(keep);
            for entry in files.into_iter().take(remove_count) {
                fs::remove_file(entry.path())
                    .map_err(|_| StoreError::WriteFailed(WriteStage::SyncParent))?;
            }
            if remove_count > 0 {
                File::open(path)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|_| StoreError::WriteFailed(WriteStage::SyncParent))?;
            }
            Ok(())
        }
    }

    fn lock_with_mode(&self, nonblocking: bool) -> Result<ChannelLock, StoreError> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let file = open_file_at(
                &self.channel,
                CHANNEL_LOCK,
                libc::O_RDWR | libc::O_CREAT,
                0o600,
            )
            .map_err(|_| StoreError::LockFailed)?;
            let operation = if nonblocking {
                libc::LOCK_EX | libc::LOCK_NB
            } else {
                libc::LOCK_EX
            };
            if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
                return Err(StoreError::LockFailed);
            }
            return Ok(ChannelLock { file });
        }

        #[cfg(not(unix))]
        {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(self.channel_path.join(CHANNEL_LOCK))
                .map_err(|_| StoreError::LockFailed)?;
            let _ = nonblocking;
            Ok(ChannelLock { file })
        }
    }

    fn read_entry(&self, directory: Directory, name: &str) -> Result<Option<Vec<u8>>, StoreError> {
        #[cfg(unix)]
        let file = match open_file_at(self.directory(directory), name, libc::O_RDONLY, 0) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                return Err(StoreError::SymlinkRejected);
            }
            Err(_) => return Err(StoreError::ReadFailed),
        };

        #[cfg(not(unix))]
        let file = match OpenOptions::new()
            .read(true)
            .open(self.directory_path(directory).join(name))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(StoreError::ReadFailed),
        };

        if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
            return Err(StoreError::UnexpectedFileType);
        }
        let mut file = file;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|_| StoreError::ReadFailed)?;
        Ok(Some(bytes))
    }

    fn entry_exists(&self, directory: Directory, name: &str) -> Result<bool, StoreError> {
        #[cfg(unix)]
        {
            match entry_kind_at(self.directory(directory), name)? {
                EntryKind::Missing => Ok(false),
                EntryKind::Regular => Ok(true),
                EntryKind::Symlink => Err(StoreError::SymlinkRejected),
                EntryKind::Other => Err(StoreError::UnexpectedFileType),
            }
        }

        #[cfg(not(unix))]
        {
            match fs::symlink_metadata(self.directory_path(directory).join(name)) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    Err(StoreError::SymlinkRejected)
                }
                Ok(metadata) if metadata.is_file() => Ok(true),
                Ok(_) => Err(StoreError::UnexpectedFileType),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(_) => Err(StoreError::ReadFailed),
            }
        }
    }

    fn atomic_write(
        &self,
        directory: Directory,
        name: &str,
        body: &[u8],
        target: WriteTarget,
        failpoint: Option<Failpoint>,
    ) -> Result<(), StoreError> {
        #[cfg(unix)]
        {
            let dir = self.directory(directory);
            match entry_kind_at(dir, name)? {
                EntryKind::Missing | EntryKind::Regular => {}
                EntryKind::Symlink => return Err(StoreError::SymlinkRejected),
                EntryKind::Other => return Err(StoreError::UnexpectedFileType),
            }
            let temp = format!(".{name}.{}.tmp", Uuid::new_v4().simple());
            maybe_fail(failpoint, target, WriteStage::CreateTemp)?;
            let result = (|| {
                let mut file = open_file_at(
                    dir,
                    &temp,
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                    0o600,
                )
                .map_err(|_| StoreError::WriteFailed(WriteStage::CreateTemp))?;
                maybe_fail(failpoint, target, WriteStage::WriteAll)?;
                file.write_all(body)
                    .map_err(|_| StoreError::WriteFailed(WriteStage::WriteAll))?;
                maybe_fail(failpoint, target, WriteStage::SyncFile)?;
                file.sync_all()
                    .map_err(|_| StoreError::WriteFailed(WriteStage::SyncFile))?;
                maybe_fail(failpoint, target, WriteStage::Rename)?;
                rename_at(dir, &temp, dir, name)
                    .map_err(|_| StoreError::WriteFailed(WriteStage::Rename))?;
                maybe_fail(failpoint, target, WriteStage::SyncParent)?;
                dir.sync_all()
                    .map_err(|_| StoreError::WriteFailed(WriteStage::SyncParent))
            })();
            if result.is_err() {
                let _ = unlink_file_at(dir, &temp);
            }
            return result;
        }

        #[cfg(not(unix))]
        {
            let parent = self.directory_path(directory);
            let path = parent.join(name);
            let temp = parent.join(format!(".{name}.{}.tmp", Uuid::new_v4().simple()));
            maybe_fail(failpoint, target, WriteStage::CreateTemp)?;
            let result = (|| {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temp)
                    .map_err(|_| StoreError::WriteFailed(WriteStage::CreateTemp))?;
                maybe_fail(failpoint, target, WriteStage::WriteAll)?;
                file.write_all(body)
                    .map_err(|_| StoreError::WriteFailed(WriteStage::WriteAll))?;
                maybe_fail(failpoint, target, WriteStage::SyncFile)?;
                file.sync_all()
                    .map_err(|_| StoreError::WriteFailed(WriteStage::SyncFile))?;
                maybe_fail(failpoint, target, WriteStage::Rename)?;
                fs::rename(&temp, path).map_err(|_| StoreError::WriteFailed(WriteStage::Rename))?;
                maybe_fail(failpoint, target, WriteStage::SyncParent)?;
                File::open(parent)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|_| StoreError::WriteFailed(WriteStage::SyncParent))
            })();
            if result.is_err() {
                let _ = fs::remove_file(temp);
            }
            result
        }
    }

    #[cfg(unix)]
    fn directory(&self, directory: Directory) -> &File {
        match directory {
            Directory::Channel => &self.channel,
            Directory::Operations => &self.operations,
            Directory::Quarantine => &self.quarantine,
        }
    }

    #[cfg(not(unix))]
    fn directory_path(&self, directory: Directory) -> PathBuf {
        match directory {
            Directory::Channel => self.channel_path.clone(),
            Directory::Operations => self.channel_path.join("operations"),
            Directory::Quarantine => self.channel_path.join("quarantine"),
        }
    }

    #[cfg(test)]
    pub(super) fn initial_channel_path(&self) -> &Path {
        &self.initial_channel_path
    }

    #[cfg(test)]
    pub(super) fn try_lock(&self) -> Result<ChannelLock, StoreError> {
        self.lock_with_mode(true)
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Directory {
    Channel,
    Operations,
    Quarantine,
}

fn maybe_fail(
    failpoint: Option<Failpoint>,
    target: WriteTarget,
    stage: WriteStage,
) -> Result<(), StoreError> {
    if failpoint == Some(Failpoint { target, stage }) {
        Err(StoreError::WriteFailed(stage))
    } else {
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Missing,
    Regular,
    Symlink,
    Other,
}

#[cfg(unix)]
fn open_or_create_root_directory(path: &Path) -> Result<File, StoreError> {
    use std::os::unix::ffi::OsStrExt;

    let resolved_parent = path
        .parent()
        .ok_or(StoreError::InvalidPathComponent("root"))?
        .canonicalize()
        .map_err(|_| StoreError::ReadFailed)?;
    let root_name = path
        .file_name()
        .ok_or(StoreError::InvalidPathComponent("root"))?;
    let mut current = if resolved_parent.is_absolute() {
        open_absolute_directory()?
    } else {
        open_current_directory()?
    };
    for component in resolved_parent.components() {
        use std::path::Component;

        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => {
                let name = std::str::from_utf8(name.as_bytes())
                    .map_err(|_| StoreError::InvalidPathComponent("root"))?;
                current = open_or_create_directory(&current, name)?;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(StoreError::InvalidPathComponent("root"));
            }
        }
    }
    let root_name = std::str::from_utf8(root_name.as_bytes())
        .map_err(|_| StoreError::InvalidPathComponent("root"))?;
    open_or_create_directory(&current, root_name)
}

#[cfg(unix)]
fn open_absolute_directory() -> Result<File, StoreError> {
    open_directory_path(Path::new("/"))
}

#[cfg(unix)]
fn open_current_directory() -> Result<File, StoreError> {
    open_directory_path(Path::new("."))
}

#[cfg(unix)]
fn open_directory_path(path: &Path) -> Result<File, StoreError> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    options.open(path).map_err(|error| {
        if error.raw_os_error() == Some(libc::ELOOP) {
            StoreError::SymlinkRejected
        } else {
            StoreError::ReadFailed
        }
    })
}

#[cfg(unix)]
fn open_or_create_directory(parent: &File, name: &str) -> Result<File, StoreError> {
    use std::os::fd::AsRawFd;

    let name = cstring(name)?;
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if created != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(StoreError::WriteFailed(WriteStage::CreateTemp));
        }
    }
    open_directory_at(parent, &name)
}

#[cfg(unix)]
fn open_directory_at(parent: &File, name: &CStr) -> Result<File, StoreError> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(
            if matches!(
                error.raw_os_error(),
                Some(libc::ELOOP) | Some(libc::ENOTDIR)
            ) {
                StoreError::SymlinkRejected
            } else {
                StoreError::UnexpectedFileType
            },
        );
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn open_file_at(parent: &File, name: &str, flags: libc::c_int, mode: u32) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let name = CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_int,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn entry_kind_at(parent: &File, name: &str) -> Result<EntryKind, StoreError> {
    use std::mem::MaybeUninit;
    use std::os::fd::AsRawFd;

    let name = cstring(name)?;
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(EntryKind::Missing)
        } else {
            Err(StoreError::ReadFailed)
        };
    }
    let mode = unsafe { stat.assume_init() }.st_mode;
    Ok(match mode & libc::S_IFMT {
        libc::S_IFREG => EntryKind::Regular,
        libc::S_IFLNK => EntryKind::Symlink,
        _ => EntryKind::Other,
    })
}

#[cfg(unix)]
fn rename_at(from_dir: &File, from: &str, to_dir: &File, to: &str) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let from = CString::new(from).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let to = CString::new(to).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    if unsafe {
        libc::renameat(
            from_dir.as_raw_fd(),
            from.as_ptr(),
            to_dir.as_raw_fd(),
            to.as_ptr(),
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unlink_file_at(parent: &File, name: &str) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let name = CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn list_regular_files(directory: &File) -> Result<Vec<String>, StoreError> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let duplicated = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicated < 0 {
        return Err(StoreError::ReadFailed);
    }
    if unsafe { libc::lseek(duplicated, 0, libc::SEEK_SET) } < 0 {
        unsafe { libc::close(duplicated) };
        return Err(StoreError::ReadFailed);
    }
    let cloned = unsafe { File::from_raw_fd(duplicated) };
    let stream = unsafe { libc::fdopendir(std::os::fd::IntoRawFd::into_raw_fd(cloned)) };
    if stream.is_null() {
        return Err(StoreError::ReadFailed);
    }
    struct DirectoryStream(*mut libc::DIR);
    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = std::str::from_utf8(bytes).map_err(|_| StoreError::UnexpectedFileType)?;
        if entry_kind_at(directory, name)? == EntryKind::Regular {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

#[cfg(unix)]
fn cstring(value: &str) -> Result<CString, StoreError> {
    CString::new(value).map_err(|_| StoreError::InvalidPathComponent("filesystem component"))
}
