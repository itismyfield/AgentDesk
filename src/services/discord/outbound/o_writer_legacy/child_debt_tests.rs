use super::*;
use crate::services::tui_o::{
    shadow::{ShadowProvider, SourceId},
    writer::{
        adoption::{LegacyDelivery, LegacyTurnIdentity},
        binding::{BindingEvent, BindingRecord},
    },
};

fn fixture() -> (
    BindingEvent,
    LegacyTurnIdentity,
    LegacyDelivery,
    dr::ConfirmedDeliveryReceipt,
) {
    let binding = BindingEvent {
        seq: 1,
        channel_id: 42,
        provider: ShadowProvider::Claude,
        tmux_session: "fixture-pane".into(),
        execution_nonce: "execution-a".into(),
        committed_at: chrono::Utc::now(),
        record: BindingRecord::Resolved {
            resolves_seq: 1,
            source: SourceId {
                session_id: "fixture".into(),
                path: "fixture.jsonl".into(),
                dev: 1,
                ino: 1,
            },
        },
    };
    let turn = (
        1,
        "fixture-start".into(),
        Some("fixture-pane".into()),
        Some(5),
        Some("turn-a".into()),
    );
    let scope = LegacyDelivery {
        source_path: "fixture.jsonl".into(),
        execution_nonce: Some("execution-a".into()),
        owner: 41,
        generation: 7,
    };
    let receipt = dr::ConfirmedDeliveryReceipt {
        source: dr::ExactJsonlSourceIdentity {
            provider: "claude".into(),
            tmux_session_name: "fixture-pane".into(),
            turn_nonce: "turn-a".into(),
            range: (5, 20),
            generation_mtime_ns: 7,
            offset_authority_channel_id: 41,
            delivery_channel_id: 42,
        },
        delivery_channel_id: 42,
        message_id: 1,
    };
    (binding, turn, scope, receipt)
}
#[test]
fn ch3fix_historical_a_receipt_can_arrive_after_b() {
    let (binding, turn, scope, a) = fixture();
    let mut b = a.clone();
    b.source.turn_nonce = "turn-b".into();
    let mut record = dr::DeliveryRecord {
        confirmed_deliveries: vec![b],
        ..Default::default()
    };
    assert_eq!(
        historical_frontier_in(record.clone(), &binding, Some(&turn), Some(&scope), 20),
        None
    );
    record.confirmed_deliveries.push(a);
    assert_eq!(
        historical_frontier_in(record, &binding, Some(&turn), Some(&scope), 20),
        Some(20)
    );
}
#[test]
fn ch3fix_historical_receipt_requires_the_frozen_scope() {
    let (binding, turn, scope, a) = fixture();
    let check = |receipt| {
        historical_frontier_in(
            dr::DeliveryRecord {
                confirmed_deliveries: vec![receipt],
                ..Default::default()
            },
            &binding,
            Some(&turn),
            Some(&scope),
            20,
        )
    };
    for wrong in [
        LegacyDelivery {
            source_path: "b.jsonl".into(),
            ..scope.clone()
        },
        LegacyDelivery {
            execution_nonce: Some("execution-b".into()),
            ..scope.clone()
        },
    ] {
        assert_eq!(
            historical_frontier_in(
                dr::DeliveryRecord {
                    confirmed_deliveries: vec![a.clone()],
                    ..Default::default()
                },
                &binding,
                Some(&turn),
                Some(&wrong),
                20
            ),
            None
        );
    }
    let mut cases = Vec::new();
    let mut wrong = a.clone();
    wrong.source.offset_authority_channel_id = 99;
    cases.push(wrong);
    let mut wrong = a.clone();
    wrong.source.generation_mtime_ns = 8;
    cases.push(wrong);
    let mut wrong = a.clone();
    wrong.source.tmux_session_name = "other-pane".into();
    cases.push(wrong);
    let mut wrong = a.clone();
    wrong.source.provider = "codex".into();
    cases.push(wrong);
    let mut wrong = a.clone();
    wrong.source.delivery_channel_id = 43;
    cases.push(wrong);
    let mut wrong = a.clone();
    wrong.source.range = (6, 20);
    cases.push(wrong);
    let mut wrong = a.clone();
    wrong.source.range = (5, 21);
    cases.push(wrong);
    let mut wrong = a.clone();
    wrong.message_id = 0;
    cases.push(wrong);
    for wrong in cases {
        assert_eq!(check(wrong), None);
    }
    assert_eq!(
        historical_frontier_in(
            dr::DeliveryRecord {
                confirmed_deliveries: vec![a],
                ..Default::default()
            },
            &binding,
            Some(&turn),
            None,
            20
        ),
        None
    );
}
