use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::*;

static STORE_WRITE_LOCK: Mutex<()> = Mutex::new(());

struct ChannelFileLock {
    _file: fs::File,
}

impl Drop for ChannelFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn lock_channel_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> Result<ChannelFileLock, String> {
    let lock_path =
        provider_dir_in_root(root, provider, token_hash).join(format!("{channel_id}.orphans.lock"));
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(ChannelFileLock { _file: file })
}

pub(super) fn provider_dir_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
) -> PathBuf {
    root.join(provider.as_str()).join(token_hash)
}

pub(super) fn channel_file_path_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> PathBuf {
    provider_dir_in_root(root, provider, token_hash).join(format!("{channel_id}.json"))
}

fn channel_dir_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> PathBuf {
    provider_dir_in_root(root, provider, token_hash).join(channel_id.to_string())
}

pub(super) fn entry_path_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> PathBuf {
    channel_dir_in_root(root, provider, token_hash, channel_id).join(format!("{panel_msg_id}.json"))
}

pub(super) fn tombstone_path_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> PathBuf {
    channel_dir_in_root(root, provider, token_hash, channel_id)
        .join(format!("{panel_msg_id}.removed"))
}

fn load_legacy_channel_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> Result<Vec<StatusPanelOrphanEntry>, String> {
    let path = channel_file_path_in_root(root, provider, token_hash, channel_id);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    serde_json::from_str::<StatusPanelOrphanChannelFile>(&raw)
        .map(StatusPanelOrphanChannelFile::into_entries)
        .map_err(|error| error.to_string())
}

fn load_entry_file(path: &Path) -> Result<Option<StatusPanelOrphanEntry>, String> {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

pub(super) fn load_channel_result_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> Result<Vec<StatusPanelOrphanEntry>, String> {
    let mut entries: HashMap<u64, StatusPanelOrphanEntry> =
        load_legacy_channel_in_root(root, provider, token_hash, channel_id)?
            .into_iter()
            .map(|entry| (entry.id, entry))
            .collect();
    let dir = channel_dir_in_root(root, provider, token_hash, channel_id);
    let files = match fs::read_dir(&dir) {
        Ok(files) => files,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut entries: Vec<_> = entries.into_values().collect();
            entries.sort_by_key(|entry| entry.id);
            return Ok(entries);
        }
        Err(error) => return Err(error.to_string()),
    };
    let mut tombstones = HashSet::new();
    for file in files {
        let file = file.map_err(|error| error.to_string())?;
        let path = file.path();
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(id) = stem.parse::<u64>().ok() else {
            continue;
        };
        match path.extension().and_then(|value| value.to_str()) {
            Some("removed") => {
                tombstones.insert(id);
            }
            Some("json") => {
                let entry = load_entry_file(&path)?
                    .ok_or_else(|| "status panel orphan entry disappeared".to_string())?;
                if entry.id != id {
                    return Err("status panel orphan entry id/path mismatch".to_string());
                }
                entries.insert(id, entry);
            }
            _ => {}
        }
    }
    for id in tombstones {
        entries.remove(&id);
    }
    let mut entries: Vec<_> = entries.into_values().collect();
    entries.sort_by_key(|entry| entry.id);
    Ok(entries)
}

pub(super) fn load_channel_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> Vec<StatusPanelOrphanEntry> {
    load_channel_result_in_root(root, provider, token_hash, channel_id).unwrap_or_default()
}

pub(super) fn save_entry_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    entry: &StatusPanelOrphanEntry,
) -> Result<(), String> {
    let json = serde_json::to_string_pretty(entry).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(
        &entry_path_in_root(root, provider, token_hash, channel_id, entry.id),
        &json,
    )
}

fn upsert_entry_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    mut entry: StatusPanelOrphanEntry,
) -> Result<(), String> {
    if channel_id == 0 || entry.id == 0 {
        return Err("status panel orphan ids must be non-zero".to_string());
    }
    let _process_guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _channel_guard = lock_channel_in_root(root, provider, token_hash, channel_id)?;
    let _ = load_channel_result_in_root(root, provider, token_hash, channel_id)?;
    let path = entry_path_in_root(root, provider, token_hash, channel_id, entry.id);
    if let Some(existing) = load_entry_file(&path)? {
        match (existing.kind, entry.kind) {
            (_, StatusPanelOrphanKind::Stranded) => {
                entry = StatusPanelOrphanEntry::stranded(entry.id)
            }
            (StatusPanelOrphanKind::Stranded, StatusPanelOrphanKind::PendingBind) => {
                entry = existing
            }
            (StatusPanelOrphanKind::PendingBind, StatusPanelOrphanKind::PendingBind)
                if entry.turn_identity.is_none() =>
            {
                entry.turn_identity = existing.turn_identity;
            }
            _ => {}
        }
    }
    let tombstone = tombstone_path_in_root(root, provider, token_hash, channel_id, entry.id);
    match fs::remove_file(tombstone) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    save_entry_in_root(root, provider, token_hash, channel_id, &entry)
}

pub(super) fn enqueue_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> Result<(), String> {
    upsert_entry_in_root(
        root,
        provider,
        token_hash,
        channel_id,
        StatusPanelOrphanEntry::stranded(panel_msg_id),
    )
}

pub(super) fn enqueue_pending_bind_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
    turn_identity: Option<InflightTurnIdentity>,
) -> Result<(), String> {
    upsert_entry_in_root(
        root,
        provider,
        token_hash,
        channel_id,
        StatusPanelOrphanEntry::pending_bind(panel_msg_id, turn_identity),
    )
}

fn remove_in_root_checked_locked(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> Result<(), String> {
    let legacy_contains = load_legacy_channel_in_root(root, provider, token_hash, channel_id)?
        .iter()
        .any(|entry| entry.id == panel_msg_id);
    let _ = load_channel_result_in_root(root, provider, token_hash, channel_id)?;
    let path = entry_path_in_root(root, provider, token_hash, channel_id, panel_msg_id);
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let tombstone = tombstone_path_in_root(root, provider, token_hash, channel_id, panel_msg_id);
    if legacy_contains {
        runtime_store::atomic_write(&tombstone, "removed\n")
    } else {
        match fs::remove_file(tombstone) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

pub(super) fn remove_in_root_checked(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> Result<(), String> {
    let _process_guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _channel_guard = lock_channel_in_root(root, provider, token_hash, channel_id)?;
    remove_in_root_checked_locked(root, provider, token_hash, channel_id, panel_msg_id)
}

pub(super) fn remove_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let _ = remove_in_root_checked(root, provider, token_hash, channel_id, panel_msg_id);
}

pub(super) fn remove_pending_bind_in_root_checked(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> Result<(), String> {
    let _process_guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _channel_guard = lock_channel_in_root(root, provider, token_hash, channel_id)?;
    let entries = load_channel_result_in_root(root, provider, token_hash, channel_id)?;
    if entries
        .iter()
        .any(|entry| entry.id == panel_msg_id && entry.is_pending_bind())
    {
        remove_in_root_checked_locked(root, provider, token_hash, channel_id, panel_msg_id)?;
    }
    Ok(())
}

pub(super) fn remove_pending_bind_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let _ =
        remove_pending_bind_in_root_checked(root, provider, token_hash, channel_id, panel_msg_id);
}

pub(super) fn with_channel_lock<T>(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    operation: impl FnOnce() -> T,
) -> Result<T, String> {
    let _process_guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _channel_guard = lock_channel_in_root(root, provider, token_hash, channel_id)?;
    Ok(operation())
}
