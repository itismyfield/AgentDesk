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
