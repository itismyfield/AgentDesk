//! Source-text gate wiring checks for the queued-card gate tests.

fn region<'a>(source: &'a str, label: &str, start: &str, end: &str) -> &'a str {
    let from = source
        .find(start)
        .unwrap_or_else(|| panic!("{label}: start marker missing ({start})"));
    let len = source[from..]
        .find(end)
        .unwrap_or_else(|| panic!("{label}: end marker missing ({end})"));
    &source[from..from + len]
}

/// Every spelling by which a gated region could reach Discord's message DELETE
/// or PATCH without a teardown token.
///
/// Enumerated from the tree, NOT from what the pre-#5035 code happened to call:
/// `git grep -ohE "[A-Za-z_.:]*(delete|edit)_[a-z_]*message[a-z_]*\(" --
/// 'src/services/discord/**/*.rs' | sort -u`. Both the serenity spellings
/// (`.delete_message(`, `.edit_message(`) and the in-tree helper spellings
/// (`http::delete_channel_message`, `http::edit_channel_message`, the
/// `_with_components` and `edit_outbound_message*` variants) are covered; the
/// earlier two-string list missed every helper spelling in both directions.
///
/// This list is deliberately NOT applied to `queued_card_gate.rs` itself, which
/// legitimately calls `http::delete_channel_message` / `http::edit_channel_message`
/// inside its teardown helpers. The gate file is not among the regions below.
const RAW_DISCORD_MESSAGE_OPS: &[&str] = &[
    "delete_message(",
    "delete_channel_message(",
    "delete_rollback_channel_message(",
    "edit_message(",
    "edit_channel_message(",
    "edit_channel_message_with_components(",
    "edit_outbound_message(",
    "edit_outbound_message_classified(",
];

/// Both conditions must hold INDEPENDENTLY: the region routes through the gate,
/// AND it holds no raw destructive call. A region containing a gate call *and* a
/// raw call fails the second assertion — co-existence is rejected, not accepted.
fn gated(label: &str, source: &str, start: &str, end: &str) {
    let body = region(source, label, start, end);
    assert!(body.contains("queued_card_gate::"), "{label}: not gated");
    for op in RAW_DISCORD_MESSAGE_OPS {
        assert!(
            !body.contains(op),
            "{label}: raw `{op}` inside a gated region"
        );
    }
}

pub(super) fn assert_gated_sites() {
    const MOD_RS: &str = include_str!("../../../mod.rs");
    const KICKOFF_RS: &str = include_str!("../../../queue_dispatch/kickoff.rs");
    const GATEWAY_RS: &str = include_str!("../../../gateway.rs");
    const INTAKE_RS: &str = include_str!("../../../router/message_handler/intake_turn.rs");
    const HANDOFF_RS: &str =
        include_str!("../../../router/message_handler/intake_turn/placeholder_handoff.rs");
    const BOOTSTRAP_RS: &str = include_str!("../../../runtime_bootstrap/queued_placeholders.rs");
    const FLUSH_RS: &str = include_str!("../../../runtime_bootstrap/recovery_flush.rs");
    const CONTROLLER_RS: &str = include_str!("../../../placeholder_controller.rs");

    gated(
        "A1/A2 queue-exit drain",
        MOD_RS,
        "async fn queue_exit_drain_queued_placeholders",
        "struct QueueExitPendingPlaceholderDeleter",
    );
    gated(
        "A3 deferred drain",
        MOD_RS,
        "async fn drain_pending_queue_exit_placeholder_clears_with",
        "async fn enqueue_internal_followup",
    );
    gated(
        "A5 kickoff drain",
        KICKOFF_RS,
        "let drained_cards = gateway::drain_merged_queued_placeholders(",
        "let dispatch_result =",
    );
    gated(
        "A4 merged drain helper",
        GATEWAY_RS,
        "pub(super) async fn drain_merged_queued_placeholders",
        "fn live_bot_owner_provider",
    );
    gated(
        "A4 dispatch call site",
        GATEWAY_RS,
        "let drained = drain_merged_queued_placeholders(",
        "router::finish_admitted_queued_intake",
    );
    gated(
        "A6 dequeue hand-off",
        INTAKE_RS,
        "// Fresh anchor is live; tear down the buried queued card.",
        "📬➡️🔄 DISPATCH: queued dequeued",
    );
    gated(
        "A7 busy-notice reuse",
        HANDOFF_RS,
        "if let Some(stale_queued) = queued_placeholder_handoff",
        "routed the queued card through the #5035 gate",
    );
    gated(
        "A8 stale-card delete",
        BOOTSTRAP_RS,
        "async fn delete_stale_queued_placeholder_cards_with",
        "async fn collect_live_queue_message_ids",
    );

    // The A8 bootstrap caller does not name the gate itself — it must hand
    // `shared` to the helper that re-gates each card.
    let flush = region(
        FLUSH_RS,
        "A8 bootstrap caller",
        "delete_stale_queued_placeholder_cards(",
        "mark_reconcile_complete",
    );
    assert!(
        flush.contains("&shared_for_tmux2"),
        "A8 caller: no `shared`"
    );
    for op in RAW_DISCORD_MESSAGE_OPS {
        assert!(!flush.contains(op), "A8 caller: raw `{op}`");
    }
    assert!(
        !CONTROLLER_RS.contains(".detach_by_message("),
        "placeholder_controller.rs itself must hold no detach_by_message call site"
    );
}

pub(super) fn assert_exit_body_logs_both_failures() {
    let source = include_str!("../../queued_card_gate.rs");
    let body = source
        .split("async fn teardown_exit_body(")
        .nth(1)
        .unwrap()
        .split("async fn teardown_via_deleter(")
        .next()
        .unwrap();
    assert_eq!(body.matches("tracing::warn!").count(), 1);
    let warning = body
        .split("tracing::warn!")
        .nth(1)
        .unwrap()
        .split(";")
        .next()
        .unwrap();
    assert!(warning.contains("edit_error") && warning.contains("delete_error"));
    assert!(!body.contains("let _ ="));
}
