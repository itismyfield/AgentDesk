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
    let registrations = registry.native_evidence_registrations_for_test().unwrap();
    assert!(
        projection.receipts_for_test(&registrations).is_empty(),
        "absorption supplies no physical membership receipt"
    );
    let evidence = projection.analyze(&registrations).unwrap().evidence;
    let [executed] = evidence.as_slice() else {
        panic!("expected one native execution proof: {evidence:?}");
    };
    assert_eq!(&executed.ticket, ticket);
    assert_eq!(executed.disposition, Disposition::Executed);
    assert_eq!(
        executed.run.as_ref().unwrap().native_id_for_test(),
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

fn arm_absorption(c: &Case, registry: &history::v2::Registry, id: u64, frame: &str) -> Ticket {
    let mut key = registration(&c.path, &c.logical, OWNER_NONCE, id, false);
    key["attempt"]["source"]["offset"] = json!(std::fs::metadata(&c.path).unwrap().len());
    registry
        .prepare(NAMESPACE, &registered_input(&key, frame))
        .unwrap();
    registry
        .prepare_native_intent(NAMESPACE, &key, CHANNEL, &Intent::Follower)
        .unwrap();
    let ticket = registry.native_ticket(NAMESPACE, &key).unwrap();
    registry.note_native_any_key(NAMESPACE, &ticket).unwrap();
    ticket
}

fn append_parent_turn(c: &Case, id: &str) {
    let parent = "[User: parent (ID: 42)]\nIndependent parent turn";
    assert!(crate::services::tui_o::writer::input_notices::discord_input(parent));
    append(&c.path, &row(id, parent));
}

fn append_remove_only(c: &Case, id: &str, frame: &str) {
    for value in [
        json!({"type":"queue-operation", "operation":"enqueue", "content":frame}),
        json!({"type":"queue-operation", "operation":"remove", "reason":"absorbed_mid_turn",
            "content":frame, "commandUuid":id, "deliveryId":format!("delivery-{id}")}),
        json!({"type":"system", "subtype":"turn_duration"}),
    ] {
        append(&c.path, format!("{value}\n").as_bytes());
    }
}

async fn capture_settled_absorption(label: &str) -> (Case, Host, history::v2::Registry, Ticket) {
    let c = Case::new(label);
    let frame = c.frame();
    append_parent_turn(&c, "parent-turn");
    let registry = history::v2::Registry::runtime().unwrap();
    let ticket = arm_absorption(&c, &registry, 8825, &frame);
    append_remove_only(&c, OWNED_ID, &frame);
    assert_native_absorption(&c, &registry, &ticket);
    let mut actor = c.open();
    c.run(&mut actor).await;
    assert_retirement_notice_clear(&c, &mut actor);
    (c, actor, registry, ticket)
}

#[tokio::test(start_paused = true)]
async fn owned_absorbed_remove_settles_without_physical_receipt_across_restart() {
    let (c, actor, registry, ticket) = capture_settled_absorption("owned-remove-only").await;
    let frame = c.frame();
    let key = ticket.key;
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

fn assert_registration_still_open(registry: &history::v2::Registry, ticket: &Ticket) {
    let settlement = registry.native_settlement(NAMESPACE, ticket).unwrap();
    assert!(settlement.outcome.is_none());
    assert!(!settlement.cleanup_complete);
    let registrations = registry.native_evidence_registrations_for_test().unwrap();
    let seen = registrations.iter().find(|r| r.ticket == *ticket).unwrap();
    assert!(seen.candidate);
    assert!(seen.receipts.is_empty());
}

#[tokio::test(start_paused = true)]
async fn consumed_absorption_does_not_contest_the_next_registered_remove() {
    const NEXT: &str = "absorbed-next-owned";
    let (c, mut actor, registry, first) = capture_settled_absorption("owned-remove-repeat").await;
    assert_registration_still_open(&registry, &first);

    let frame = c.frame();
    append_parent_turn(&c, "second-parent-turn");
    let second = arm_absorption(&c, &registry, 8826, &frame);
    assert_registration_still_open(&registry, &second);
    append_remove_only(&c, NEXT, &frame);
    c.run(&mut actor).await;
    assert_registration_still_open(&registry, &first);
    c.excluded(&mut actor, NEXT);
    assert_retirement_notice_clear(&c, &mut actor);
    drop(actor);

    let mut restarted = c.open();
    c.run(&mut restarted).await;
    c.excluded(&mut restarted, NEXT);
    assert_retirement_notice_clear(&c, &mut restarted);

    append_parent_turn(&c, "contested-parent-turn");
    arm_absorption(&c, &registry, 8827, &frame);
    arm_absorption(&c, &registry, 8828, &frame);
    append_remove_only(&c, "contested-remove", &frame);
    c.run(&mut restarted).await;
    let key = input_key("contested-remove");
    assert_eq!(notice_evidence(&mut restarted, &key), 1);
    let debt = restarted.undelivered_for_test(2).unwrap();
    let ledger = restarted.writer_mut_for_test().store().ledger();
    assert_eq!(debt.notice_owed, 1);
    assert_eq!(ledger.excluded(&key), None);
    assert!(ledger.latest_piece(&key, 0).is_none());
    assert_eq!(c.notices(), 0);
}

#[tokio::test(start_paused = true)]
async fn consumed_absorption_does_not_block_a_separate_unowned_remove() {
    const NEXT: &str = "absorbed-next-unowned";
    let (c, mut actor, registry, first) = capture_settled_absorption("unowned-remove-repeat").await;
    assert_registration_still_open(&registry, &first);

    let frame = c.frame();
    append_parent_turn(&c, "second-parent-turn");
    append_remove_only(&c, NEXT, &frame);
    c.run(&mut actor).await;
    assert_registration_still_open(&registry, &first);
    assert_eq!(c.notices(), 1);
    assert_eq!(actor.undelivered_for_test(2).unwrap().notice_owed, 0);
    drop(actor);

    let mut restarted = c.open();
    c.run(&mut restarted).await;
    assert_eq!(restarted.undelivered_for_test(3).unwrap().notice_owed, 0);
    let ledger = restarted.writer_mut_for_test().store().ledger();
    assert_eq!(
        ledger.excluded(&input_key(OWNED_ID)),
        Some("input_notice_source")
    );
    let (_, independent) = ledger
        .latest_piece(&input_key(NEXT), 0)
        .expect("an unowned remove in the next turn owes its own notice");
    assert!(matches!(
        independent.outcome,
        Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(
            _
        ))
    ));
    assert_eq!(c.notices(), 1);
}
