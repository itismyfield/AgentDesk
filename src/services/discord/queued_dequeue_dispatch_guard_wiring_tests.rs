#[test]
fn dequeue_uses_preservation_aware_stale_dispatch_guard() {
    let source = include_str!("queue_dispatch.rs");
    let function_start = source
        .find("async fn mailbox_take_soft_intervention(")
        .expect("mailbox dequeue helper exists");
    let function_body = &source[function_start..];
    let queued_guard = format!("{}{}", "stale_dispatch_turn_for_queued_", "intervention(");
    let text_guard = format!(
        "{}{}",
        "stale_dispatch_turn_for_", "text(shared.pg_pool.as_ref(), &intervention.text)"
    );

    assert!(
        function_body.contains(&queued_guard),
        "dequeue must retain the preservation-aware queued dispatch guard"
    );
    assert!(
        !function_body.contains(&text_guard),
        "dequeue must not bypass queued preservation with the raw text guard"
    );
}
