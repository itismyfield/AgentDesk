//! Only an OS adapter can create the identity of an executable already mapped by a process.

use super::super::super::provenance::{PeerToken, ProcessStart};
use super::super::Unverified;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedImage {
    dev: u64,
    ino: u64,
}

impl LoadedImage {
    pub(super) fn matches(&self, checker: &super::FileToken) -> bool {
        (self.dev, self.ino) == checker.dev_ino()
    }
}

/// Whether this platform has a loaded-image adapter; image layouts are still judged per process.
pub(crate) const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

#[cfg(target_os = "linux")]
pub(super) fn for_process(
    pid: u32,
    _start: &ProcessStart,
    _generation: Option<&PeerToken>,
) -> Result<LoadedImage, Unverified> {
    use std::os::unix::fs::MetadataExt;
    // The procfs magic link names the mapped executable even after its pathname is replaced.
    let mapped = std::fs::metadata(format!("/proc/{pid}/exe")).map_err(|_| Unverified::Identity)?;
    Ok(LoadedImage {
        dev: mapped.dev(),
        ino: mapped.ino(),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn for_process(
    _pid: u32,
    _start: &ProcessStart,
    _generation: Option<&PeerToken>,
) -> Result<LoadedImage, Unverified> {
    // No safe vnode-identity adapter is available here; a pathname is not loaded-image evidence.
    Err(Unverified::Identity)
}

/// Every read happens between the generation reads that bracket the snapshot, so an exec of the
/// same pid in between is caught there; the token itself fences the text-path read.
#[cfg(target_os = "macos")]
pub(super) fn for_process(
    pid: u32,
    start: &ProcessStart,
    generation: Option<&PeerToken>,
) -> Result<LoadedImage, Unverified> {
    let token = generation
        .filter(|token| token.pid == pid)
        .ok_or(Unverified::Changed)?;
    let arch: ArchInfo = pid_info(pid, PROC_PIDARCHINFO, 0)?;
    let region = main_region(pid)?;
    let filesystem = filesystem_of(region.vip.fsid)?;
    let text_path = text_path(token)?;
    let stat = &region.vip.stat;
    judge(
        &Mapped {
            cputype: arch.cputype,
            offset: region.info.offset,
            protection: region.info.protection,
            path: c_bytes(region.vip.path.iter().copied()),
            text_path,
            // st_dev is a signed dev_t; widen it the way std's MetadataExt::dev does.
            dev: stat.dev as i32 as u64,
            ino: stat.ino,
            changed: (stat.ctime, stat.ctimensec),
            filesystem,
        },
        start,
    )
}

#[cfg(target_os = "macos")]
const PROC_PIDREGIONPATHINFO: libc::c_int = 8;
#[cfg(target_os = "macos")]
const PROC_PIDARCHINFO: libc::c_int = 19;
#[cfg(target_os = "macos")]
const CPU_TYPE_ARM64: i32 = 0x0100_000C;
#[cfg(target_os = "macos")]
const VM_PROT_EXECUTE: u32 = 4;
#[cfg(target_os = "macos")]
const REGION_LIMIT: usize = 64;
#[cfg(target_os = "macos")]
const NANOS: i128 = 1_000_000_000;

/// `struct proc_regioninfo`; only the fields read here are named.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
#[repr(C)]
struct RegionInfo {
    protection: u32,
    _max_protection_inheritance_flags: [u32; 3],
    offset: u64,
    _counters: [u32; 14],
    address: u64,
    size: u64,
}

/// `struct vinfo_stat`; only the fields read here are named.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
#[repr(C)]
struct VnodeStat {
    dev: u32,
    _mode_nlink: [u16; 2],
    ino: u64,
    _uid_gid: [u32; 2],
    _atime_mtime: [i64; 4],
    ctime: i64,
    ctimensec: i64,
    _rest: [u64; 8],
}

/// `struct vnode_info_path` with its `vnode_info` header flattened.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
#[repr(C)]
struct VnodeInfoPath {
    stat: VnodeStat,
    _type_pad: [i32; 2],
    fsid: [i32; 2],
    path: [u8; 1024],
}

/// `struct proc_regionwithpathinfo`.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
#[repr(C)]
struct RegionWithPath {
    info: RegionInfo,
    vip: VnodeInfoPath,
}

/// `struct proc_archinfo`.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
#[repr(C)]
struct ArchInfo {
    cputype: i32,
    _cpusubtype: i32,
}

/// `audit_token_t`.
#[cfg(target_os = "macos")]
#[repr(C)]
struct AuditToken {
    val: [u32; 8],
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn proc_pidpath_audittoken(
        token: *mut AuditToken,
        buffer: *mut libc::c_void,
        size: u32,
    ) -> libc::c_int;
}

/// What the kernel reported about one process's main image inside one generation bracket.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mapped {
    cputype: i32,
    offset: u64,
    protection: u32,
    path: Vec<u8>,
    text_path: Vec<u8>,
    dev: u64,
    ino: u64,
    changed: (i64, i64),
    filesystem: Vec<u8>,
}

/// Only a native arm64 single-architecture image on APFS is judged. APFS records change times in
/// ns, and `pbi_start` truncates to µs, so the start's µs is its certain lower bound.
#[cfg(target_os = "macos")]
fn judge(mapped: &Mapped, start: &ProcessStart) -> Result<LoadedImage, Unverified> {
    if mapped.cputype != CPU_TYPE_ARM64
        || mapped.offset != 0
        || mapped.protection & VM_PROT_EXECUTE == 0
        || mapped.path.is_empty()
        || mapped.ino == 0
    {
        return Err(Unverified::ImageLayoutUnsupported);
    }
    if mapped.text_path != mapped.path {
        return Err(Unverified::Identity);
    }
    if mapped.filesystem != b"apfs" {
        return Err(Unverified::ImageFilesystemUnsupported);
    }
    let super::super::super::provenance::StartIdentity::Darwin { seconds, micros } = start.identity
    else {
        return Err(Unverified::Identity);
    };
    let (seconds_changed, nanos_changed) = mapped.changed;
    if !(0..1_000_000_000).contains(&nanos_changed) {
        return Err(Unverified::Identity);
    }
    let started = i128::from(seconds) * NANOS + i128::from(micros) * 1_000;
    let changed = i128::from(seconds_changed) * NANOS + i128::from(nanos_changed);
    if changed > started {
        // Inside the start µs the order is unknown; past it the change came after the start.
        return Err(if changed < started + 1_000 {
            Unverified::ImageTimeAmbiguous
        } else {
            Unverified::ImageChangedSinceStart
        });
    }
    Ok(LoadedImage {
        dev: mapped.dev,
        ino: mapped.ino,
    })
}

/// One `proc_pidinfo` flavor that must fill the whole plain-integer struct `T`.
#[cfg(target_os = "macos")]
fn pid_info<T: Copy>(pid: u32, flavor: libc::c_int, arg: u64) -> Result<T, Unverified> {
    let pid = libc::c_int::try_from(pid).map_err(|_| Unverified::Identity)?;
    let size = libc::c_int::try_from(std::mem::size_of::<T>()).map_err(|_| Unverified::Identity)?;
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    // SAFETY: proc_pidinfo writes at most `size` bytes into `value`; the written count is checked.
    let written = unsafe { libc::proc_pidinfo(pid, flavor, arg, value.as_mut_ptr().cast(), size) };
    if written != size {
        return Err(Unverified::Identity);
    }
    // SAFETY: `value` was zeroed and then filled; T holds only integers.
    Ok(unsafe { value.assume_init() })
}

/// The lowest file-backed mapping: under the arm64 page-zero nothing maps below the main image.
#[cfg(target_os = "macos")]
fn main_region(pid: u32) -> Result<RegionWithPath, Unverified> {
    let mut address = 0u64;
    for _ in 0..REGION_LIMIT {
        let region: RegionWithPath = pid_info(pid, PROC_PIDREGIONPATHINFO, address)?;
        if region.vip.stat.ino != 0 && region.vip.path[0] != 0 {
            return Ok(region);
        }
        address = region
            .info
            .address
            .checked_add(region.info.size)
            .filter(|next| *next > address)
            .ok_or(Unverified::Identity)?;
    }
    Err(Unverified::Identity)
}

/// The type name of the mounted filesystem that holds `fsid`; unknown mounts read as empty.
#[cfg(target_os = "macos")]
fn filesystem_of(fsid: [i32; 2]) -> Result<Vec<u8>, Unverified> {
    // SAFETY: a null buffer asks getfsstat only for the mount count.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    let count = usize::try_from(count).map_err(|_| Unverified::Identity)?;
    // SAFETY: statfs holds only integers and byte arrays, so all-zero is a valid value.
    let mut mounts = vec![unsafe { std::mem::zeroed::<libc::statfs>() }; count + 8];
    let bytes = libc::c_int::try_from(mounts.len() * std::mem::size_of::<libc::statfs>())
        .map_err(|_| Unverified::Identity)?;
    // SAFETY: getfsstat writes at most `bytes` into `mounts` and returns how many it filled.
    let filled = unsafe { libc::getfsstat(mounts.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    let filled = usize::try_from(filled)
        .ok()
        .filter(|filled| *filled <= mounts.len())
        .ok_or(Unverified::Identity)?;
    for mount in &mounts[..filled] {
        // SAFETY: fsid_t is a #[repr(C)] wrapper around two i32 values.
        let id = unsafe { std::ptr::addr_of!(mount.f_fsid).cast::<[i32; 2]>().read() };
        if id == fsid {
            return Ok(c_bytes(mount.f_fstypename.iter().map(|c| *c as u8)));
        }
    }
    Ok(Vec::new())
}

/// The executable path of the token's exact process generation; any later exec refuses it.
#[cfg(target_os = "macos")]
fn text_path(token: &PeerToken) -> Result<Vec<u8>, Unverified> {
    let mut audit = AuditToken { val: token.raw };
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let size = u32::try_from(buffer.len()).map_err(|_| Unverified::Identity)?;
    // SAFETY: the kernel writes at most `size` bytes and compares the token's pid version.
    let written = unsafe { proc_pidpath_audittoken(&mut audit, buffer.as_mut_ptr().cast(), size) };
    if written <= 0 {
        return Err(Unverified::Changed);
    }
    Ok(c_bytes(buffer))
}

#[cfg(target_os = "macos")]
fn c_bytes(bytes: impl IntoIterator<Item = u8>) -> Vec<u8> {
    bytes.into_iter().take_while(|byte| *byte != 0).collect()
}

#[cfg(test)]
thread_local! {
    static AFTER_IMAGE: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs a test's step after the image reads and before the closing generation read.
#[cfg(test)]
pub(super) fn after_image_for_test() {
    AFTER_IMAGE.with(|hook| {
        if let Some(step) = hook.borrow_mut().as_mut() {
            step();
        }
    });
}

#[cfg(test)]
pub(super) fn fixture(identity: (u64, u64)) -> LoadedImage {
    LoadedImage {
        dev: identity.0,
        ino: identity.1,
    }
}

#[cfg(all(test, target_os = "macos"))]
#[path = "loaded_image_tests.rs"]
mod tests;
