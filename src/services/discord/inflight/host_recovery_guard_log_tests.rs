use super::*;

#[derive(Clone, Default)]
pub(super) struct WarningSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for WarningSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl WarningSink {
    pub(super) fn count(&self) -> usize {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|line| {
                line.contains("WARN") && line.contains("host guard refused the keyed teardown")
            })
            .count()
    }
    pub(super) fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + 'static {
        let writer = self.clone();
        crate::logging::test_capture::pin_callsite_interest();
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish()
    }
}

#[test]
fn keyed_guard_warnings_follow_state_changes_and_keep_first_per_target() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let sink = WarningSink::default();
    let dispatch = tracing::Dispatch::new(sink.subscriber());
    let run = |lookup, channel, caller, key, name| {
        tracing::dispatcher::with_default(&dispatch, || {
            teardown_for_lookup(
                lookup,
                &ProviderKind::Claude,
                channel,
                key,
                name,
                None,
                caller,
            )
        })
    };
    let missing = |channel, caller, key, name| {
        assert!(matches!(
            run(HostedLookup::Missing, channel, caller, key, name),
            KeyedTeardown::RowMissing
        ));
    };
    for _ in 0..16 {
        missing(7_263, "retry", Some("log-key"), "log-target");
    }
    assert_eq!(sink.count(), 1, "same guard state must emit one WARN");
    assert!(matches!(
        run(
            HostedLookup::Unknown("lookup unavailable".into()),
            7_263,
            "retry",
            Some("log-key"),
            "log-target"
        ),
        KeyedTeardown::Kept
    ));
    assert_eq!(sink.count(), 2, "changed guard state must emit a WARN");
    missing(7_263, "retry", Some("log-key"), "log-target");
    assert_eq!(sink.count(), 3, "returning guard state must warn again");
    missing(7_263, "startup", Some("log-key"), "log-target");
    missing(7_264, "retry", Some("log-key"), "log-target");
    missing(7_263, "retry", Some("other-key"), "log-target");
    missing(0, "routine", None, "routine-one");
    missing(0, "routine", None, "routine-two");
    assert_eq!(
        sink.count(),
        8,
        "each recovery scope retains its first warning"
    );
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let dispatch = &dispatch;
            scope.spawn(move || {
                tracing::dispatcher::with_default(dispatch, || {
                    for _ in 0..16 {
                        assert!(matches!(
                            teardown_for_lookup(
                                HostedLookup::Missing,
                                &ProviderKind::Claude,
                                7_265,
                                Some("concurrent"),
                                "log-concurrent",
                                None,
                                "retry"
                            ),
                            KeyedTeardown::RowMissing
                        ));
                    }
                })
            });
        }
    });
    assert_eq!(sink.count(), 9, "concurrent repeats must emit one WARN");
}
