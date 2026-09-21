use super::{
    EventLog, GLOBAL_EVENT_LOG, MAX_EVENTS, StructuredEvent, global, record, record_simple,
    test_capture::*,
};
use crate::services::observability::emit::tests::{
    ROOT_CAUSE_ACTIONS, every_emit_surface_records_one_recent_event_scenario as surfaces,
    invariant_severity_routes_to_exact_log_level_and_suffix_4422 as foreign_invariants,
    relay_root_cause_metric_wrappers_record_persistent_events_scenario as root_metrics,
};
use crate::services::turn_lifecycle::policy_observability_tests::{
    assert_noop_cancel_event,
    cancel_observability_emits_unknown_noop_direct_fallback_scenario as noop_cancel,
};
use crate::voice::flight::tests::record_writes_structured_voice_flight_event_scenario as voice;
use serde_json::json;
use std::sync::Arc;

fn foreign(action: impl FnOnce() + Send + 'static) {
    std::thread::spawn(action).join().unwrap();
}

fn publish_substitute(event: StructuredEvent) -> bool {
    let expected = serde_json::to_value(&event).unwrap();
    foreign(move || record(event));
    global()
        .recent(1)
        .first()
        .is_some_and(|row| serde_json::to_value(row).unwrap() == expected)
}

fn assert_global_append(seed: bool) {
    let log = Arc::new(EventLog::new(MAX_EVENTS));
    if seed {
        log.push(StructuredEvent::new(
            "sentinel",
            None,
            None,
            json!({"id": 0}),
        ));
    }
    GLOBAL_EVENT_LOG
        .set(log)
        .expect("fresh child must not have initialized global log");
    let before = global().recent(MAX_EVENTS);
    assert_eq!(before.len(), usize::from(seed), "seed setup must be real");
    if seed {
        assert_eq!(before[0].event_type, "sentinel");
        assert_eq!(before[0].payload, json!({"id": 0}));
    }
    let intended = vec![
        StructuredEvent::new("append first", Some(31), Some("Codex"), json!({"id": 1})),
        StructuredEvent::new("append second", None, None, json!({"id": 2})),
    ];
    let (_, captured) = capture_sync(|| {
        for event in intended.clone() {
            record(event);
        }
    });
    let after = global().recent(MAX_EVENTS);
    assert_eq!(
        after.len().saturating_sub(before.len()),
        2,
        "actual global append count"
    );
    assert_eq!(
        serde_json::to_value(&after[..before.len()]).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    let added = serde_json::to_value(&after[after.len() - 2..]).unwrap();
    assert_eq!(added, serde_json::to_value(&captured).unwrap());
    assert_eq!(added, serde_json::to_value(intended).unwrap());
}

fn interference(case: &str) {
    match case {
        "foreign" => {
            capture_sync(|| surfaces(|| foreign(foreign_invariants)));
        }
        "clear" => {
            capture_sync(|| {
                surfaces(|| {
                    foreign(|| {
                        global().clear();
                        record_simple("foreign after clear", None, None, json!({}));
                    })
                })
            });
        }
        "flood" => {
            capture_sync(|| {
                surfaces(|| {
                    foreign(|| {
                        tracing::subscriber::with_default(
                            tracing::subscriber::NoSubscriber::default(),
                            || {
                                for _ in 0..=MAX_EVENTS {
                                    record_simple("external flood", None, None, json!({}));
                                }
                            },
                        );
                    })
                })
            });
            assert!(
                global()
                    .recent(MAX_EVENTS)
                    .iter()
                    .all(|row| row.event_type == "external flood")
            );
        }
        "voice" => {
            global().clear();
            record_simple("voice sentinel", None, None, json!({}));
            capture_sync(|| voice(|| {}));
            assert!(
                global()
                    .recent(MAX_EVENTS)
                    .iter()
                    .any(|row| row.event_type == "voice sentinel"),
                "voice scenario cleared a foreign sentinel"
            );
        }
        "poison" => {
            let log = global();
            assert!(
                std::panic::catch_unwind(|| {
                    let _lock = log.inner.lock().unwrap();
                    panic!("intentional failed append");
                })
                .is_err()
            );
            let (_, rows) = capture_sync(|| record_simple("failed append", None, None, json!({})));
            assert!(rows.is_empty(), "failed append invented a captured event");
        }
        "negative" => negative_controls(),
        _ => panic!("unknown capture case"),
    }
}

fn negative_controls() {
    let (_, baseline) = capture_sync(|| root_metrics(&ROOT_CAUSE_ACTIONS, || {}));
    let a = ROOT_CAUSE_ACTIONS;
    for actions in [
        vec![a[0], a[1]],
        vec![a[0], a[0], a[1], a[2]],
        vec![a[2], a[1], a[0]],
    ] {
        let substitute = baseline[2].clone();
        let mut published = false;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            capture_sync(|| {
                root_metrics(&actions, || {
                    published = publish_substitute(substitute);
                })
            })
        }));
        assert!(
            published,
            "foreign substitute must be recorded byte-identically"
        );
        assert!(
            result.is_err(),
            "omission, duplicate or reversed own metric actions must fail"
        );
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (_, cancellation) = runtime.block_on(capture_async(noop_cancel(|| {})));
    assert_eq!(cancellation.len(), 1);
    let substitute = cancellation[0].clone();
    global().clear();
    capture_sync(|| {
        assert!(publish_substitute(substitute));
        assert_eq!(global().recent(MAX_EVENTS).len(), 1);
        assert!(
            std::panic::catch_unwind(assert_noop_cancel_event).is_err(),
            "foreign byte-identical no-channel event cannot replace omitted own action"
        );
    });
    assert!(
        std::panic::catch_unwind(|| capture_sync(|| {
            record(cancellation[0].clone());
            record(cancellation[0].clone());
            assert_noop_cancel_event();
        }))
        .is_err(),
        "duplicate own cancellation must fail"
    );
}

#[test]
fn isolated_capture_interference_and_append_equivalence() {
    const CASE: &str = "ADK_EVENT_CAPTURE_CHILD_CASE";
    if let Ok(case) = std::env::var(CASE) {
        match case.as_str() {
            "empty" => assert_global_append(false),
            "seeded" => assert_global_append(true),
            _ => interference(&case),
        }
        return;
    }
    for case in [
        "empty", "seeded", "foreign", "clear", "flood", "voice", "poison", "negative",
    ] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["services::observability::events::capture_stress_tests::isolated_capture_interference_and_append_equivalence", "--exact", "--nocapture"])
            .env(CASE, case).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("capture child {case} timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let output = child.wait_with_output().unwrap();
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "capture child {case}: {log}");
        assert!(
            log.contains("1 passed; 0 failed; 0 ignored"),
            "capture child {case} incomplete: {log}"
        );
    }
}
