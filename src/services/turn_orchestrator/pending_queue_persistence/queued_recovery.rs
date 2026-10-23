use super::super::queued_head::compat::{self, SnapshotRead};
use super::{
    ChannelId, Instant, PendingQueueRecovery, ProviderKind, STALE_PENDING_QUEUE_TMP_AGE,
    SystemTime, cleanup_stale_pending_queue_tmp_files_in_dir, fs,
    pending_dispatch_marker_channel_id, pending_queue_items_to_interventions, pending_queue_root,
};
use std::collections::HashMap;
use std::io;

#[cfg(test)]
#[path = "queued_recovery_tests.rs"]
mod tests;

#[derive(Default)]
pub(crate) struct SnapshotCatalog {
    pub(crate) snapshots: HashMap<ChannelId, SnapshotRead>,
    pub(crate) quarantined: Vec<(ChannelId, String)>,
    pub(crate) scan_errors: Vec<String>,
}

pub(crate) fn load_pending_queues_checked(
    provider: &ProviderKind,
    token_hash: &str,
) -> PendingQueueRecovery {
    let catalog = read_catalog(provider, token_hash);
    let mut recovery = PendingQueueRecovery {
        quarantined: catalog.quarantined,
        scan_errors: catalog.scan_errors,
        ..Default::default()
    };
    let reference_instant = Instant::now();
    let reference_wall_time = SystemTime::now();
    for (channel, snapshot) in catalog.snapshots {
        let items = match snapshot {
            SnapshotRead::Legacy(items, meta) => {
                if items.is_empty() && meta.as_ref().is_some_and(super::SnapshotMeta::is_set) {
                    tracing::debug!(%channel, "metadata-only queue remains in recovery catalog");
                }
                items
            }
            SnapshotRead::ReadOnlyV2(snapshot) => {
                recovery.quarantined.push((
                    channel,
                    format!(
                        "queue V2 is read-only: {} retained inputs",
                        snapshot.retained_inputs.len()
                    ),
                ));
                continue;
            }
        };
        if let Some(override_id) = items.iter().find_map(|item| item.override_channel_id) {
            recovery
                .overrides
                .insert(channel, ChannelId::new(override_id));
        }
        let interventions =
            pending_queue_items_to_interventions(items, reference_wall_time, reference_instant);
        if !interventions.is_empty() {
            recovery.queues.insert(channel, interventions);
        }
    }
    recovery
}

pub(crate) fn read_catalog(provider: &ProviderKind, token_hash: &str) -> SnapshotCatalog {
    let mut recovery = SnapshotCatalog::default();
    let Some(root) = pending_queue_root() else {
        return recovery;
    };
    let dir = root.join(provider.as_str()).join(token_hash);
    let _ = cleanup_stale_pending_queue_tmp_files_in_dir(
        provider,
        token_hash,
        &dir,
        SystemTime::now(),
        STALE_PENDING_QUEUE_TMP_AGE,
    );
    let entries = match read_queue_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return recovery,
        Err(error) => {
            recovery
                .scan_errors
                .push(format!("read_dir {}: {error}", dir.display()));
            return recovery;
        }
    };
    for entry in entries {
        #[cfg(test)]
        let entry = scan_fault::take(&dir, scan_fault::Point::Entry).map_or(entry, Err);
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                recovery
                    .scan_errors
                    .push(format!("scan {}: {error}", dir.display()));
                continue;
            }
        };
        let path = entry.path();
        if pending_dispatch_marker_channel_id(&path).is_some() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let channel_id: u64 = match path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse().ok())
        {
            Some(id) => id,
            None => continue,
        };
        let items = match fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))
            .and_then(|content| compat::inspect(&content))
        {
            Ok(items) => items,
            Err(error) => {
                recovery
                    .quarantined
                    .push((ChannelId::new(channel_id), error));
                continue;
            }
        };
        recovery.snapshots.insert(ChannelId::new(channel_id), items);
    }
    recovery
}

fn read_queue_dir(dir: &super::Path) -> io::Result<fs::ReadDir> {
    #[cfg(test)]
    if let Some(error) = scan_fault::take(dir, scan_fault::Point::ReadDir) {
        return Err(error);
    }
    fs::read_dir(dir)
}

pub(super) fn report_scan_errors(provider: &ProviderKind, errors: &[String]) {
    if errors.is_empty() {
        return;
    }
    tracing::warn!(
        provider = provider.as_str(),
        scan_errors = errors.len(),
        "pending queue recovery scan errors"
    );
    for error in errors {
        tracing::warn!(provider = provider.as_str(), %error, "pending queue recovery scan error");
    }
}

#[cfg(test)]
mod scan_fault {
    use std::cell::RefCell;
    use std::io;
    use std::path::{Path, PathBuf};

    #[derive(Clone, Copy, PartialEq)]
    pub(super) enum Point {
        ReadDir,
        Entry,
    }

    type Fault = (PathBuf, Point, io::ErrorKind);
    thread_local! {
        static ARMED: RefCell<Option<Fault>> = const { RefCell::new(None) };
    }

    pub(super) struct Guard(Option<Fault>);

    pub(super) fn arm(dir: &Path, point: Point, kind: io::ErrorKind) -> Guard {
        Guard(ARMED.with(|armed| armed.replace(Some((dir.to_owned(), point, kind)))))
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            ARMED.with(|armed| armed.replace(self.0.take()));
        }
    }

    pub(super) fn take(dir: &Path, point: Point) -> Option<io::Error> {
        ARMED.with(|armed| {
            let mut armed = armed.borrow_mut();
            if !armed
                .as_ref()
                .is_some_and(|(path, stage, _)| path == dir && *stage == point)
            {
                return None;
            }
            let (_, _, kind) = armed.take().expect("matching scan fault is armed");
            Some(io::Error::new(kind, "injected pending queue scan error"))
        })
    }
}
