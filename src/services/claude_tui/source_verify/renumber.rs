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
    #[cfg(test)]
    if super::n2b_mutant("renumber-off") {
        return false;
    }
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

/// `pin` under the device `file` was found on, which is what a re-pin records.
pub(crate) fn under_new_device(pin: &SourceId, (dev, _): (u64, u64)) -> SourceId {
    #[cfg(test)]
    if super::n2b_mutant("renumber-return-old-pin") {
        return pin.clone();
    }
    SourceId { dev, ..pin.clone() }
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
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    use super::super::{
        ClaudeHookSource, Observation, PinJudgment, SourceHistory, SourceVerdict, judge_pin,
        observe_transcript, verify_claude_source,
    };
    use super::*;

    #[test]
    fn this_host_reports_a_boot_time_in_the_past() {
        let boot = os_boot_time().expect("a unix host reports when it booted");
        assert!(boot < Utc::now(), "{boot}");
    }

    /// `<root>/<project>/<session>.jsonl` whose first record names `session`.
    fn transcript(root: &Path, project: &str, session: &str) -> PathBuf {
        let path = root.join(project).join(format!("{session}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let row = serde_json::json!({"type": "mode", "sessionId": session});
        std::fs::write(&path, format!("{row}\n")).unwrap();
        path
    }

    fn identity(session: &str, path: &Path) -> SourceId {
        let meta = std::fs::metadata(path).unwrap();
        let (session_id, path) = (session.to_owned(), path.to_path_buf());
        let (dev, ino) = (meta.dev(), meta.ino());
        SourceId {
            session_id,
            path,
            dev,
            ino,
        }
    }

    /// A stat of the file is judged against a pin taken at `pinned_at` under a boot at `boot`.
    #[test]
    fn only_an_earlier_boots_pin_of_the_same_inode_and_path_is_renumbered() {
        let root = tempfile::tempdir().unwrap();
        let s = uuid::Uuid::new_v4().to_string();
        let path = transcript(root.path(), "-work", &s);
        let real = identity(&s, &path);
        let boot = Utc::now();
        let second = chrono::Duration::seconds(1);
        let moved = SourceId {
            dev: real.dev + 1,
            ..real.clone()
        };
        let elsewhere = SourceId {
            path: root.path().join("-other").join(format!("{s}.jsonl")),
            ..moved.clone()
        };
        let other_ino = SourceId {
            ino: real.ino + 1,
            ..moved.clone()
        };
        let cases = [
            (
                "earlier boot",
                &moved,
                Some(boot - second),
                Some(boot),
                "renumbered",
            ),
            ("at the boot", &moved, Some(boot), Some(boot), "anomaly"),
            (
                "this boot",
                &moved,
                Some(boot + second),
                Some(boot),
                "anomaly",
            ),
            ("pin time unknown", &moved, None, Some(boot), "anomaly"),
            ("boot unknown", &moved, Some(boot - second), None, "anomaly"),
            (
                "another inode",
                &other_ino,
                Some(boot - second),
                Some(boot),
                "anomaly",
            ),
            (
                "another project",
                &elsewhere,
                Some(boot - second),
                Some(boot),
                "unpinned",
            ),
            (
                "same device",
                &real,
                Some(boot - second),
                Some(boot),
                "verified",
            ),
        ];
        for (case, pin, pinned_at, boot, expected) in cases {
            let _boot = boot_time_for_tests(boot);
            let seen = Observation::Stat(Ok((real.dev, real.ino)));
            let judged = match judge_pin(Some(pin), pinned_at, &s, &path, seen) {
                PinJudgment::Renumbered => "renumbered",
                PinJudgment::Anomaly => "anomaly",
                PinJudgment::Unpinned => "unpinned",
                PinJudgment::Verified(id) if id == real => "verified",
                other => panic!("{case}: {other:?}"),
            };
            assert_eq!(judged, expected, "{case}");
        }
    }

    /// The bound session's hook after a reboot confirms its file under the new device, which the
    /// next hook then finds current.
    #[test]
    fn a_bound_hook_after_a_reboot_confirms_the_new_device_then_finds_it_current() {
        let root = tempfile::tempdir().unwrap();
        let s = uuid::Uuid::new_v4().to_string();
        let path = transcript(root.path(), "-work", &s);
        let real = identity(&s, &path);
        let boot = Utc::now();
        let _boot = boot_time_for_tests(Some(boot));
        let hook = ClaudeHookSource {
            event: "SessionStart".into(),
            session_id: s.clone(),
            transcript_path: path.clone(),
            start_source: Some("resume".into()),
            published_at: None,
        };
        let opened = observe_transcript(&path).unwrap();
        let history = SourceHistory::default();
        let moved = SourceId {
            dev: real.dev + 1,
            ..real.clone()
        };
        let earlier = Some(boot - chrono::Duration::seconds(1));
        let pinned = (Some(&moved), earlier);
        let verdict = verify_claude_source(&hook, root.path(), Ok(&opened), &s, pinned, &history);
        let SourceVerdict::Confirm(source) = verdict else {
            panic!("{verdict:?}");
        };
        assert_eq!(source.source_id(), Some(real.clone()));
        let repinned = (Some(&real), Some(boot + chrono::Duration::seconds(1)));
        let verdict = verify_claude_source(&hook, root.path(), Ok(&opened), &s, repinned, &history);
        assert_eq!(verdict, SourceVerdict::Current);
    }
}
