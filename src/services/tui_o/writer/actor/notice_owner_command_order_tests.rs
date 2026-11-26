use super::*;

#[tokio::test(start_paused = true)]
async fn known_command_receipt_excludes_logical_notice_across_restart() {
    let c = Case::new("known-command-restarts");
    let frame = c.frame();
    let (registry, ticket) = c.v2(&frame, 8822, false);
    append_queued_command(&c, &frame);
    assert_eq!(
        registry.receipt(
            NAMESPACE,
            &ticket,
            (&c.path, 0, "command", Some("physical-command"), &frame)
        ),
        Some(true)
    );

    let mut actor = c.open();
    c.run(&mut actor).await;
    assert_eq!(c.notices(), 0);

    append(&c.path, &row("original-input", &frame));
    append(&c.path, &row("independent-input", &frame));
    actor.read_sources_for_test();
    drop(actor);

    let mut restarted = c.open();
    c.run(&mut restarted).await;
    let ledger = restarted.writer_mut_for_test().store().ledger();
    assert_eq!(
        ledger.excluded(&input_key("original-input")),
        Some("input_notice_source")
    );
    let (_, independent) = ledger
        .latest_piece(&input_key("independent-input"), 0)
        .expect("a different input with the same text owes its own notice");
    assert!(matches!(
        independent.outcome,
        Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(
            _
        ))
    ));
    assert_eq!(c.notices(), 1);
}
