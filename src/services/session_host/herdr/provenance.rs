//! OS reads behind E7: the socket peer pid, a process's wall-clock start and environment,
//! and one consistent read of a config file. Unsupported platforms read as Unverified.
#![cfg_attr(not(test), allow(dead_code))]

use std::fs::{File, Metadata};
use std::io::Read;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::observe::{ConfigRead, RestoreUnverified, ServerProvenance};

/// Larger than any canonical config; a bigger file is not read past this.
const CONFIG_READ_LIMIT: u64 = 4096;

pub(crate) struct OsProvenance;

impl ServerProvenance for OsProvenance {
    fn process_start(&self, pid: u32) -> Result<SystemTime, RestoreUnverified> {
        process_start(pid)
    }

    fn process_env(&self, pid: u32, key: &str) -> Result<Vec<String>, RestoreUnverified> {
        let entries = process_environ(pid)?;
        let prefix = format!("{key}=");
        Ok(entries
            .iter()
            .filter_map(|entry| entry.strip_prefix(prefix.as_str()))
            .map(str::to_string)
            .collect())
    }

    fn read_config(&self, path: &Path) -> Result<ConfigRead, RestoreUnverified> {
        read_config(path)
    }
}

/// Bytes and mtime come from one open file whose metadata is the same before and after
/// the read and still matches the path, so a write or replace during the read is caught.
fn read_config(path: &Path) -> Result<ConfigRead, RestoreUnverified> {
    let unreadable = |_| RestoreUnverified::ConfigUnreadable;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(RestoreUnverified::ConfigMissing);
        }
        Err(_) => return Err(RestoreUnverified::ConfigUnreadable),
    };
    let before = file.metadata().map_err(unreadable)?;
    if !before.is_file() {
        return Err(RestoreUnverified::ConfigUnreadable);
    }
    if before.len() > CONFIG_READ_LIMIT {
        return Err(RestoreUnverified::ConfigNotCanonical);
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(CONFIG_READ_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    let after = file.metadata().map_err(unreadable)?;
    let at_path = std::fs::metadata(path).map_err(unreadable)?;
    if !same_file_state(&before, &after) || !same_file_state(&before, &at_path) {
        return Err(RestoreUnverified::ConfigChanged);
    }
    let modified = before.modified().map_err(unreadable)?;
    Ok(ConfigRead { bytes, modified })
}

fn same_file_state(a: &Metadata, b: &Metadata) -> bool {
    #[cfg(unix)]
    let same_inode = {
        use std::os::unix::fs::MetadataExt;
        (a.dev(), a.ino(), a.ctime(), a.ctime_nsec())
            == (b.dev(), b.ino(), b.ctime(), b.ctime_nsec())
    };
    #[cfg(not(unix))]
    let same_inode = true;
    same_inode && a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

/// macOS reports a process start as epoch seconds and microseconds.
fn macos_start(seconds: u64, micros: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_micros(micros)
}

/// Linux reports a process start in clock ticks after boot; `/proc/stat` btime is boot
/// in whole epoch seconds, so the result can be up to a second early, never late.
fn linux_start(ticks: u64, ticks_per_second: u64, boot_seconds: u64) -> Option<SystemTime> {
    if ticks_per_second == 0 {
        return None;
    }
    let after_boot = Duration::from_secs(ticks / ticks_per_second)
        + Duration::from_nanos((ticks % ticks_per_second) * 1_000_000_000 / ticks_per_second);
    Some(UNIX_EPOCH + Duration::from_secs(boot_seconds) + after_boot)
}

/// The environment strings in a macOS `KERN_PROCARGS2` buffer: argc, the exec path and
/// its padding, argc arguments, then environment entries up to the first empty one.
fn procargs2_environ(raw: &[u8]) -> Option<Vec<String>> {
    let argc = usize::try_from(i32::from_ne_bytes(raw.get(..4)?.try_into().ok()?)).ok()?;
    let rest = &raw[4..];
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    let args_start = path_end + rest[path_end..].iter().position(|byte| *byte != 0)?;
    let mut strings = rest[args_start..].split(|byte| *byte == 0).skip(argc);
    let mut environ = Vec::new();
    for entry in strings.by_ref() {
        if entry.is_empty() {
            break;
        }
        environ.push(String::from_utf8_lossy(entry).into_owned());
    }
    Some(environ)
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn process_start(pid: u32) -> Result<SystemTime, RestoreUnverified> {
    use std::mem::MaybeUninit;
    let mut info: MaybeUninit<libc::proc_bsdinfo> = MaybeUninit::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info`; the return is checked.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written < size {
        return Err(RestoreUnverified::ProcessUnreadable);
    }
    // SAFETY: proc_pidinfo filled the whole struct.
    let info = unsafe { info.assume_init() };
    Ok(macos_start(info.pbi_start_tvsec, info.pbi_start_tvusec))
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn process_start(pid: u32) -> Result<SystemTime, RestoreUnverified> {
    let unreadable = RestoreUnverified::ProcessUnreadable;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|_| unreadable)?;
    let ticks = stat
        .rsplit_once(") ")
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .and_then(|field| field.parse::<u64>().ok())
        .ok_or(unreadable)?;
    let boot = std::fs::read_to_string("/proc/stat").map_err(|_| unreadable)?;
    let boot_seconds = boot
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .ok_or(unreadable)?;
    // SAFETY: sysconf only reads a configuration value.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let hz = u64::try_from(hz).map_err(|_| unreadable)?;
    linux_start(ticks, hz, boot_seconds).ok_or(unreadable)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_start(_pid: u32) -> Result<SystemTime, RestoreUnverified> {
    Err(RestoreUnverified::PlatformUnsupported)
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn process_environ(pid: u32) -> Result<Vec<String>, RestoreUnverified> {
    let unreadable = RestoreUnverified::ProcessUnreadable;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    // SAFETY: a size query; a null buffer asks the kernel only for the length.
    let probed = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if probed != 0 || size == 0 {
        return Err(unreadable);
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: the kernel writes at most `size` bytes into `buffer` and updates `size`.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return Err(unreadable);
    }
    buffer.truncate(size);
    procargs2_environ(&buffer).ok_or(unreadable)
}

#[cfg(target_os = "linux")]
fn process_environ(pid: u32) -> Result<Vec<String>, RestoreUnverified> {
    let raw = std::fs::read(format!("/proc/{pid}/environ"))
        .map_err(|_| RestoreUnverified::ProcessUnreadable)?;
    Ok(raw
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .collect())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_environ(_pid: u32) -> Result<Vec<String>, RestoreUnverified> {
    Err(RestoreUnverified::PlatformUnsupported)
}

/// The pid of the process that accepted this connection, as the kernel recorded it.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub(crate) fn socket_peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: LOCAL_PEERPID writes one pid_t into `pid`; the length is checked.
    let read = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &mut len,
        )
    };
    (read == 0 && pid > 0).then_some(pid as u32)
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) fn socket_peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED writes one ucred into `cred`; the length is checked.
    let read = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    (read == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub(crate) fn socket_peer_pid(_stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    None
}

#[cfg(test)]
#[path = "provenance_tests.rs"]
mod tests;
