use super::*;
use std::io::Write;

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    fn count(&self, message: &str) -> usize {
        self.text()
            .lines()
            .filter(|line| line.contains(message))
            .count()
    }

    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        crate::logging::test_capture::pin_callsite_interest();
        let writer = self.clone();
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || writer.clone())
                .finish(),
        )
    }
}

const DEFERRED: &str = "live_original_bridge_deferred";

#[tokio::test(start_paused = true)]
async fn deferred_log_flood_is_bounded_and_changed_keys_emit_immediately() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 799_300_001;
    let token = Arc::new(CancelToken::new());
    let original = register_original(&ProviderKind::Codex, channel, &token)
        .await
        .unwrap();
    let logs = Logs::default();
    let _capture = logs.capture();
    for _ in 0..120 {
        assert!(try_recovery(&ProviderKind::Codex, channel).is_err());
        tokio::time::advance(Duration::from_millis(500)).await;
    }
    assert_eq!(logs.count(DEFERRED), 1, "{}", logs.text());
    assert!(logs.text().contains("INFO"));
    let other = register_original(&ProviderKind::Codex, channel + 1, &token)
        .await
        .unwrap();
    assert!(try_recovery(&ProviderKind::Codex, channel + 1).is_err());
    let claude = register_original(&ProviderKind::Claude, channel, &token)
        .await
        .unwrap();
    assert!(try_respawn_recovery(&ProviderKind::Claude, channel).is_err());
    assert_eq!(logs.count(DEFERRED), 3, "{}", logs.text());
    drop((original, other, claude));
    assert!(try_recovery(&ProviderKind::Codex, channel).is_ok());
    let again = register_original(&ProviderKind::Codex, channel, &token)
        .await
        .unwrap();
    assert!(try_recovery(&ProviderKind::Codex, channel).is_err());
    assert_eq!(logs.count(DEFERRED), 4, "{}", logs.text());
    drop(again);
}

#[tokio::test(start_paused = true)]
async fn deferred_summaries_cover_exact_five_minute_boundaries_and_reset_counts() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 799_300_003;
    let token = Arc::new(CancelToken::new());
    let original = register_original(&ProviderKind::Codex, channel, &token)
        .await
        .unwrap();
    let logs = Logs::default();
    let _capture = logs.capture();
    for _ in 0..120 {
        assert!(try_recovery(&ProviderKind::Codex, channel).is_err());
        tokio::time::advance(Duration::from_millis(500)).await;
    }
    tokio::time::advance(Duration::from_millis(239_500)).await;
    assert!(try_recovery(&ProviderKind::Codex, channel).is_err());
    assert_eq!(logs.count(DEFERRED), 1, "{}", logs.text());
    tokio::time::advance(Duration::from_millis(500)).await;
    assert!(try_recovery(&ProviderKind::Codex, channel).is_err());
    assert_eq!(logs.count(DEFERRED), 2, "{}", logs.text());
    assert!(
        logs.text()
            .contains("suppressed 120 times since previous log"),
        "{}",
        logs.text()
    );
    assert!(try_recovery(&ProviderKind::Codex, channel).is_err());
    tokio::time::advance(Duration::from_secs(300)).await;
    assert!(try_recovery(&ProviderKind::Codex, channel).is_err());
    assert_eq!(logs.count(DEFERRED), 3, "{}", logs.text());
    assert!(
        logs.text()
            .contains("suppressed 1 times since previous log"),
        "{}",
        logs.text()
    );
    drop(original);
    assert_eq!(
        logs.count("live_original_bridge_deferral_cleared"),
        1,
        "{}",
        logs.text()
    );
}

#[tokio::test]
async fn concurrent_deferred_observers_emit_one_first_line_without_changing_results() {
    use crate::test_support::wait::{WAIT_CAP, join_within};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 799_300_005;
    let token = Arc::new(CancelToken::new());
    let original = register_original(&ProviderKind::Codex, channel, &token)
        .await
        .unwrap();
    let logs = Logs::default();
    let threads = (0..16)
        .map(|_| {
            let logs = logs.clone();
            std::thread::spawn(move || {
                let _capture = logs.capture();
                for _ in 0..120 {
                    assert!(try_recovery(&ProviderKind::Codex, channel).is_err());
                }
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        join_within(thread, WAIT_CAP);
    }
    assert_eq!(logs.count(DEFERRED), 1, "{}", logs.text());
    drop(original);
    assert!(try_recovery(&ProviderKind::Codex, channel).is_ok());
}
