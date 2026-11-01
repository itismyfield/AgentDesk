//! A pinned transcript whose volume came back under another device number after a reboot.
//! Only the device may differ, and only for a pin committed before this boot.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};

use crate::services::tui_prompt_dedupe::binding_events::SourceId;

/// Whether `file` is `pin`'s own file renumbered by a reboot: same inode on another device, pinned
/// by a record committed before this boot. Without either time it is not.
pub(crate) fn renumbered(
    pin: &SourceId,
    pinned_at: Option<DateTime<Utc>>,
    (dev, ino): (u64, u64),
) -> bool {
    let earlier_boot = pinned_at
        .zip(boot_time())
        .is_some_and(|(at, boot)| at < boot);
    #[cfg(test)]
    let earlier_boot = earlier_boot || super::n2b_mutant("renumber-ignore-boot");
    #[cfg(test)]
    if super::n2b_mutant("renumber-ignore-ino") {
        return pin.dev != dev && earlier_boot;
    }
    pin.ino == ino && pin.dev != dev && earlier_boot
}

/// When this host booted, read once per process so a later clock change cannot move it.
fn boot_time() -> Option<DateTime<Utc>> {
    #[cfg(test)]
    if let Some(boot) = TEST_BOOT.with(std::cell::Cell::get) {
        return boot;
    }
    static BOOT: OnceLock<DateTime<Utc>> = OnceLock::new();
    if let Some(boot) = BOOT.get() {
        return Some(*boot);
    }
    let boot = os_boot_time()?;
    Some(*BOOT.get_or_init(|| boot))
}

#[cfg(target_os = "macos")]
fn os_boot_time() -> Option<DateTime<Utc>> {
    let mut value = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut size = std::mem::size_of::<libc::timeval>();
    let name = c"kern.boottime";
    let read = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut libc::timeval).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 || size != std::mem::size_of::<libc::timeval>() || value.tv_sec <= 0 {
        return None;
    }
    let micros = u32::try_from(value.tv_usec).ok()?;
    DateTime::from_timestamp(value.tv_sec, micros.saturating_mul(1_000))
}

#[cfg(target_os = "linux")]
fn os_boot_time() -> Option<DateTime<Utc>> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().find_map(|line| line.strip_prefix("btime "))?;
    let secs = line.trim().parse::<i64>().ok().filter(|secs| *secs > 0)?;
    DateTime::from_timestamp(secs, 0)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn os_boot_time() -> Option<DateTime<Utc>> {
    None
}

#[cfg(test)]
thread_local! {
    /// `Some(boot)` replaces this thread's boot time; `Some(None)` makes it unknown.
    static TEST_BOOT: std::cell::Cell<Option<Option<DateTime<Utc>>>> = const { std::cell::Cell::new(None) };
}

/// Sets this thread's boot time until the guard drops; `None` makes the boot time unknown.
#[cfg(test)]
pub(crate) fn boot_time_for_tests(boot: Option<DateTime<Utc>>) -> TestBootGuard {
    TEST_BOOT.with(|cell| cell.set(Some(boot)));
    TestBootGuard
}

#[cfg(test)]
pub(crate) struct TestBootGuard;

#[cfg(test)]
impl Drop for TestBootGuard {
    fn drop(&mut self) {
        TEST_BOOT.with(|cell| cell.set(None));
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn this_host_reports_a_boot_time_in_the_past() {
        let boot = os_boot_time().expect("a unix host reports when it booted");
        assert!(boot < Utc::now(), "{boot}");
    }
}
