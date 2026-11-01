use super::MemoryResources;

/// The `host_statistics64` page counts Activity Monitor's "Memory Used" adds up.
#[derive(Clone, Copy, Debug)]
pub(super) struct VmPages {
    pub internal: u64,
    pub purgeable: u64,
    pub wired: u64,
    pub compressed: u64,
}

/// Used = app (anonymous minus purgeable) + wired + compressed. Cached files
/// and inactive pages are reclaimable, so they count as available.
pub(super) fn activity_monitor(
    total_bytes: u64,
    page_bytes: u64,
    pages: VmPages,
) -> MemoryResources {
    let used_pages = pages
        .internal
        .saturating_sub(pages.purgeable)
        .saturating_add(pages.wired)
        .saturating_add(pages.compressed);
    let used_bytes = used_pages.saturating_mul(page_bytes).min(total_bytes);
    MemoryResources {
        total_bytes,
        used_bytes,
        available_bytes: total_bytes - used_bytes,
    }
}

/// sysinfo subtracts compressor pages from its macOS "available" figure, which
/// reaches 0 B under compression, so the page counts are read directly.
#[cfg(target_os = "macos")]
pub(super) fn vm_pages(host: libc::mach_port_t) -> Option<VmPages> {
    let mut stat = std::mem::MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `stat` is a zeroed `vm_statistics64` and `count` is its size in
    // `integer_t` words, as `host_statistics64` requires for HOST_VM_INFO64.
    let stat = unsafe {
        if libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            stat.as_mut_ptr().cast(),
            &mut count,
        ) != libc::KERN_SUCCESS
        {
            return None;
        }
        stat.assume_init()
    };
    Some(VmPages {
        internal: u64::from(stat.internal_page_count),
        purgeable: u64::from(stat.purgeable_count),
        wired: u64::from(stat.wire_count),
        compressed: u64::from(stat.compressor_page_count),
    })
}

#[cfg(target_os = "macos")]
#[allow(deprecated)] // libc points at the mach2 crate; the call itself is stable.
pub(super) fn host_port() -> libc::mach_port_t {
    // SAFETY: mach_host_self has no preconditions.
    unsafe { libc::mach_host_self() }
}

#[cfg(target_os = "macos")]
pub(super) fn page_bytes() -> Option<u64> {
    // SAFETY: sysconf has no preconditions.
    u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .ok()
        .filter(|bytes| *bytes > 0)
}
