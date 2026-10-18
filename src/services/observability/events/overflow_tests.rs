use super::{EventLog, StructuredEvent};
use std::sync::{Arc, Mutex};
use tracing::{Level, Subscriber};
use tracing_subscriber::{Layer, layer::SubscriberExt};

#[derive(Clone, Default)]
struct Levels(Arc<Mutex<Vec<Level>>>);

impl<S: Subscriber> Layer<S> for Levels {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target().ends_with("observability::events") {
            self.0.lock().unwrap().push(*event.metadata().level());
        }
    }
}

fn push(log: &EventLog, name: &str) {
    log.push(StructuredEvent::new(
        name,
        None,
        None,
        serde_json::json!({}),
    ));
}

#[test]
fn flushed_history_eviction_is_debug_and_retains_recent_events() {
    let levels = Levels::default();
    crate::logging::test_capture::pin_callsite_interest();
    tracing::subscriber::with_default(tracing_subscriber::registry().with(levels.clone()), || {
        let log = EventLog::new(2);
        push(&log, "first");
        push(&log, "second");
        let (batch, watermark) = log.drain_unflushed();
        assert_eq!(batch.len(), 2);
        assert_eq!(log.drain_unflushed().0.len(), 2);
        log.commit_flushed(watermark);
        for name in ["third", "fourth"] {
            push(&log, name);
        }
        assert_eq!(log.inner.lock().unwrap().dropped_total, 0);
        assert_eq!(
            log.recent(2)
                .iter()
                .map(|row| row.event_type.as_str())
                .collect::<Vec<_>>(),
            ["third", "fourth"]
        );
        let (batch, watermark) = log.drain_unflushed();
        assert_eq!(batch.len(), 2);
        log.commit_flushed(watermark);
        assert!(log.drain_unflushed().0.is_empty());
        assert_eq!(log.recent(2).len(), 2);
    });
    assert_eq!(*levels.0.lock().unwrap(), [Level::DEBUG, Level::DEBUG]);
}

#[test]
fn unflushed_loss_warns_once_until_persistence_makes_progress() {
    let levels = Levels::default();
    crate::logging::test_capture::pin_callsite_interest();
    tracing::subscriber::with_default(tracing_subscriber::registry().with(levels.clone()), || {
        let log = EventLog::new(2);
        for name in ["first", "second", "third", "fourth"] {
            push(&log, name);
        }
        let (_, watermark) = log.drain_unflushed();
        for name in ["fifth", "sixth"] {
            push(&log, name);
        }
        log.commit_flushed(watermark);
        log.commit_flushed(watermark - 1);
        assert_eq!(
            log.drain_unflushed()
                .0
                .iter()
                .map(|row| row.event_type.as_str())
                .collect::<Vec<_>>(),
            ["fifth", "sixth"]
        );
        push(&log, "seventh");
        let (_, recovery_watermark) = log.drain_unflushed();
        log.commit_flushed(recovery_watermark);
        push(&log, "eighth");
        push(&log, "ninth");
        push(&log, "tenth");
        log.commit_flushed(recovery_watermark);
        log.commit_flushed(watermark);
        push(&log, "eleventh");
        assert_eq!(log.inner.lock().unwrap().dropped_total, 7);
    });
    assert_eq!(
        *levels.0.lock().unwrap(),
        [
            Level::WARN,
            Level::DEBUG,
            Level::DEBUG,
            Level::DEBUG,
            Level::DEBUG,
            Level::DEBUG,
            Level::DEBUG,
            Level::WARN,
            Level::DEBUG
        ]
    );
}

#[test]
fn concurrent_overflow_at_default_capacity_warns_once() {
    let levels = Levels::default();
    crate::logging::test_capture::pin_callsite_interest();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(levels.clone()));
    let log = Arc::new(EventLog::new(super::MAX_EVENTS));
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let log = log.clone();
            let dispatch = dispatch.clone();
            scope.spawn(move || {
                tracing::dispatcher::with_default(&dispatch, || {
                    for index in 0..1600 {
                        push(&log, &format!("{worker}:{index}"));
                    }
                })
            });
        }
    });
    assert_eq!(log.recent(super::MAX_EVENTS).len(), super::MAX_EVENTS);
    assert_eq!(log.inner.lock().unwrap().dropped_total, 2800);
    let levels = levels.0.lock().unwrap();
    assert_eq!(
        levels.iter().filter(|level| **level == Level::WARN).count(),
        1
    );
    assert_eq!(
        levels
            .iter()
            .filter(|level| **level == Level::DEBUG)
            .count(),
        2799
    );
}
