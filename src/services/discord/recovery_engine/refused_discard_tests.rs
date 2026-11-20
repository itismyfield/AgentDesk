use super::*;

// The key follows the turn nonce alone: a later view of the same turn reuses the first reason.
#[test]
fn a_rows_reason_key_ignores_its_owner_message_and_generation() {
    let mut row = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        7,
        None,
        1,
        2,
        3,
        "prompt".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_nonce = Some("turn-a".into());
    let first = identity(&ProviderKind::Claude, &row).unwrap();
    (
        row.request_owner_user_id,
        row.user_msg_id,
        row.born_generation,
    ) = (9, 8, 6);
    assert_eq!(identity(&ProviderKind::Claude, &row).as_ref(), Some(&first));
    row.turn_nonce = Some("turn-b".into());
    assert_ne!(identity(&ProviderKind::Claude, &row).as_ref(), Some(&first));
}
