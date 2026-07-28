use super::{LoadedRoutineScript, RoutineScriptCandidate};
use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(unix)]
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::fd::{AsRawFd as _, FromRawFd as _};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;

mod authority;
use authority::AuthorityFileKind;
#[cfg(unix)]
use authority::{FileIdentity, require_available_identity};
pub(super) use authority::{
    PathResolutionError, RoutineRootValidationError, ValidatedHelperSurface, ValidatedRoutineRoot,
    ValidatedRuntimeRoot, bind_routine_root_authority, validate_routine_authority,
};
#[cfg(test)]
pub(super) use authority::{
    bind_routine_root_authority_with_hook, validate_routine_authority_with_hook,
    validate_routine_roots,
};

#[derive(Debug)]
pub(super) struct DiscoveredRoutineScript {
    pub(super) path: PathBuf,
    source: std::result::Result<String, RoutineSourceReadError>,
}

#[derive(Debug)]
struct RoutineSourceReadError {
    kind: io::ErrorKind,
    message: String,
}

impl From<io::Error> for RoutineSourceReadError {
    fn from(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct RoutineDiscoveryHooks<'a> {
    pub(super) before_open: Option<&'a (dyn Fn(&Path) + Send + Sync)>,
    pub(super) before_read: Option<&'a (dyn Fn(&Path) + Send + Sync)>,
    pub(super) read_observer: Option<&'a (dyn Fn(&Path) + Send + Sync)>,
    pub(super) authority_check: Option<&'a (dyn Fn() -> io::Result<()> + Send + Sync)>,
}

impl DiscoveredRoutineScript {
    pub(super) fn read_source(&self) -> io::Result<String> {
        match &self.source {
            Ok(source) => Ok(source.clone()),
            Err(error) => Err(io::Error::new(error.kind, error.message.clone())),
        }
    }
}

fn read_opened_routine_source(file: &File, path: &Path) -> io::Result<String> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other(format!(
            "opened routine candidate `{}` is not a regular file",
            path.display()
        )));
    }
    let mut source = String::new();
    let mut file = file;
    file.read_to_string(&mut source)?;
    Ok(source)
}

fn verify_discovery_authority(hooks: RoutineDiscoveryHooks<'_>) -> io::Result<()> {
    if let Some(check) = hooks.authority_check {
        check()?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn candidate_failure_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .expect("test current directory must be available")
                .join(path)
        }
    })
}

pub(super) fn routine_roots_identity(
    runtime_authority: &ValidatedRuntimeRoot,
    roots: &[ValidatedRoutineRoot],
    helper_authority: &ValidatedHelperSurface,
) -> PathBuf {
    use sha2::{Digest as _, Sha256};

    fn update_path(hasher: &mut Sha256, path: &Path) {
        let bytes = path.as_os_str().as_encoded_bytes();
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }

    fn update_identity(
        hasher: &mut Sha256,
        exists: bool,
        kind: Option<AuthorityFileKind>,
        identity: Option<(u64, u64)>,
    ) {
        hasher.update([u8::from(exists)]);
        hasher.update([match kind {
            None => 0,
            Some(AuthorityFileKind::Directory) => 1,
            Some(AuthorityFileKind::RegularFile) => 2,
            Some(AuthorityFileKind::Symlink) => 3,
            Some(AuthorityFileKind::Other) => 4,
        }]);
        match identity {
            Some((device, inode)) => {
                hasher.update([1]);
                hasher.update(device.to_le_bytes());
                hasher.update(inode.to_le_bytes());
            }
            None => hasher.update([0]),
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(b"runtime");
    update_path(&mut hasher, &runtime_authority.configured);
    update_path(&mut hasher, &runtime_authority.canonical);
    #[cfg(unix)]
    let runtime_identity = runtime_authority
        .identity
        .map(|identity| (identity.device, identity.inode));
    #[cfg(not(unix))]
    let runtime_identity = None;
    update_identity(
        &mut hasher,
        runtime_authority.exists,
        runtime_authority.kind,
        runtime_identity,
    );
    for root in roots {
        hasher.update(b"root");
        update_path(&mut hasher, &root.canonical);
        #[cfg(unix)]
        let identity = root
            .identity
            .map(|identity| (identity.device, identity.inode));
        #[cfg(not(unix))]
        let identity = None;
        update_identity(&mut hasher, root.exists, root.kind, identity);
    }
    hasher.update(b"helper");
    update_path(&mut hasher, &helper_authority.canonical);
    #[cfg(unix)]
    let helper_identity = helper_authority
        .identity
        .map(|identity| (identity.device, identity.inode));
    #[cfg(not(unix))]
    let helper_identity = None;
    update_identity(
        &mut hasher,
        helper_authority.exists,
        helper_authority.kind,
        helper_identity,
    );
    PathBuf::from(hex::encode(hasher.finalize()))
}

pub(super) fn script_ref(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub(super) fn add_cached_candidates_for_root(
    existing_scripts: &HashMap<String, LoadedRoutineScript>,
    candidates_by_ref: &mut BTreeMap<String, Vec<RoutineScriptCandidate>>,
    seen_refs: &mut HashSet<String>,
    root_index: usize,
    root: &Path,
) {
    for (script_ref, script) in existing_scripts
        .iter()
        .filter(|(_, script)| script.file.starts_with(root))
    {
        seen_refs.insert(script_ref.clone());
        candidates_by_ref
            .entry(script_ref.clone())
            .or_default()
            .push(RoutineScriptCandidate {
                root_index,
                root: root.to_path_buf(),
                path: script.file.clone(),
                failure_key: script.file.clone(),
                snapshot: None,
                cached: Some(script.clone()),
            });
    }
}

#[cfg(unix)]
struct DirectoryStream(*mut libc::DIR);

#[cfg(unix)]
struct PinnedDirectoryEntry {
    name: OsString,
    identity: FileIdentity,
    kind: PinnedEntryKind,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PinnedEntryKind {
    Directory,
    RegularFile,
    Other,
}

#[cfg(unix)]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: `DirectoryStream` exclusively owns the successful `fdopendir` result.
        unsafe {
            libc::closedir(self.0);
        }
    }
}

#[cfg(unix)]
fn directory_entries(directory: &File) -> io::Result<Vec<PinnedDirectoryEntry>> {
    // SAFETY: `fcntl` receives a valid live descriptor and returns an independent descriptor.
    let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `duplicate` is an owned directory descriptor. `fdopendir` owns it on success.
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so ownership of `duplicate` remains here.
        unsafe {
            libc::close(duplicate);
        }
        return Err(error);
    }
    let stream = DirectoryStream(stream);
    let mut entries = Vec::new();
    loop {
        clear_readdir_errno();
        // SAFETY: the stream stays live for this call and the returned entry is copied immediately.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            if let Some(error) = readdir_error() {
                return Err(error);
            }
            break;
        }
        // SAFETY: POSIX guarantees `d_name` is a NUL-terminated byte sequence in this entry.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = OsStr::from_bytes(bytes).to_os_string();
        let (identity, kind) = entry_identity(directory, &name)?;
        entries.push(PinnedDirectoryEntry {
            name,
            identity,
            kind,
        });
    }
    Ok(entries)
}

#[cfg(unix)]
fn clear_readdir_errno() {
    #[cfg(any(target_os = "linux", target_os = "dragonfly"))]
    // SAFETY: libc exposes the calling thread's errno slot through this pointer.
    unsafe {
        *libc::__errno_location() = 0;
    }
    #[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd"))]
    // SAFETY: libc exposes the calling thread's errno slot through this pointer.
    unsafe {
        *libc::__errno() = 0;
    }
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    // SAFETY: libc exposes the calling thread's errno slot through this pointer.
    unsafe {
        *libc::__error() = 0;
    }
}

#[cfg(unix)]
fn readdir_error() -> Option<io::Error> {
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        let error = io::Error::last_os_error();
        return (error.raw_os_error() != Some(0)).then_some(error);
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    None
}

#[cfg(unix)]
fn directory_entry_cstring(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "routine directory entry contains an interior NUL",
        )
    })
}

#[cfg(unix)]
fn entry_identity(parent: &File, name: &OsStr) -> io::Result<(FileIdentity, PinnedEntryKind)> {
    let entry_path = PathBuf::from(name);
    let name = directory_entry_cstring(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `parent` and `name` stay live, and `stat` points to writable storage.
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful `fstatat` initialized the complete `stat` value.
    let stat = unsafe { stat.assume_init() };
    let kind = match stat.st_mode & libc::S_IFMT {
        libc::S_IFDIR => PinnedEntryKind::Directory,
        libc::S_IFREG => PinnedEntryKind::RegularFile,
        _ => PinnedEntryKind::Other,
    };
    let identity = require_available_identity(FileIdentity::from_stat(&stat), &entry_path)?;
    Ok((identity, kind))
}

#[cfg(unix)]
fn openat(parent: &File, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
    let name = directory_entry_cstring(name)?;
    // SAFETY: `parent` is live and `name` is NUL-terminated for the duration of the call.
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `openat` returns a new descriptor owned by this function.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn open_root(root: &ValidatedRoutineRoot) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    let directory = options.open(&root.canonical)?;
    let metadata = directory.metadata()?;
    let observed_identity =
        require_available_identity(FileIdentity::from_metadata(&metadata), &root.canonical)?;
    if !metadata.is_dir()
        || root
            .identity
            .is_some_and(|identity| identity != observed_identity)
    {
        return Err(io::Error::other(format!(
            "validated routine root `{}` no longer names the preflight directory",
            root.canonical.display()
        )));
    }
    Ok(directory)
}

#[cfg(unix)]
fn verify_opened_entry_identity(
    entry: &PinnedDirectoryEntry,
    opened: &File,
    path: &Path,
) -> io::Result<()> {
    let opened_metadata = opened.metadata()?;
    let opened_identity =
        require_available_identity(FileIdentity::from_metadata(&opened_metadata), path)?;
    if opened_identity != entry.identity {
        return Err(io::Error::other(format!(
            "routine entry `{}` changed identity during discovery",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn collect_routine_scripts_inner(
    directory: &File,
    current_path: &Path,
    hooks: RoutineDiscoveryHooks<'_>,
    out: &mut Vec<DiscoveredRoutineScript>,
) -> io::Result<()> {
    let mut entries = directory_entries(directory)?;
    entries.sort_by(|first, second| first.name.cmp(&second.name));
    for entry in entries {
        let path = current_path.join(&entry.name);
        match entry.kind {
            PinnedEntryKind::Directory => {
                if let Some(hook) = hooks.before_open {
                    hook(&path);
                }
                verify_discovery_authority(hooks)?;
                let child_directory = openat(
                    directory,
                    &entry.name,
                    libc::O_RDONLY
                        | libc::O_DIRECTORY
                        | libc::O_NOFOLLOW
                        | libc::O_NONBLOCK
                        | libc::O_CLOEXEC,
                )?;
                verify_opened_entry_identity(&entry, &child_directory, &path)?;
                verify_discovery_authority(hooks)?;
                collect_routine_scripts_inner(&child_directory, &path, hooks, out)?;
            }
            PinnedEntryKind::RegularFile
                if path.extension().is_some_and(|extension| extension == "js") =>
            {
                if let Some(hook) = hooks.before_open {
                    hook(&path);
                }
                verify_discovery_authority(hooks)?;
                let file = openat(
                    directory,
                    &entry.name,
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )?;
                verify_opened_entry_identity(&entry, &file, &path)?;
                if let Some(hook) = hooks.before_read {
                    hook(&path);
                }
                verify_discovery_authority(hooks)?;
                if let Some(observer) = hooks.read_observer {
                    observer(&path);
                }
                verify_discovery_authority(hooks)?;
                let source =
                    read_opened_routine_source(&file, &path).map_err(RoutineSourceReadError::from);
                verify_discovery_authority(hooks)?;
                out.push(DiscoveredRoutineScript { path, source });
            }
            PinnedEntryKind::RegularFile | PinnedEntryKind::Other => {}
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn collect_routine_scripts_inner(
    current_path: &Path,
    hooks: RoutineDiscoveryHooks<'_>,
    out: &mut Vec<DiscoveredRoutineScript>,
) -> io::Result<()> {
    let mut entries = std::fs::read_dir(current_path)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if let Some(hook) = hooks.before_open {
            hook(&path);
        }
        verify_discovery_authority(hooks)?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_routine_scripts_inner(&path, hooks, out)?;
            continue;
        }
        if !file_type.is_file() || path.extension().is_none_or(|extension| extension != "js") {
            continue;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        options.custom_flags(0x0020_0000);
        let file = options.open(&path)?;
        if !file.metadata()?.is_file() || std::fs::symlink_metadata(&path)?.file_type().is_symlink()
        {
            return Err(io::Error::other(format!(
                "routine candidate `{}` is not a non-reparse regular file",
                path.display()
            )));
        }
        if let Some(hook) = hooks.before_read {
            hook(&path);
        }
        verify_discovery_authority(hooks)?;
        if let Some(observer) = hooks.read_observer {
            observer(&path);
        }
        verify_discovery_authority(hooks)?;
        let source = read_opened_routine_source(&file, &path).map_err(RoutineSourceReadError::from);
        verify_discovery_authority(hooks)?;
        out.push(DiscoveredRoutineScript { path, source });
    }
    Ok(())
}

pub(super) fn collect_routine_script_paths(
    root: &ValidatedRoutineRoot,
    hooks: RoutineDiscoveryHooks<'_>,
) -> io::Result<Vec<DiscoveredRoutineScript>> {
    #[cfg(unix)]
    {
        verify_discovery_authority(hooks)?;
        let directory = open_root(root)?;
        verify_discovery_authority(hooks)?;
        let mut out = Vec::new();
        collect_routine_scripts_inner(&directory, &root.canonical, hooks, &mut out)?;
        verify_discovery_authority(hooks)?;
        Ok(out)
    }
    #[cfg(not(unix))]
    {
        verify_discovery_authority(hooks)?;
        let mut out = Vec::new();
        collect_routine_scripts_inner(&root.canonical, hooks, &mut out)?;
        verify_discovery_authority(hooks)?;
        Ok(out)
    }
}
