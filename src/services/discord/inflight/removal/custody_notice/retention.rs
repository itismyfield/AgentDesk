//! Delivered custody is retired as a directory; uncertainty always keeps the evidence.

use super::Notice;
use crate::services::message_outbox::dedupe_key_for_message;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A stable sidecar serializes boot preservation and retirement without moving the lock inode.
pub(in super::super) fn lock(custody: &Path, blocking: bool) -> io::Result<File> {
    let parent = custody.parent().ok_or(io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(parent)?;
    directory(parent)?;
    match fs::create_dir(custody) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    directory(custody)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(custody.with_extension("lock"))?;
    if blocking {
        file.lock()?;
    } else {
        file.try_lock().map_err(io::Error::other)?;
    }
    Ok(file)
}

fn directory(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(io::ErrorKind::InvalidInput.into())
    }
}

/// Only the exact persistent notice's sent row proves delivery; NULL time cannot expire it.
pub(super) async fn terminal(
    pool: &PgPool,
    notice: &Notice,
) -> Result<Option<Option<DateTime<Utc>>>, sqlx::Error> {
    let key = dedupe_key_for_message(
        &notice.target,
        &notice.text,
        Some("boot_custody.notice"),
        Some(&notice.session),
    );
    sqlx::query_scalar(
        "SELECT sent_at FROM message_outbox
         WHERE dedupe_key = $1 AND status = 'sent' AND source = 'boot_custody_notice'
           AND target = $2 AND session_key = $3 AND reason_code = 'boot_custody.notice'",
    )
    .bind(key)
    .bind(&notice.target)
    .bind(&notice.session)
    .fetch_optional(pool)
    .await
}

/// The caller holds the provider lock; both delivery and every retained entry must be old.
pub(in super::super) fn remove_expired(
    dir: &Path,
    sent_at: SystemTime,
    now: SystemTime,
) -> io::Result<Option<u64>> {
    let Some(cutoff) = now.checked_sub(RETENTION) else {
        return Ok(None);
    };
    if sent_at > cutoff {
        return Ok(None);
    }
    directory(dir)?;
    let Some(bytes) = old_tree_bytes(dir, cutoff)? else {
        return Ok(None);
    };
    let retired = dir.with_file_name(format!(".retired-{}", uuid::Uuid::new_v4()));
    fs::rename(dir, &retired)?;
    fs::remove_dir_all(&retired)?;
    Ok(Some(bytes))
}

fn old_tree_bytes(path: &Path, cutoff: SystemTime) -> io::Result<Option<u64>> {
    let meta = fs::symlink_metadata(path)?;
    if meta.modified()? > cutoff {
        return Ok(None);
    }
    if meta.is_file() || meta.is_symlink() {
        return Ok(Some(meta.len()));
    }
    if !meta.is_dir() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut bytes = 0u64;
    for entry in fs::read_dir(path)? {
        let Some(size) = old_tree_bytes(&entry?.path(), cutoff)? else {
            return Ok(None);
        };
        bytes = bytes.checked_add(size).ok_or(io::ErrorKind::InvalidData)?;
    }
    Ok(Some(bytes))
}
