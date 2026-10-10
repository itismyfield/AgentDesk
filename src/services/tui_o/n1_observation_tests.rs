use super::*;
use std::cell::RefCell;
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::Duration;

thread_local! {
    static LOCAL: RefCell<Option<Option<Arc<Observer>>>> = const { RefCell::new(None) };
    static LOSE_AT_CUT: RefCell<bool> = const { RefCell::new(false) };
}

pub(super) fn snapshot_cut(observer: &Observer) {
    if LOSE_AT_CUT.with(|s| s.replace(false)) {
        observer.lost();
    }
}

pub(crate) fn faulted<T>(failure: &str, work: impl FnOnce(Arc<Observer>) -> T) -> T {
    let (observer, rx) = fixture(0);
    let _rx = if failure == "closed" { None } else { Some(rx) };
    if failure == "sink" {
        observer.sink_errors.fetch_add(1, Ordering::SeqCst);
    }
    let _guard =
        (failure == "lock").then(|| observer.channels.lock().unwrap_or_else(|e| e.into_inner()));
    scoped(Some(observer.clone()), || work(observer.clone()))
}

pub(crate) fn local_observer() -> Option<Option<Arc<Observer>>> {
    LOCAL.with(|slot| slot.borrow().clone())
}

pub(crate) fn scoped<T>(observer: Option<Arc<Observer>>, work: impl FnOnce() -> T) -> T {
    struct Reset(Option<Option<Arc<Observer>>>);
    impl Drop for Reset {
        fn drop(&mut self) {
            LOCAL.with(|s| *s.borrow_mut() = self.0.take());
        }
    }
    let _reset = Reset(LOCAL.with(|s| s.replace(Some(observer))));
    work()
}

pub(crate) fn fixture(capacity: usize) -> (Arc<Observer>, Receiver<Event>) {
    let (tx, rx) = sync_channel(capacity);
    (Arc::new(Observer::new(tx)), rx)
}

pub(crate) fn snapshot_value(observer: &Observer, channel: u64) -> serde_json::Value {
    serde_json::to_value(observer.snapshot(channel)).unwrap()
}

fn context() -> Context<'static> {
    Context {
        provider: "codex",
        origin: "tui_direct_synthetic",
        input_message_id: None,
    }
}

fn json(observer: &Observer) -> serde_json::Value {
    serde_json::to_value(observer.snapshot(7)).unwrap()
}

#[test]
fn n1_attempt_precedes_effect_and_error_remains_uncertain() {
    let (observer, rx) = fixture(16);
    scoped(Some(observer.clone()), || {
        let attempt = placeholder_attempt(context(), 7, "post_placeholder", Some((7, 8)), None);
        let before = rx.try_recv().unwrap();
        assert_eq!(json(&observer)["open_attempts"], 1);
        let created_message = 99; // Discord may have accepted the operation before returning Err.
        assert_eq!(created_message, 99);
        placeholder_result(attempt, Err("timeout_or_uncertain"));
        let after = rx.try_recv().unwrap();
        let a = serde_json::to_value(before).unwrap();
        let b = serde_json::to_value(after).unwrap();
        assert_eq!(a["op_id"], b["op_id"]);
        assert_eq!(a["reference"], serde_json::json!([7, 8]));
        assert_eq!(b["phase"], "failed_or_uncertain");
        assert_eq!(json(&observer)["counters"]["failed_or_uncertain"], 1);
        assert_eq!(json(&observer)["open_attempts"], 0);
    });
}

#[test]
fn n1_unterminated_and_patch_attempts_preserve_target_and_origin() {
    let (observer, rx) = fixture(16);
    scoped(Some(observer.clone()), || {
        drop(placeholder_attempt(
            context(),
            7,
            "post_placeholder",
            None,
            None,
        ));
        let patch = placeholder_attempt(context(), 7, "patch_placeholder", None, Some(99));
        placeholder_result(patch, Ok(99));
        let events: Vec<_> = rx
            .try_iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1]["target_message_id"], 99);
        assert_eq!(events[2]["message_id"], 99);
        assert_eq!(events[1]["origin"], "tui_direct_synthetic");
        assert_eq!(json(&observer)["open_attempts"], 1);
        assert_eq!(json(&observer)["counters"]["succeeded_patch"], 1);
    });
}

#[test]
fn n1_unavailable_never_becomes_zero_and_loss_never_disappears() {
    let (observer, _rx) = fixture(1);
    assert_eq!(
        serde_json::to_value(observer.snapshot(7)).unwrap(),
        serde_json::json!({"status":"unavailable"})
    );
    observer.emit(
        "codex",
        7,
        Kind::ModeConfirmed {
            confirmation_window_start: None,
            actual_confirmed_at_us: None,
            boundary_rule: "unknown",
        },
    );
    let guard = observer.channels.lock().unwrap_or_else(|e| e.into_inner());
    assert!(matches!(observer.snapshot(7), Snapshot::Unavailable));
    observer.emit(
        "codex",
        7,
        Kind::ModeConfirmed {
            confirmation_window_start: None,
            actual_confirmed_at_us: None,
            boundary_rule: "unknown",
        },
    );
    drop(guard);
    assert_eq!(json(&observer)["dropped_events"], 1);
    assert_eq!(json(&observer)["observer_unhealthy"], true);
    assert_eq!(json(&observer)["open_attempts_complete"], false);
    assert_eq!(json(&observer)["seq_high_water"], 1);
    LOSE_AT_CUT.with(|s| *s.borrow_mut() = true);
    assert!(matches!(observer.snapshot(7), Snapshot::Unavailable));
    assert_eq!(json(&observer)["dropped_events"], 2);
}

#[test]
fn n1_closed_only_preserves_physical_and_logical_keys_across_readers() {
    use crate::services::tui_o::writer::input_facts::InputFacts;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("parent.jsonl");
    let records = [
        serde_json::json!({"type":"system","subtype":"turn_duration"}),
        serde_json::json!({"type":"user","uuid":"native-a","message":{"content":"private text"}}),
        serde_json::json!({"type":"system","subtype":"turn_duration"}),
        serde_json::json!({"type":"system","subtype":"turn_duration"}),
    ];
    let source = records.iter().map(|v| format!("{v}\n")).collect::<String>();
    std::fs::write(&path, &source).unwrap();
    let (dev, ino) =
        super::super::shadow::capture::file_identity(&std::fs::metadata(&path).unwrap());
    let binding = SourceBinding {
        channel_id: 7,
        provider: ShadowProvider::Claude,
        source: SourceId {
            session_id: "session".into(),
            path,
            dev,
            ino,
        },
    };
    let off = scoped(None, || {
        InputFacts::open(binding.clone())
            .unwrap()
            .poll(u64::MAX)
            .unwrap()
    });
    let (observer, rx) = fixture(16);
    scoped(Some(observer.clone()), || {
        for _ in 0..2 {
            let on = InputFacts::open(binding.clone())
                .unwrap()
                .poll(u64::MAX)
                .unwrap();
            assert_eq!(on, off);
        }
        // Opening at a closer yields EdgeTurn, not another measured completion.
        let offset = format!("{}\n{}\n", records[0], records[1]).len() as u64;
        InputFacts::open_at(binding, offset)
            .unwrap()
            .poll(u64::MAX)
            .unwrap();
    });
    let events: Vec<_> = rx
        .try_iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["logical_turn_key"], events[1]["logical_turn_key"]);
    assert_eq!(
        events[0]["physical_observation_key"],
        events[1]["physical_observation_key"]
    );
    assert_eq!(events[0]["logical_turn_key"]["native_turn_id"], "native-a");
    assert_eq!(
        events[0]["physical_observation_key"]["source_id"]["dev"],
        dev
    );
    assert_eq!(events[0]["outcome"], "completed");
    assert!(
        !serde_json::to_string(&events)
            .unwrap()
            .contains("private text")
    );
    assert_eq!(json(&observer)["counters"]["closed_events"], 2);
}

#[test]
fn n1_full_closed_and_contended_observer_do_not_wait_for_consumer() {
    for failure in ["full", "closed", "lock", "sink"] {
        let (observer, rx) = fixture(0);
        if failure == "closed" {
            drop(rx);
        }
        if failure == "sink" {
            observer.sink_errors.fetch_add(1, Ordering::SeqCst);
        }
        let guard = (failure == "lock")
            .then(|| observer.channels.lock().unwrap_or_else(|e| e.into_inner()));
        let (done, returned) = std::sync::mpsc::channel();
        let worker = observer.clone();
        let thread = std::thread::spawn(move || {
            scoped(Some(worker), || {
                let request = placeholder_attempt(context(), 7, "post_placeholder", None, None);
                placeholder_result(request, Ok(99));
                done.send(99).unwrap();
            });
        });
        assert_eq!(
            returned.recv_timeout(Duration::from_secs(2)).unwrap(),
            99,
            "{failure}"
        );
        drop(guard);
        thread.join().unwrap();
        assert!(observer.health() != (0, 0));
    }
}

#[test]
fn n1_bounds_ids_channels_and_open_requests_without_eviction() {
    let (observer, _rx) = fixture(1024);
    scoped(Some(observer.clone()), || {
        assert!(bounded_id(Some(&"x".repeat(ID_CAP + 1))).is_none());
        for _ in 0..=OPEN_CAP {
            drop(placeholder_attempt(
                context(),
                7,
                "post_placeholder",
                None,
                None,
            ));
        }
        assert_eq!(json(&observer)["open_attempts"], OPEN_CAP);
        assert_eq!(json(&observer)["open_attempts_complete"], false);
        for id in 1..=(CHANNEL_CAP as u64 + 1) {
            confirmation_boundary(id);
        }
        assert!(
            observer
                .channels
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len()
                <= CHANNEL_CAP
        );
        assert!(observer.health().0 >= 3);
    });
}

#[test]
fn n1_confirmation_event_is_later_than_transition_and_preserves_first_window() {
    let (observer, rx) = fixture(16);
    scoped(Some(observer.clone()), || {
        let before = confirmation_boundary(7);
        // A request after actual confirm but before the caller receives the returned list.
        let request = placeholder_attempt(context(), 7, "post_placeholder", None, None);
        mode_confirmed("codex", 7, before);
        let later = confirmation_boundary(7);
        mode_confirmed("codex", 7, later);
        placeholder_result(request, Ok(99));
        let events: Vec<_> = rx
            .try_iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        assert!(events[0]["seq"].as_u64().unwrap() < events[1]["seq"].as_u64().unwrap());
        assert_eq!(events[1]["actual_confirmed_at_us"], serde_json::Value::Null);
        assert_eq!(events[1]["confirmation_window_start"]["seq"], 0);
        assert_eq!(json(&observer)["first_confirmation"]["seq"], 0);
    });
}
