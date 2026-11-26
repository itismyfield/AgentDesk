use super::*;
use crate::services::tui_input::human_frame::history::v2::native::{Disposition, Intent, Ticket};
use crate::services::tui_o::shadow::SourceBinding;
use crate::services::tui_o::writer::input_facts::native::Projection as NativeProjection;

const OWNED_ID: &str = "absorbed-original";

fn assert_native_absorption(c: &Case, registry: &history::v2::Registry, ticket: &Ticket) {
    let transcript = std::fs::read(&c.path).unwrap();
    let binding = SourceBinding {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        source: c.source.clone(),
    };
    let (_, projection) = NativeProjection::reopen(
        binding,
        transcript.len() as u64,
        &hex::encode(Sha256::digest(&transcript)),
    )
    .unwrap();
    let projection = projection.unwrap();
    let registrations = registry.native_evidence_registrations().unwrap();
    assert!(
        projection.receipts(&registrations).is_empty(),
        "absorption supplies no physical membership receipt"
    );
    let evidence = projection.evidence(&registrations).unwrap();
    let [executed] = evidence.as_slice() else {
        panic!("expected one native execution proof: {evidence:?}");
    };
    assert_eq!(&executed.ticket, ticket);
    assert_eq!(executed.disposition, Disposition::Executed);
    assert_eq!(
        executed.run.as_ref().unwrap().native_id.as_deref(),
        Some("parent-turn")
    );
}

fn assert_retirement_notice_clear(c: &Case, actor: &mut Host) {
    assert_eq!(c.notices(), 0);
    let debt = actor
        .undelivered_for_test(1)
        .expect("child retirement reads the actor's fresh debt");
    let ledger = actor.writer_mut_for_test().store().ledger();
    assert_eq!(
        (ledger.excluded(&input_key(OWNED_ID)), debt.notice_owed),
        (Some("input_notice_source"), 0)
    );
}

#[tokio::test(start_paused = true)]
async fn owned_absorbed_remove_settles_without_physical_receipt_across_restart() {
    let c = Case::new("owned-remove-only");
    let frame = c.frame();
    let parent = "[User: parent (ID: 42)]\nIndependent parent turn";
    assert!(crate::services::tui_o::writer::input_notices::discord_input(parent));
    append(&c.path, &row("parent-turn", parent));

    let registry = history::v2::Registry::runtime().unwrap();
    let mut key = registration(&c.path, &c.logical, OWNER_NONCE, 8825, false);
    key["attempt"]["source"]["offset"] = json!(std::fs::metadata(&c.path).unwrap().len());
    registry
        .prepare(NAMESPACE, &registered_input(&key, &frame))
        .unwrap();
    registry
        .prepare_native_intent(NAMESPACE, &key, CHANNEL, &Intent::Follower)
        .unwrap();
    let ticket = registry.native_ticket(NAMESPACE, &key).unwrap();
    registry.note_native_any_key(NAMESPACE, &ticket).unwrap();

    for value in [
        json!({"type":"queue-operation", "operation":"enqueue", "content":frame}),
        json!({"type":"queue-operation", "operation":"remove", "reason":"absorbed_mid_turn",
            "content":frame, "commandUuid":OWNED_ID, "deliveryId":"owned-delivery"}),
        json!({"type":"system", "subtype":"turn_duration"}),
    ] {
        append(&c.path, format!("{value}\n").as_bytes());
    }
    assert_native_absorption(&c, &registry, &ticket);

    let mut actor = c.open();
    c.run(&mut actor).await;
    assert_retirement_notice_clear(&c, &mut actor);
    drop(actor);

    registry
        .project(&json!({"namespace":NAMESPACE,"retained_inputs":[],
            "terminal_identities":[{"identity":key["identity"]}]}))
        .unwrap();
    let mut restarted = c.open();
    c.run(&mut restarted).await;
    assert_retirement_notice_clear(&c, &mut restarted);

    append_command(&c, "late-physical", OWNED_ID, &frame);
    append(&c.path, &row(OWNED_ID, &frame));
    c.run(&mut restarted).await;
    assert_retirement_notice_clear(&c, &mut restarted);

    append(&c.path, &row("separate-input", &frame));
    c.run(&mut restarted).await;
    let debt = restarted.undelivered_for_test(2).unwrap();
    assert_eq!(debt.notice_owed, 0);
    let ledger = restarted.writer_mut_for_test().store().ledger();
    let (_, independent) = ledger
        .latest_piece(&input_key("separate-input"), 0)
        .expect("a distinct UUID with the same content is still unowned");
    assert!(matches!(
        independent.outcome,
        Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(
            _
        ))
    ));
    assert_eq!(c.notices(), 1);
}
