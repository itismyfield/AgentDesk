use super::finalize_epilogue::resume_pinned_watcher;
use super::*;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};

fn handle() -> TmuxWatcherHandle {
    TmuxWatcherHandle {
        tmux_session_name: "C1-source".into(),
        output_path: "/C1-source.jsonl".into(),
        paused: Arc::new(AtomicBool::new(true)),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(false)),
        pause_epoch: Arc::new(AtomicU64::new(0)),
        turn_delivered: Arc::new(AtomicBool::new(true)),
        last_heartbeat_ts_ms: Arc::new(AtomicI64::new(super::super::tmux_watcher_now_ms())),
    }
}
fn copy_handle(h: &TmuxWatcherHandle) -> TmuxWatcherHandle {
    TmuxWatcherHandle {
        tmux_session_name: h.tmux_session_name.clone(),
        output_path: h.output_path.clone(),
        paused: h.paused.clone(),
        resume_offset: h.resume_offset.clone(),
        cancel: h.cancel.clone(),
        pause_epoch: h.pause_epoch.clone(),
        turn_delivered: h.turn_delivered.clone(),
        last_heartbeat_ts_ms: h.last_heartbeat_ts_ms.clone(),
    }
}
fn unchanged(h: &TmuxWatcherHandle) {
    assert_eq!(*h.resume_offset.lock().unwrap(), None);
    assert!(h.paused.load(Ordering::Acquire));
    assert!(h.turn_delivered.load(Ordering::Acquire));
}
fn capture(shared: &SharedData, path: &str) -> Option<WatcherClaimIncarnation> {
    WatcherClaimIncarnation::capture_for_source(
        &shared.tmux_watchers,
        "C1-source",
        std::path::Path::new(path),
    )
}

#[test]
fn c1_missing_and_idle_tail_mismatch_never_resume_latest() {
    let shared = super::super::make_shared_data_for_tests();
    assert!(capture(&shared, "/C1-source.jsonl").is_none());
    let h = handle();
    shared
        .tmux_watchers
        .insert(ChannelId::new(580801), copy_handle(&h));
    // Process/no-handoff and Claude idle-tail path mismatch both lack a source pin.
    for pin in [None, capture(&shared, "/different-transcript.jsonl")] {
        assert!(pin.is_none());
        assert!(!resume_pinned_watcher(
            &shared.tmux_watchers,
            pin.as_ref(),
            900
        ));
        unchanged(&h);
    }
    assert!(
        WatcherClaimIncarnation::capture_for_source(
            &shared.tmux_watchers,
            "other-tmux",
            std::path::Path::new("/C1-source.jsonl")
        )
        .is_none()
    );
}

#[test]
fn c1_synthetic_and_handoff_stale_pins_leave_replacement_untouched() {
    let shared = super::super::make_shared_data_for_tests();
    let owner = ChannelId::new(580802);
    let a = handle();
    shared.tmux_watchers.insert(owner, copy_handle(&a));
    let synthetic = capture(&shared, "/C1-source.jsonl").unwrap();
    let mut handoff = None;
    super::runtime_handoff_loop::adopt_claimed_watcher_delivery_marker(&mut handoff, &synthetic);
    let b = handle();
    shared.tmux_watchers.insert(owner, copy_handle(&b));
    for pin in [Some(synthetic), handoff] {
        assert!(!resume_pinned_watcher(
            &shared.tmux_watchers,
            pin.as_ref(),
            901
        ));
        unchanged(&b);
        unchanged(&a);
    }
}

#[test]
fn c1_cancelled_registered_pin_leaves_all_effects_untouched() {
    let shared = super::super::make_shared_data_for_tests();
    let h = handle();
    shared
        .tmux_watchers
        .insert(ChannelId::new(580803), copy_handle(&h));
    let pin = capture(&shared, "/C1-source.jsonl").unwrap();
    h.cancel.store(true, Ordering::Release);
    assert!(!resume_pinned_watcher(
        &shared.tmux_watchers,
        Some(&pin),
        902
    ));
    assert!(capture(&shared, "/C1-source.jsonl").is_none());
    unchanged(&h);
}

#[test]
fn c1_same_synthetic_and_handoff_pin_resume_without_clearing_marker() {
    let shared = super::super::make_shared_data_for_tests();
    let h = handle();
    shared
        .tmux_watchers
        .insert(ChannelId::new(580804), copy_handle(&h));
    let pin = capture(&shared, "/C1-source.jsonl").unwrap();
    let mut handoff = None;
    super::runtime_handoff_loop::adopt_claimed_watcher_delivery_marker(&mut handoff, &pin);
    for (pin, offset) in [(Some(pin), 903), (handoff, 904)] {
        h.paused.store(true, Ordering::Release);
        assert!(resume_pinned_watcher(
            &shared.tmux_watchers,
            pin.as_ref(),
            offset
        ));
        assert_eq!(*h.resume_offset.lock().unwrap(), Some(offset));
        assert!(!h.paused.load(Ordering::Acquire));
        assert!(h.turn_delivered.load(Ordering::Acquire));
    }
}

#[test]
fn c1_poisoned_resume_lock_does_not_unpause() {
    let shared = super::super::make_shared_data_for_tests();
    let h = handle();
    shared
        .tmux_watchers
        .insert(ChannelId::new(580805), copy_handle(&h));
    let pin = capture(&shared, "/C1-source.jsonl").unwrap();
    let _ = std::panic::catch_unwind(|| {
        let _guard = h.resume_offset.lock().unwrap();
        panic!("poison resume mutex");
    });
    assert!(!resume_pinned_watcher(
        &shared.tmux_watchers,
        Some(&pin),
        905
    ));
    assert!(h.paused.load(Ordering::Acquire));
    assert_eq!(*h.resume_offset.lock().unwrap_err().into_inner(), None);
    assert!(h.turn_delivered.load(Ordering::Acquire));
}

#[test]
fn c1_both_late_writers_consume_pin_without_registry_backfill() {
    let completion = include_str!("completion_postlude.rs");
    let epilogue = include_str!("finalize_epilogue.rs");
    assert_eq!(
        completion
            .matches("finalize_epilogue::resume_pinned_watcher(")
            .count(),
        1
    );
    assert_eq!(
        epilogue
            .matches("resume_pinned_watcher(\n                    &shared_owned.tmux_watchers,")
            .count(),
        1
    );
    for source in [completion, epilogue] {
        assert!(!source.contains("tmux_watchers.get(&watcher_owner_channel_id)"));
        assert!(source.contains("watcher_delivery_pin.as_ref()"));
    }
}
