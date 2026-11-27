//! Delivered custody is retired as a directory; uncertainty always keeps the evidence.

use super::Notice;
use crate::services::discord::{ProviderKind, inflight, tui_direct_pending_start};
use crate::services::maintenance::runtime_retention;
use crate::services::message_outbox::dedupe_key_for_message;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;
use std::time::SystemTime;

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

#[derive(Default)]
pub(super) struct Retired {
    episodes: usize,
    bytes: u64,
}

impl Retired {
    pub(super) fn expire(
        &mut self,
        dir: &Path,
        provider: &ProviderKind,
        episode: &Value,
        sent_at: Option<SystemTime>,
        now: SystemTime,
    ) {
        let result = remove_expired_if(dir, sent_at, now, || !live_episode(provider, episode));
        match result {
            Ok(Some(bytes)) => {
                self.episodes += 1;
                self.bytes += bytes;
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(dir = %dir.display(), %error, "custody retention failed"),
        }
    }

    pub(super) fn log(&self, provider: &ProviderKind) {
        if self.episodes != 0 {
            tracing::info!(
                provider = provider.as_str(),
                removed = self.episodes,
                bytes = self.bytes,
                "custody retention removed"
            );
        }
    }
}

fn live_episode(provider: &ProviderKind, episode: &Value) -> bool {
    let Some(channel) = episode["channel_id"].as_u64().filter(|id| *id != 0) else {
        return true;
    };
    let anchor = episode["anchor_id"].as_u64();
    let row = inflight::load_inflight_state_read_only_result(provider, channel);
    #[cfg(test)]
    super::super::custody_notice_tests::between_owner_probes();
    match row {
        Err(_) => return true,
        Ok(Some(row)) => {
            let same_episode = super::super::boot_custody::row_episode_key(provider, &row);
            if same_episode == *episode
                || anchor.is_none_or(|anchor| anchor == row.user_msg_id && anchor == 0)
                || (anchor == Some(row.user_msg_id) && episode["anchorless"].is_null())
            {
                return true;
            }
        }
        Ok(None) => {}
    }
    tui_direct_pending_start::load_all().iter().any(|record| {
        record.provider == provider.as_str()
            && record.channel_id == channel
            && anchor.is_none_or(|anchor| record.anchor_message_id == anchor)
    })
}

#[cfg(test)]
pub(in super::super) fn remove_expired(
    dir: &Path,
    sent_at: SystemTime,
    now: SystemTime,
) -> io::Result<Option<u64>> {
    remove_expired_if(dir, Some(sent_at), now, || true)
}

// The provider lock keeps boot preservation serialized with retirement.
fn remove_expired_if(
    dir: &Path,
    sent_at: Option<SystemTime>,
    now: SystemTime,
    inactive: impl FnOnce() -> bool,
) -> io::Result<Option<u64>> {
    let Some(cutoff) = runtime_retention::cutoff(now) else {
        return Ok(None);
    };
    if sent_at.is_some_and(|sent_at| !runtime_retention::expired(sent_at, cutoff)) {
        return Ok(None);
    }
    directory(dir)?;
    let Some(bytes) = old_tree_bytes(dir, cutoff)? else {
        return Ok(None);
    };
    if !inactive() {
        return Ok(None);
    }
    let retired = dir.with_file_name(format!(".retired-{}", uuid::Uuid::new_v4()));
    fs::rename(dir, &retired)?;
    fs::remove_dir_all(&retired)?;
    Ok(Some(bytes))
}

fn old_tree_bytes(path: &Path, cutoff: SystemTime) -> io::Result<Option<u64>> {
    let meta = fs::symlink_metadata(path)?;
    if !runtime_retention::expired(meta.modified()?, cutoff) {
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
