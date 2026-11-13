use super::{environment, requested};

#[derive(Debug)]
pub(crate) struct HeldLease {
    #[cfg(unix)]
    _file: std::fs::File,
}

fn rejected() -> String {
    "test instance lease admission rejected".into()
}

pub(crate) fn legacy_cleanup_if_unscoped(cleanup: impl FnOnce()) {
    if !requested() {
        cleanup();
    }
}

pub(crate) fn admit_dcserver() -> Result<Option<HeldLease>, String> {
    if !requested() {
        return Ok(None);
    }
    environment::runtime_path()?.ok_or_else(rejected)?;
    #[cfg(unix)]
    {
        admit(super::current()?.ok_or_else(rejected)?).map(Some)
    }
    #[cfg(not(unix))]
    {
        Err(rejected())
    }
}

#[cfg(unix)]
#[derive(Debug, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Witness {
    schema_version: u8,
    fd: i32,
    device: u64,
    inode: u64,
    instance_id: String,
    root: std::path::PathBuf,
    nonce: String,
}

#[cfg(unix)]
fn admit(instance: &super::TestInstance) -> Result<HeldLease, String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::FileExt;

    let encoded = std::env::var("AGENTDESK_TEST_LEASE").map_err(|_| rejected())?;
    if encoded.len() > 4096 {
        return Err(rejected());
    }
    let witness: Witness = serde_json::from_str(&encoded).map_err(|_| rejected())?;
    if witness.schema_version != 1
        || witness.fd < 3
        || witness.root != instance.root
        || witness.instance_id != instance.instance_id
        || !uuid::Uuid::parse_str(&witness.nonce)
            .is_ok_and(|nonce| !nonce.is_nil() && nonce.hyphenated().to_string() == witness.nonce)
    {
        return Err(rejected());
    }
    let flags = unsafe { libc::fcntl(witness.fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(rejected());
    }
    // Startup takes ownership once and closes it on rejection; it never unlocks the shared OFD.
    let inherited = unsafe { std::fs::File::from_raw_fd(witness.fd) };
    let metadata = inherited.metadata().map_err(|_| rejected())?;
    check_file(&metadata, &witness)?;
    let probe = open_control(&instance.root)?;
    check_file(&probe.metadata().map_err(|_| rejected())?, &witness)?;
    // An independent open must collide, and the inherited OFD must already own that lock.
    if unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0
        || !matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EWOULDBLOCK)
        )
        || unsafe { libc::flock(inherited.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
    {
        return Err(rejected());
    }
    check_file(
        &open_control(&instance.root)?
            .metadata()
            .map_err(|_| rejected())?,
        &witness,
    )?;
    let mut bytes = [0; 4097];
    let length = inherited.read_at(&mut bytes, 0).map_err(|_| rejected())?;
    if length > 4096
        || serde_json::from_slice::<Witness>(&bytes[..length]).map_err(|_| rejected())? != witness
    {
        return Err(rejected());
    }
    if unsafe {
        libc::fcntl(
            inherited.as_raw_fd(),
            libc::F_SETFD,
            flags | libc::FD_CLOEXEC,
        )
    } < 0
    {
        return Err(rejected());
    }
    // Admission runs before worker threads; auxiliary execs receive neither FD nor witness.
    unsafe { std::env::remove_var("AGENTDESK_TEST_LEASE") };
    Ok(HeldLease { _file: inherited })
}

#[cfg(unix)]
fn check_file(metadata: &std::fs::Metadata, witness: &Witness) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o7777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.dev() != witness.device
        || metadata.ino() != witness.inode
    {
        return Err(rejected());
    }
    Ok(())
}

#[cfg(unix)]
fn open_control(root: &std::path::Path) -> Result<std::fs::File, String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

    let parent = root.parent().ok_or_else(rejected)?;
    let mut directory = std::fs::File::open("/").map_err(|_| rejected())?;
    for part in parent.components().skip(1) {
        let std::path::Component::Normal(part) = part else {
            return Err(rejected());
        };
        let part = std::ffi::CString::new(part.as_bytes()).map_err(|_| rejected())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                part.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(rejected());
        }
        directory = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    let metadata = directory.metadata().map_err(|_| rejected())?;
    if metadata.mode() & 0o7777 != 0o700 || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(rejected());
    }
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".gateway.lease".as_ptr(),
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(rejected());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(all(test, unix))]
#[path = "lifecycle_tests.rs"]
mod tests;
