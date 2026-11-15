//! Restore retained snapshots without marking bodies delivered or recreating their active rows.

use super::*;
use std::collections::BTreeSet;

/// Missing active rows may have lost their closing wake in a crash; the queue still judges starts.
pub(in crate::services::discord) fn reconcile(provider: &ProviderKind) -> Vec<u64> {
    let Ok(directory) = directory(provider) else {
        return Vec::new();
    };
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(provider = provider.as_str(), %error, "body debt directory kept unread");
            return Vec::new();
        }
    };
    let mut closed = BTreeSet::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(provider = provider.as_str(), %error, "retained body debt entry unreadable");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let read = || {
            let record = store::read(&path, provider)?;
            store::resync(&path)?;
            Ok::<_, String>(record)
        };
        match read() {
            Ok(record) => {
                if matches!(
                    load_inflight_state_read_only_result(provider, record.channel_id),
                    Ok(None)
                ) {
                    closed.insert(record.channel_id);
                }
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "retained body debt needs a later read")
            }
        }
    }
    closed.into_iter().collect()
}
