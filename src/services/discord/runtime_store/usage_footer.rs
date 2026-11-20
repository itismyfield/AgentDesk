use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use super::{AtomicWriteContext, agentdesk_root, critical_publish};

const STORE: &str = "discord_usage_footer";
const DEFAULT_ENABLED: bool = true;
static WRITE: Mutex<()> = Mutex::new(());

fn path(root: &Path, channel: u64) -> PathBuf {
    root.join("runtime")
        .join(STORE)
        .join(format!("{channel}.json"))
}

fn read(path: &Path) -> io::Result<bool> {
    match fs::read_to_string(path) {
        Ok(value) => serde_json::from_str(&value).map_err(io::Error::from),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DEFAULT_ENABLED),
        Err(error) => Err(error),
    }
}

pub(crate) fn enabled(channel: u64) -> bool {
    agentdesk_root().map_or(DEFAULT_ENABLED, |root| {
        read(&path(&root, channel)).unwrap_or_else(|error| {
            tracing::warn!(channel, %error, "usage footer preference unreadable");
            DEFAULT_ENABLED
        })
    })
}

pub(in crate::services::discord) fn update(
    channel: u64,
    requested: Option<bool>,
) -> Result<bool, String> {
    let root = agentdesk_root().ok_or("쿼터 표시 설정 저장 위치를 확인하지 못했습니다.")?;
    update_at(&root, channel, requested)
}

fn update_at(root: &Path, channel: u64, requested: Option<bool>) -> Result<bool, String> {
    let target = path(root, channel);
    let write_guard = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    let enabled = match requested {
        Some(value) => value,
        None => !read(&target).map_err(|error| error.to_string())?,
    };
    critical_publish(
        &target,
        if enabled { "true" } else { "false" },
        AtomicWriteContext::new(STORE)
            .durable_root(root)
            .channel_id(channel),
    )?;
    drop(write_guard);
    Ok(enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_footer_default_toggle_and_explicit_values_survive_disk_reads() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let first = path(root, 41);
        let second = path(root, 42);
        assert!(read(&first).unwrap());
        assert!(!update_at(root, 41, None).unwrap());
        assert!(!read(&first).unwrap());
        assert!(read(&second).unwrap());
        assert!(update_at(root, 41, None).unwrap());
        assert!(read(&first).unwrap());
        assert!(!update_at(root, 41, Some(false)).unwrap());
        assert!(!read(&first).unwrap());
        assert!(update_at(root, 41, Some(true)).unwrap());
        assert!(read(&first).unwrap());
        assert!(!second.exists());
    }

    #[test]
    fn usage_footer_two_concurrent_toggles_restore_the_original_value() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                update_at(root, 41, None).unwrap()
            });
            barrier.wait();
            let second = update_at(root, 41, None).unwrap();
            assert_ne!(first.join().unwrap(), second);
        });
        assert!(read(&path(root, 41)).unwrap());
    }
}
