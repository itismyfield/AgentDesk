use super::{LoadedRoutineScript, RoutineScriptCandidate};
use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(unix)]
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub(super) const MAX_ROUTINE_SOURCE_BYTES: u64 = 1024 * 1024;

#[cfg(unix)]
use std::os::fd::{AsRawFd as _, FromRawFd as _};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;

mod authority;
mod bounded_scan;
#[cfg(test)]
mod tests;
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
use bounded_scan::{DEFAULT_ROUTINE_TREE_LIMITS, TraversalBudget};
#[cfg(test)]
use bounded_scan::{RoutineTreeLimits, collect_routine_script_paths_with_limits};
pub(super) use bounded_scan::{collect_routine_script_paths, require_nonempty_routine_tree};

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
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "routine candidate `{}` has {} hard links; code files must have exactly one link",
                path.display(),
                metadata.nlink()
            ),
        ));
    }
    if metadata.len() > MAX_ROUTINE_SOURCE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "routine candidate `{}` exceeds {} bytes",
                path.display(),
                MAX_ROUTINE_SOURCE_BYTES
            ),
        ));
    }
    let mut source = Vec::with_capacity(metadata.len() as usize);
    let mut limited = file.take(MAX_ROUTINE_SOURCE_BYTES + 1);
    limited.read_to_end(&mut source)?;
    if source.len() as u64 > MAX_ROUTINE_SOURCE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "routine candidate `{}` exceeds {} bytes",
                path.display(),
                MAX_ROUTINE_SOURCE_BYTES
            ),
        ));
    }
    String::from_utf8(source).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "routine candidate `{}` is not UTF-8: {error}",
                path.display()
            ),
        )
    })
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
        mount_id: Option<u64>,
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
        match mount_id {
            Some(mount_id) => {
                hasher.update([1]);
                hasher.update(mount_id.to_le_bytes());
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
        {
            #[cfg(unix)]
            {
                runtime_authority.mount_id
            }
            #[cfg(not(unix))]
            {
                None
            }
        },
    );
    for root in roots {
        hasher.update(b"root");
        update_path(&mut hasher, &root.configured);
        update_path(&mut hasher, &root.canonical);
        #[cfg(unix)]
        let identity = root
            .identity
            .map(|identity| (identity.device, identity.inode));
        #[cfg(not(unix))]
        let identity = None;
        update_identity(&mut hasher, root.exists, root.kind, identity, {
            #[cfg(unix)]
            {
                root.mount_id
            }
            #[cfg(not(unix))]
            {
                None
            }
        });
    }
    hasher.update(b"helper");
    update_path(&mut hasher, &helper_authority.configured);
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
        {
            #[cfg(unix)]
            {
                helper_authority.mount_id
            }
            #[cfg(not(unix))]
            {
                None
            }
        },
    );
    #[cfg(unix)]
    {
        let mut entry_identities = helper_authority
            .entry_identities
            .iter()
            .copied()
            .collect::<Vec<_>>();
        entry_identities.sort_unstable();
        hasher.update(b"helper-entries");
        hasher.update((entry_identities.len() as u64).to_le_bytes());
        for identity in entry_identities {
            hasher.update(identity.device.to_le_bytes());
            hasher.update(identity.inode.to_le_bytes());
        }
    }
    PathBuf::from(hex::encode(hasher.finalize()))
}

pub(super) fn script_ref(root: &Path, path: &Path) -> String {
    let script_ref = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    #[cfg(windows)]
    {
        script_ref.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        script_ref
    }
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
fn directory_entries(
    directory: &File,
    current_path: &Path,
    budget: &mut TraversalBudget,
) -> io::Result<Vec<PinnedDirectoryEntry>> {
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
        budget.record_entry(current_path)?;
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
pub(super) fn directory_mount_id(directory: &File) -> io::Result<Option<u64>> {
    #[cfg(target_os = "linux")]
    {
        let mut statx = std::mem::MaybeUninit::<libc::statx>::zeroed();
        let empty_path = b"\0";
        // SAFETY: `directory` is live, the empty path is NUL-terminated, and
        // `statx` points to writable storage for the complete result.
        let result = unsafe {
            libc::statx(
                directory.as_raw_fd(),
                empty_path.as_ptr().cast(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_MNT_ID,
                statx.as_mut_ptr(),
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error
                .raw_os_error()
                .is_some_and(|code| code == libc::ENOSYS || code == libc::EINVAL)
            {
                return proc_fd_mount_id(directory).map(Some);
            }
            return Err(error);
        }
        // SAFETY: successful `statx` initialized the complete result.
        let statx = unsafe { statx.assume_init() };
        if statx.stx_mask & libc::STATX_MNT_ID == 0 {
            return proc_fd_mount_id(directory).map(Some);
        }
        return Ok(Some(statx.stx_mnt_id));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = directory;
        Ok(None)
    }
}

#[cfg(target_os = "linux")]
fn proc_fd_mount_id(file: &File) -> io::Result<u64> {
    let fdinfo_path = format!("/proc/self/fdinfo/{}", file.as_raw_fd());
    let fdinfo = std::fs::read_to_string(&fdinfo_path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to read mount authority from `{fdinfo_path}`: {error}"),
        )
    })?;
    parse_proc_fdinfo_mount_id(&fdinfo)
}

#[cfg(target_os = "linux")]
fn parse_proc_fdinfo_mount_id(fdinfo: &str) -> io::Result<u64> {
    let value = fdinfo
        .lines()
        .find_map(|line| line.split_once(':').filter(|(key, _)| *key == "mnt_id"))
        .map(|(_, value)| value.trim())
        .ok_or_else(|| io::Error::other("fdinfo omitted required mnt_id authority"))?;
    value.parse::<u64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("fdinfo contained invalid mnt_id `{value}`: {error}"),
        )
    })
}

#[cfg(unix)]
fn open_root(root: &ValidatedRoutineRoot) -> io::Result<File> {
    let retained = root.handle.as_ref().ok_or_else(|| {
        io::Error::other(format!(
            "validated routine root `{}` has no retained authority handle",
            root.canonical.display()
        ))
    })?;
    let directory = openat(
        retained,
        OsStr::new("."),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
    )?;
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
    if directory_mount_id(&directory)? != root.mount_id {
        return Err(io::Error::other(format!(
            "validated routine root `{}` changed mount authority",
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
fn verify_entry_provenance(
    opened: &File,
    path: &Path,
    root_identity: FileIdentity,
    root_mount_id: Option<u64>,
    entry_kind: &str,
) -> io::Result<FileIdentity> {
    let metadata = opened.metadata()?;
    let identity = require_available_identity(FileIdentity::from_metadata(&metadata), path)?;
    if identity.device != root_identity.device {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "routine {entry_kind} `{}` crosses filesystem device authority",
                path.display()
            ),
        ));
    }
    let mount_id = directory_mount_id(opened)?;
    verify_mount_authority(mount_id, root_mount_id, path, entry_kind)?;
    Ok(identity)
}

#[cfg(unix)]
fn verify_mount_authority(
    observed_mount_id: Option<u64>,
    root_mount_id: Option<u64>,
    path: &Path,
    entry_kind: &str,
) -> io::Result<()> {
    if observed_mount_id != root_mount_id {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "routine {entry_kind} `{}` crosses mount authority",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn collect_helper_identities_inner(
    directory: &File,
    current_path: &Path,
    depth: usize,
    root_identity: FileIdentity,
    root_mount_id: Option<u64>,
    identities: &mut HashSet<FileIdentity>,
    budget: &mut TraversalBudget,
) -> io::Result<()> {
    for entry in directory_entries(directory, current_path, budget)? {
        let path = current_path.join(&entry.name);
        match entry.kind {
            PinnedEntryKind::Directory => {
                budget.verify_depth(depth + 1, &path)?;
                let child = openat(
                    directory,
                    &entry.name,
                    libc::O_RDONLY
                        | libc::O_DIRECTORY
                        | libc::O_NOFOLLOW
                        | libc::O_NONBLOCK
                        | libc::O_CLOEXEC,
                )?;
                verify_opened_entry_identity(&entry, &child, &path)?;
                let identity = verify_entry_provenance(
                    &child,
                    &path,
                    root_identity,
                    root_mount_id,
                    "helper directory",
                )?;
                if !identities.insert(identity) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "helper authority cycle or alias detected at `{}`",
                            path.display()
                        ),
                    ));
                }
                collect_helper_identities_inner(
                    &child,
                    &path,
                    depth + 1,
                    root_identity,
                    root_mount_id,
                    identities,
                    budget,
                )?;
            }
            PinnedEntryKind::RegularFile => {
                budget.record_file(&path)?;
                let file = openat(
                    directory,
                    &entry.name,
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )?;
                verify_opened_entry_identity(&entry, &file, &path)?;
                let identity = verify_entry_provenance(
                    &file,
                    &path,
                    root_identity,
                    root_mount_id,
                    "helper file",
                )?;
                if !identities.insert(identity) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "helper authority cycle or alias detected at `{}`",
                            path.display()
                        ),
                    ));
                }
            }
            PinnedEntryKind::Other => {}
        }
    }
    Ok(())
}

#[cfg(unix)]
fn collect_helper_authority_identities(
    directory: &File,
    root_path: &Path,
    root_mount_id: Option<u64>,
) -> io::Result<HashSet<FileIdentity>> {
    let root_identity = require_available_identity(
        FileIdentity::from_metadata(&directory.metadata()?),
        root_path,
    )?;
    let mut identities = HashSet::from([root_identity]);
    let mut budget = TraversalBudget::new(DEFAULT_ROUTINE_TREE_LIMITS);
    collect_helper_identities_inner(
        directory,
        root_path,
        0,
        root_identity,
        root_mount_id,
        &mut identities,
        &mut budget,
    )?;
    Ok(identities)
}

#[cfg(unix)]
struct UnixTraversalAuthority<'a> {
    root_identity: FileIdentity,
    root_mount_id: Option<u64>,
    forbidden_entry_identities: &'a HashSet<FileIdentity>,
    visited_directory_identities: HashSet<FileIdentity>,
}

#[cfg(unix)]
fn validate_routine_namespace_name(
    name: &OsStr,
    kind: PinnedEntryKind,
    parent: &Path,
) -> io::Result<()> {
    let participates_in_routine_namespace = kind == PinnedEntryKind::Directory
        || (kind == PinnedEntryKind::RegularFile
            && Path::new(name)
                .extension()
                .is_some_and(|extension| extension == "js"));
    if participates_in_routine_namespace && name.to_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "routine entry under `{}` has a non-UTF-8 name; script references must be injective UTF-8 paths",
                parent.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn collect_routine_scripts_inner(
    directory: &File,
    current_path: &Path,
    depth: usize,
    hooks: RoutineDiscoveryHooks<'_>,
    authority: &mut UnixTraversalAuthority<'_>,
    budget: &mut TraversalBudget,
    out: &mut Vec<DiscoveredRoutineScript>,
) -> io::Result<()> {
    let mut entries = directory_entries(directory, current_path, budget)?;
    entries.sort_by(|first, second| first.name.cmp(&second.name));
    for entry in entries {
        validate_routine_namespace_name(&entry.name, entry.kind, current_path)?;
        let path = current_path.join(&entry.name);
        if entry.kind == PinnedEntryKind::RegularFile {
            budget.record_file(&path)?;
        }
        match entry.kind {
            PinnedEntryKind::Directory => {
                budget.verify_depth(depth + 1, &path)?;
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
                let identity = verify_entry_provenance(
                    &child_directory,
                    &path,
                    authority.root_identity,
                    authority.root_mount_id,
                    "directory",
                )?;
                if authority.forbidden_entry_identities.contains(&identity) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "routine directory `{}` aliases the protected routine helper surface",
                            path.display()
                        ),
                    ));
                }
                if !authority.visited_directory_identities.insert(identity) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "routine directory authority cycle or alias detected at `{}`",
                            path.display()
                        ),
                    ));
                }
                verify_discovery_authority(hooks)?;
                collect_routine_scripts_inner(
                    &child_directory,
                    &path,
                    depth + 1,
                    hooks,
                    authority,
                    budget,
                    out,
                )?;
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
                let identity = verify_entry_provenance(
                    &file,
                    &path,
                    authority.root_identity,
                    authority.root_mount_id,
                    "file",
                )?;
                if authority.forbidden_entry_identities.contains(&identity) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "routine file `{}` aliases the protected routine helper surface",
                            path.display()
                        ),
                    ));
                }
                let source_length = file.metadata()?.len();
                if source_length <= MAX_ROUTINE_SOURCE_BYTES {
                    budget.reserve_source(source_length, &path)?;
                }
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
    depth: usize,
    hooks: RoutineDiscoveryHooks<'_>,
    budget: &mut TraversalBudget,
    out: &mut Vec<DiscoveredRoutineScript>,
) -> io::Result<()> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(current_path)? {
        budget.record_entry(current_path)?;
        entries.push(entry?);
    }
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if let Some(hook) = hooks.before_open {
            hook(&path);
        }
        verify_discovery_authority(hooks)?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            budget.verify_depth(depth + 1, &path)?;
            collect_routine_scripts_inner(&path, depth + 1, hooks, budget, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        budget.record_file(&path)?;
        if path.extension().is_none_or(|extension| extension != "js") {
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
        let source_length = file.metadata()?.len();
        if source_length <= MAX_ROUTINE_SOURCE_BYTES {
            budget.reserve_source(source_length, &path)?;
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
