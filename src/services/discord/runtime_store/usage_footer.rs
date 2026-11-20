use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use super::{AtomicWriteContext, agentdesk_root, critical_publish};

const STORE: &str = "discord_usage_footer";
static WRITE: Mutex<()> = Mutex::new(());

fn path(root: &Path, channel: u64) -> PathBuf {
    root.join("runtime")
        .join(STORE)
        .join(format!("{channel}.json"))
}

fn read(path: &Path) -> io::Result<Option<bool>> {
    match fs::read_to_string(path) {
        Ok(value) => serde_json::from_str(&value)
            .map(Some)
            .map_err(io::Error::from),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn current(saved: Option<bool>) -> bool {
    saved.unwrap_or(true)
}

pub(crate) fn enabled(channel: u64) -> bool {
    let saved = agentdesk_root().and_then(|root| {
        read(&path(&root, channel)).unwrap_or_else(|error| {
            tracing::warn!(channel, %error, "usage footer preference unreadable");
            None
        })
    });
    current(saved)
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
        None => !current(read(&target).map_err(|error| error.to_string())?),
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
    use crate::test_support::wait::{WAIT_CAP, join_within, recv_within};
    use std::sync::{Arc, mpsc};

    #[test]
    fn usage_footer_default_toggle_and_explicit_values_survive_disk_reads() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let first = path(root, 41);
        let second = path(root, 42);
        assert!(current(read(&first).unwrap()));
        assert!(!update_at(root, 41, None).unwrap());
        assert_eq!(read(&first).unwrap(), Some(false));
        assert!(current(read(&second).unwrap()));
        assert!(update_at(root, 41, None).unwrap());
        assert_eq!(read(&first).unwrap(), Some(true));
        assert!(!update_at(root, 41, Some(false)).unwrap());
        assert_eq!(read(&first).unwrap(), Some(false));
        assert!(update_at(root, 41, Some(true)).unwrap());
        assert_eq!(read(&first).unwrap(), Some(true));
        assert!(!second.exists());
    }

    #[test]
    fn usage_footer_two_concurrent_toggles_restore_the_original_value() {
        let root = Arc::new(tempfile::tempdir().unwrap());
        let spawn_toggle = || {
            let root = Arc::clone(&root);
            let (start, ready) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                recv_within(&ready, WAIT_CAP);
                update_at(root.path(), 41, None).unwrap()
            });
            (start, worker)
        };
        let (start_first, first) = spawn_toggle();
        let (start_second, second) = spawn_toggle();
        start_first.send(()).unwrap();
        start_second.send(()).unwrap();
        assert_ne!(join_within(first, WAIT_CAP), join_within(second, WAIT_CAP));
        assert_eq!(read(&path(root.path(), 41)).unwrap(), Some(true));
    }
}
