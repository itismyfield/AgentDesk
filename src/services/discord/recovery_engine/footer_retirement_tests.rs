use super::*;

fn state() -> inflight::InflightTurnState {
    inflight::InflightTurnState::new(
        ProviderKind::Claude,
        8_071_817,
        None,
        1,
        41,
        300,
        "turn".into(),
        None,
        None,
        None,
        None,
        0,
    )
}

#[test]
fn retired_recovery_completion_keeps_only_an_existing_same_turn_panel() {
    let mut original = state();
    assert_eq!(completion_target_for_flags(true, &original, None), None);
    original.status_message_id = Some(200);
    assert_eq!(
        completion_target_for_flags(true, &original, None),
        Some(Some(MessageId::new(200)))
    );
    assert_eq!(completion_target_for_flags(false, &original, None), None);
    let mut persisted = original.clone();
    persisted.status_message_id = Some(400);
    assert_eq!(
        completion_target_for_flags(true, &original, Some(&persisted)),
        Some(Some(MessageId::new(400)))
    );
    persisted.user_msg_id += 1;
    assert_eq!(
        completion_target_for_flags(true, &original, Some(&persisted)),
        Some(Some(MessageId::new(200)))
    );
}
