//! Source-text wiring pin for the committed-progress tick tests.

pub(super) fn assert_progress_wiring() {
    assert!(include_str!("../turn_stream_collector.rs").contains("update_streaming_status_tick("));
    let tick = include_str!("../streaming_status_tick.rs");
    let at = |needle: &str| {
        tick.find(needle)
            .unwrap_or_else(|| panic!("missing {needle}"))
    };
    let persist = at("let progress_outcome = ctx.legacy_mode.is_legacy()");
    assert!(
        persist < at("if ctx.legacy_mode.is_legacy() && !terminal_progress_rejected {"),
        "gate the panel helper"
    );
    let cleanup = at("watcher_streaming_recent_stop_cleanup");
    assert!(
        cleanup < at("if terminal_progress_rejected {"),
        "cleanup precedes the stop"
    );
}
