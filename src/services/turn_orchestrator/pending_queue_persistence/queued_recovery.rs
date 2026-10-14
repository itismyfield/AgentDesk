use super::{
    ChannelId, Instant, PendingQueueRecovery, ProviderKind, STALE_PENDING_QUEUE_TMP_AGE,
    SystemTime, cleanup_stale_pending_queue_tmp_files_in_dir, fs, parse_snapshot,
    pending_dispatch_marker_channel_id, pending_queue_items_to_interventions, pending_queue_root,
};
use std::io;

#[cfg(test)]
#[path = "queued_recovery_tests.rs"]
mod tests;

pub(crate) fn load_pending_queues_checked(
    provider: &ProviderKind,
    token_hash: &str,
) -> PendingQueueRecovery {
    let report = load_pending_queues_report(provider, token_hash);
    tracing::debug!(issues = ?report.issues, cleanup_observation_complete = report.cleanup_observation_complete,
        cleanup_audits = report.cleanup.len(), "pending queue bulk observation");
    report.payload
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum QueueIssue {
    RootUnavailable,
    ReadDir(String),
    Entry(String),
    Read(u64, String),
    Parse(u64, String),
    Cleanup(String),
}

pub(crate) struct QueueRecoveryReport {
    pub(crate) payload: PendingQueueRecovery,
    pub(crate) issues: Vec<QueueIssue>,
    pub(crate) cleanup: Vec<super::PendingQueueTmpCleanupAudit>,
    pub(crate) cleanup_observation_complete: bool,
}

pub(crate) fn load_pending_queues_report(
    provider: &ProviderKind,
    token_hash: &str,
) -> QueueRecoveryReport {
    load_pending_queues_report_at(pending_queue_root(), provider, token_hash)
}

fn load_pending_queues_report_at(
    root: Option<super::PathBuf>,
    provider: &ProviderKind,
    token_hash: &str,
) -> QueueRecoveryReport {
    let mut report = QueueRecoveryReport {
        payload: PendingQueueRecovery::default(),
        issues: Vec::new(),
        cleanup: Vec::new(),
        // The legacy cleanup discards directory, entry and metadata errors.
        cleanup_observation_complete: false,
    };
    let Some(root) = root else {
        report.issues.push(QueueIssue::RootUnavailable);
        #[cfg(test)]
        if tests::mutant("queue-root-empty") {
            report.issues.clear();
        }
        return report;
    };
    let recovery = &mut report.payload;
    let dir = root.join(provider.as_str()).join(token_hash);
    #[cfg(test)]
    tests::CLEANUPS.set(tests::CLEANUPS.get() + 1);
    report.cleanup = cleanup_stale_pending_queue_tmp_files_in_dir(
        provider,
        token_hash,
        &dir,
        SystemTime::now(),
        STALE_PENDING_QUEUE_TMP_AGE,
    );
    #[cfg(test)]
    if tests::mutant("queue-cleanup-twice") {
        tests::CLEANUPS.set(tests::CLEANUPS.get() + 1);
        let _ = cleanup_stale_pending_queue_tmp_files_in_dir(
            provider,
            token_hash,
            &dir,
            SystemTime::now(),
            STALE_PENDING_QUEUE_TMP_AGE,
        );
    }
    for audit in &report.cleanup {
        if let Some(error) = &audit.error {
            report.issues.push(QueueIssue::Cleanup(format!(
                "{}: {error}",
                audit.path.display()
            )));
        }
    }
    let entries = match read_queue_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return report,
        Err(error) => {
            report.issues.push(QueueIssue::ReadDir(error.to_string()));
            recovery
                .scan_errors
                .push(format!("read_dir {}: {error}", dir.display()));
            return report;
        }
    };
    let reference_instant = Instant::now();
    let reference_wall_time = SystemTime::now();
    for entry in entries {
        #[cfg(test)]
        let entry = scan_fault::take(&dir, scan_fault::Point::Entry).map_or(entry, Err);
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.issues.push(QueueIssue::Entry(error.to_string()));
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
        let items = match fs::read_to_string(&path) {
            Ok(content) => match parse_snapshot(&content) {
                Ok((items, _)) => items,
                Err(error) => {
                    report
                        .issues
                        .push(QueueIssue::Parse(channel_id, error.clone()));
                    recovery
                        .quarantined
                        .push((ChannelId::new(channel_id), error));
                    continue;
                }
            },
            Err(error) => {
                let error = format!("read {}: {error}", path.display());
                report
                    .issues
                    .push(QueueIssue::Read(channel_id, error.clone()));
                recovery
                    .quarantined
                    .push((ChannelId::new(channel_id), error));
                continue;
            }
        };
        if let Some(override_id) = items.iter().find_map(|item| item.override_channel_id) {
            recovery
                .overrides
                .insert(ChannelId::new(channel_id), ChannelId::new(override_id));
        }
        let interventions =
            pending_queue_items_to_interventions(items, reference_wall_time, reference_instant);
        if !interventions.is_empty() {
            recovery
                .queues
                .insert(ChannelId::new(channel_id), interventions);
        }
    }
    #[cfg(test)]
    if tests::mutant("queue-drop-errors") {
        report.issues.clear();
    }
    report
}

fn read_queue_dir(dir: &super::Path) -> io::Result<fs::ReadDir> {
    #[cfg(test)]
    tests::SCANS.set(tests::SCANS.get() + 1);
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

#[cfg(test)]
fn load_pending_dispatch_markers_report_at(
    root: Option<super::PathBuf>,
    provider: &ProviderKind,
    token_hash: &str,
) -> (Vec<super::PendingDispatchMarker>, Vec<QueueIssue>) {
    let mut markers = Vec::new();
    let mut issues = Vec::new();
    let Some(root) = root else {
        return (markers, vec![QueueIssue::RootUnavailable]);
    };
    let dir = root.join(provider.as_str()).join(token_hash);
    let entries = match read_queue_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (markers, issues),
        Err(error) => return (markers, vec![QueueIssue::ReadDir(error.to_string())]),
    };
    for entry in entries {
        let entry = scan_fault::take(&dir, scan_fault::Point::Entry).map_or(entry, Err);
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                issues.push(QueueIssue::Entry(error.to_string()));
                continue;
            }
        };
        let Some(channel) = pending_dispatch_marker_channel_id(&path) else {
            continue;
        };
        let channel_id = ChannelId::new(channel);
        match super::load_channel_pending_dispatch_marker_checked(provider, token_hash, channel_id)
        {
            Ok(Some((intervention, restored_override))) => {
                markers.push(super::PendingDispatchMarker {
                    channel_id,
                    intervention,
                    restored_override,
                })
            }
            Ok(None) => {}
            Err(error) if error.starts_with("parse dispatch marker ") => {
                issues.push(QueueIssue::Parse(channel, error))
            }
            Err(error) => issues.push(QueueIssue::Read(channel, error)),
        }
    }
    if tests::mutant("marker-drop-errors") {
        issues.clear();
    }
    if tests::mutant("marker-rescan") {
        let _ = read_queue_dir(&dir);
    }
    (markers, issues)
}

#[cfg(test)]
pub(crate) fn load_pending_dispatch_markers_report(
    provider: &ProviderKind,
    token_hash: &str,
) -> (Vec<super::PendingDispatchMarker>, Vec<QueueIssue>) {
    load_pending_dispatch_markers_report_at(pending_queue_root(), provider, token_hash)
}
