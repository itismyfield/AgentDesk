//! Site × child: each census funnel's O test again with its channel as a recorded thread child of
//! a selected parent, so a funnel that reads only boot membership posts the child's body.

use super::test_override::funnel_as_child;

const WATCHER: &str = "services::discord::tmux::tmux_watcher::";
const EPILOGUE: &str =
    "services::discord::turn_bridge::terminal_outcome_delivery::delivery_epilogue_tests::";

fn run(prefix: &str, rest: &str) {
    funnel_as_child(&format!("{prefix}{rest}"));
}

#[test]
fn child_w01_watcher_turn_shows_no_body() {
    run(
        WATCHER,
        "tests::streaming_harness_tests::o_delegated_watcher_tests::o_delegated_watcher_turn_shows_no_body_and_records_no_frontier",
    );
}

#[test]
fn child_w02_rollover_tick_writes_no_body() {
    run(
        WATCHER,
        "streaming_status_tick::committed_progress_tests::o_delegated_rollover_tick_writes_no_body",
    );
}

#[test]
fn child_w04_single_message_footer_sends_no_body() {
    run(
        WATCHER,
        "single_message_footer_tests::o_delegated_single_message_footer_completion_sends_no_body",
    );
}

#[test]
fn child_w10_tui_body_is_cut_on_direct_gateways() {
    run(
        EPILOGUE,
        "turn_mode_anchor_tests::o_delegated_tui_body_is_cut_on_direct_gateways_but_not_headless",
    );
}

#[test]
fn child_w11_cancelled_partial_body_is_not_replaced() {
    run(
        EPILOGUE,
        "o_delegated_cancelled_partial_body_is_not_replaced",
    );
}

#[test]
fn child_w13_foreign_custody_follows_destination_membership() {
    run(
        EPILOGUE,
        "rowless_receipt_tests::o_delegated_foreign_custody_follows_destination_membership",
    );
}

#[test]
fn child_w20_idle_range_is_consumed_without_transport() {
    run(
        "services::discord::session_relay_sink::",
        "delivery_orchestration_tests::o_delegated_idle_range_is_consumed_once_without_transport_or_evidence",
    );
}

#[test]
fn child_w21_task_response_leaves_no_legacy_claim() {
    run(
        "services::discord::session_relay_sink::",
        "task_notification_context::tests::o_delegated_task_response_leaves_no_legacy_claim",
    );
}

#[test]
fn child_w30_recovery_body_posts_only_the_marker() {
    run(
        "services::discord::recovery_engine::",
        "o_recovery_cut_tests::o_delegated_recovery_body_posts_only_the_marker_without_evidence",
    );
}

#[test]
fn child_w31_captured_recovery_range_is_consumed_without_send() {
    run(
        "services::discord::recovery_engine::",
        "o_recovery_cut_tests::o_delegated_captured_recovery_range_is_consumed_without_send",
    );
}

#[test]
fn child_w33_stale_leak_recovery_resends_nothing() {
    run(
        "services::discord::health::recovery::",
        "o_stale_leak_cut_tests::o_delegated_stale_leak_recovery_resends_nothing",
    );
}

#[test]
fn child_w34_restart_handoff_keeps_only_the_marker() {
    run(
        "services::discord::tmux_restart_handoff::",
        "o_cut_tests::o_delegated_restart_handoff_keeps_only_the_marker",
    );
}

#[test]
fn child_w35_anchor_repost_is_skipped() {
    run(
        "services::discord::recovery_paths::",
        "o_anchor_repost_tests::o_delegated_anchor_repost_is_skipped",
    );
}

#[test]
fn child_backstop_done_turn_needs_no_legacy_confirmation() {
    run(
        "services::discord::turn_finalizer::",
        "watcher_backstop::tests::o_delegated_done_turn_needs_no_legacy_delivery_confirmation",
    );
}
