//! Busy inputs ride their birth episode's row: kept by a stale save, never inherited by a successor.
use super::*;
use crate::services::provider::CancelToken;

#[test]
fn an_episode_keeps_its_busy_inputs_and_a_successor_never_inherits_them() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 7_212_990;
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        channel + 1,
        channel + 2,
        "own".into(),
        None,
        Some("busy-episode".into()),
        None,
        None,
        0,
    );
    row.turn_nonce = Some("episode-a".into());
    save_inflight_state_create_new(&row).unwrap();
    let raw = || {
        let row = load_inflight_state_read_only(&ProviderKind::Claude, channel).unwrap();
        serde_json::to_value(row).unwrap()
    };
    assert!(
        raw().get("native_stop_busy_children").is_none(),
        "an old row has no field"
    );
    for attempt in ["busy-1", "busy-2", "busy-1"] {
        record_native_busy_child(&row, &CancelToken::new(), attempt);
    }
    let kept = raw()["native_stop_busy_children"].clone();
    assert_eq!(kept["attempts"], serde_json::json!(["busy-1", "busy-2"]));
    let decoded: InflightTurnState = serde_json::from_value(raw()).unwrap();
    assert_eq!(
        serde_json::to_value(decoded).unwrap()["native_stop_busy_children"],
        kept
    );
    // A same-episode snapshot taken before the busy inputs keeps them when it is saved.
    save_inflight_state(&row).unwrap();
    assert_eq!(
        raw()["native_stop_busy_children"],
        kept,
        "a stale save erases none"
    );
    let mut next = row.clone();
    next.turn_nonce = Some("episode-b".into());
    next.user_msg_id += 100;
    save_inflight_state(&next).unwrap();
    assert!(
        raw().get("native_stop_busy_children").is_none(),
        "a successor inherits none"
    );
}
