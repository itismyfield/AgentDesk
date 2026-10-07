use super::*;
use std::{
    io::Write,
    sync::{Arc, Mutex},
};

#[test]
fn verified_tail_hold_diagnostic_is_bounded_to_one_warning_per_minute() {
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let sink = Sink::default();
    let writer = sink.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    tracing::subscriber::with_default(subscriber, || {
        let mut polling = HoldPoll::new(super::super::canary::CANARY_TMUX);
        let start = Instant::now();
        for second in [0, 1, 2, 3, 59] {
            polling.diagnose(
                Some("permission_unknown"),
                start + Duration::from_secs(second),
            );
        }
        let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            logs.matches("Codex verified output is held").count(),
            1,
            "{logs}"
        );
        assert!(
            logs.contains("WARN") && logs.contains("permission_unknown"),
            "{logs}"
        );
        polling.diagnose(
            Some("verified_unavailable"),
            start + Duration::from_secs(60),
        );
        let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            logs.matches("Codex verified output is held").count(),
            2,
            "{logs}"
        );
        assert!(logs.contains("verified_unavailable"), "{logs}");
    });
}
