use super::*;
use std::io::Write;
use std::sync::{Arc, Mutex};

const TOKEN: &str = "s2b-scan-errors";

fn namespace(provider: &ProviderKind) -> std::path::PathBuf {
    pending_queue_root()
        .unwrap()
        .join(provider.as_str())
        .join(TOKEN)
}

fn seed(dir: &super::super::Path, channel: u64) -> (std::path::PathBuf, Vec<u8>) {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{channel}.json"));
    let bytes = serde_json::to_vec(&serde_json::json!([{
        "author_id": 7,
        "message_id": channel + 100,
        "queued_generation": 7,
        "text": "queued input",
        "channel_id": channel
    }]))
    .unwrap();
    fs::write(&path, &bytes).unwrap();
    (path, bytes)
}

#[derive(Clone)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn permission_denied_is_returned_and_logged_without_changing_the_queue() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let dir = namespace(&provider);
    let (path, bytes) = seed(&dir, 6_813_401);
    let _fault = scan_fault::arm(
        &dir,
        scan_fault::Point::ReadDir,
        io::ErrorKind::PermissionDenied,
    );
    let loaded = load_pending_queues_checked(&provider, TOKEN);
    assert!(loaded.queues.is_empty() && loaded.quarantined.is_empty());
    assert_eq!(loaded.scan_errors.len(), 1);
    assert!(loaded.scan_errors[0].contains("read_dir"));
    assert_eq!(fs::read(&path).unwrap(), bytes);

    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = CapturedLogs(logs.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let _fault = scan_fault::arm(
        &dir,
        scan_fault::Point::ReadDir,
        io::ErrorKind::PermissionDenied,
    );
    tracing::subscriber::with_default(subscriber, || {
        let (queues, overrides) = super::super::load_pending_queues(&provider, TOKEN);
        assert!(queues.is_empty() && overrides.is_empty());
    });
    let captured = String::from_utf8(
        logs.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
    )
    .unwrap();
    let summary: Vec<_> = captured
        .lines()
        .filter(|line| line.contains("pending queue recovery scan errors"))
        .collect();
    assert_eq!(summary.len(), 1, "{captured}");
    assert!(summary[0].contains("scan_errors=1"), "{captured}");
    assert!(
        captured.contains("read_dir") && captured.contains("injected pending queue scan error"),
        "{captured}"
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn an_entry_error_is_returned_while_other_channels_still_restore() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let dir = namespace(&provider);
    let originals = [seed(&dir, 6_813_411), seed(&dir, 6_813_412)];
    let _fault = scan_fault::arm(
        &dir,
        scan_fault::Point::Entry,
        io::ErrorKind::PermissionDenied,
    );
    let loaded = load_pending_queues_checked(&provider, TOKEN);
    assert_eq!(loaded.queues.len(), 1);
    assert!(loaded.quarantined.is_empty());
    assert_eq!(loaded.scan_errors.len(), 1);
    assert!(loaded.scan_errors[0].contains("scan"));
    for (path, bytes) in originals {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn a_missing_namespace_is_empty_without_a_scan_error() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let dir = namespace(&provider);
    assert!(!dir.exists());
    let loaded = load_pending_queues_checked(&provider, TOKEN);
    assert!(loaded.queues.is_empty() && loaded.overrides.is_empty());
    assert!(loaded.quarantined.is_empty() && loaded.scan_errors.is_empty());
    assert!(!dir.exists());
}

#[test]
fn r0l_queue_root_empty_partial_and_cleanup_are_distinct() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let missing = load_pending_queues_report_at(None, &provider, TOKEN);
    assert_eq!(missing.issues, vec![QueueIssue::RootUnavailable]);
    let empty = load_pending_queues_report(&provider, TOKEN);
    assert!(empty.issues.is_empty());
    assert!(!empty.cleanup_observation_complete);
    let dir = namespace(&provider);
    let (path, bytes) = seed(&dir, 632_501);
    fs::write(dir.join("632502.json"), b"{broken").unwrap();
    fs::create_dir(dir.join("632503.json")).unwrap();
    let tmp = dir.join(".632501.json.tmp");
    fs::write(&tmp, b"stale").unwrap();
    filetime::set_file_mtime(&tmp, filetime::FileTime::from_unix_time(1, 0)).unwrap();
    CLEANUPS.set(0);
    let report = load_pending_queues_report(&provider, TOKEN);
    assert_eq!(CLEANUPS.get(), 1, "one legacy cleanup call");
    assert_eq!(report.payload.queues.len(), 1);
    assert_eq!(report.payload.quarantined.len(), 2);
    assert!(
        report
            .issues
            .iter()
            .any(|issue| matches!(issue, QueueIssue::Read(632503, _)))
    );
    assert!(
        report
            .issues
            .iter()
            .any(|issue| matches!(issue, QueueIssue::Parse(632502, _)))
    );
    assert_eq!(report.cleanup.len(), 1);
    assert_eq!(report.cleanup[0].action, "removed_stale");
    assert!(!report.cleanup_observation_complete);
    assert!(!tmp.exists());
    assert_eq!(fs::read(path).unwrap(), bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let failed_tmp = dir.join(".632502.json.tmp");
        fs::write(&failed_tmp, b"stale").unwrap();
        filetime::set_file_mtime(&failed_tmp, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        let permissions = fs::metadata(&dir).unwrap().permissions();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let cleanup = load_pending_queues_report(&provider, TOKEN);
        fs::set_permissions(&dir, permissions).unwrap();
        assert!(
            cleanup
                .issues
                .iter()
                .any(|issue| matches!(issue, QueueIssue::Cleanup(_)))
        );
        assert_eq!(cleanup.cleanup[0].action, "remove_failed");
        assert!(failed_tmp.exists());
    }
    let _fault = scan_fault::arm(
        &dir,
        scan_fault::Point::ReadDir,
        io::ErrorKind::PermissionDenied,
    );
    let report = load_pending_queues_report(&provider, TOKEN);
    assert!(matches!(report.issues.as_slice(), [QueueIssue::ReadDir(_)]));
    let _fault = scan_fault::arm(&dir, scan_fault::Point::Entry, io::ErrorKind::Other);
    let report = load_pending_queues_report(&provider, TOKEN);
    assert!(
        report
            .issues
            .iter()
            .any(|issue| matches!(issue, QueueIssue::Entry(_)))
    );
}

#[test]
fn r0l_marker_bulk_preserves_valid_rows_and_channel_errors() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    assert_eq!(
        load_pending_dispatch_markers_report_at(None, &provider, TOKEN).1,
        vec![QueueIssue::RootUnavailable]
    );
    assert!(
        load_pending_dispatch_markers_report(&provider, TOKEN)
            .1
            .is_empty()
    );
    let dir = namespace(&provider);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("632511.dispatch"),
        br#"{"author_id":7,"message_id":9,"text":"old marker","override_channel_id":31}"#,
    )
    .unwrap();
    fs::write(dir.join("632512.dispatch"), b"bad").unwrap();
    fs::create_dir(dir.join("632513.dispatch")).unwrap();
    SCANS.set(0);
    let (rows, errors) = load_pending_dispatch_markers_report(&provider, TOKEN);
    assert_eq!(SCANS.get(), 1, "one marker scan");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].channel_id.get(), 632511);
    assert_eq!(rows[0].intervention.text, "old marker");
    assert_eq!(rows[0].restored_override.unwrap().get(), 31);
    assert!(
        errors
            .iter()
            .any(|issue| matches!(issue, QueueIssue::Parse(632512, _)))
    );
    assert!(
        errors
            .iter()
            .any(|issue| matches!(issue, QueueIssue::Read(632513, _)))
    );
    let _fault = scan_fault::arm(
        &dir,
        scan_fault::Point::ReadDir,
        io::ErrorKind::PermissionDenied,
    );
    assert!(matches!(
        load_pending_dispatch_markers_report(&provider, TOKEN)
            .1
            .as_slice(),
        [QueueIssue::ReadDir(_)]
    ));
    let _fault = scan_fault::arm(&dir, scan_fault::Point::Entry, io::ErrorKind::Other);
    assert!(
        load_pending_dispatch_markers_report(&provider, TOKEN)
            .1
            .iter()
            .any(|issue| matches!(issue, QueueIssue::Entry(_)))
    );
}

thread_local! {
    pub(super) static SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static CLEANUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
pub(super) fn mutant(name: &str) -> bool {
    std::env::var("ADK_R0L_MUTANT").ok().as_deref() == Some(name)
}
