use super::*;
use serde_json::{Value, json};

fn fixture() -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        775_601,
        None,
        7,
        8,
        9,
        "한글 입력".into(),
        Some("pin-sid".into()),
        Some("pin-session".into()),
        Some("/tmp/pin-output.jsonl".into()),
        None,
        42,
    );
    row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    row.turn_nonce = Some("pin-episode".into());
    row.full_response = "남겨 둔 응답".into();
    row
}

fn valid_pin(row: &InflightTurnState) -> Value {
    json!({
        "input": {
            "uuid": "native-user",
            "source_file": [1, 2],
            "range": [42, 142],
            "receipt_start": 42,
            "row_digest": "row-digest",
            "frame_digest": "frame-digest",
            "session_id": row.session_id,
            "output_path": row.output_path,
            "tmux_session": row.tmux_session_name,
            "spawn_nonce": "pane-incarnation",
            "direct": false
        },
        "provider": row.provider,
        "channel_id": row.channel_id,
        "request_owner_user_id": row.request_owner_user_id,
        "user_msg_id": row.user_msg_id,
        "turn_nonce": row.turn_nonce,
        "started_at": row.started_at
    })
}

fn write_raw(row: &InflightTurnState, encoded: &Value) {
    let path = inflight_state_path(
        &inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        row.channel_id,
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(encoded).unwrap()).unwrap();
}

#[test]
fn f6_corrupt_native_pin_keeps_remaining_inflight_row() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let row = fixture();
    let expected = serde_json::to_value(&row).unwrap();
    let mut missing_input_field = valid_pin(&row);
    missing_input_field["input"]
        .as_object_mut()
        .unwrap()
        .remove("uuid");
    let mut wrong_nested_type = valid_pin(&row);
    wrong_nested_type["input"]["range"] = json!([42, "broken"]);
    for pin in [
        json!("broken"),
        json!({}),
        missing_input_field,
        wrong_nested_type,
    ] {
        let mut encoded = expected.clone();
        encoded["native_stop_user_pin"] = pin.clone();
        write_raw(&row, &encoded);
        let loaded = load_inflight_state(&ProviderKind::Claude, row.channel_id)
            .unwrap_or_else(|| panic!("malformed pin must not discard its row: {pin}"));
        assert!(loaded.native_stop_user_pin.is_none());
        assert_eq!(serde_json::to_value(loaded).unwrap(), expected);
    }
}

#[test]
fn f6_native_pin_round_trip_legacy_and_successor_keep_owner_boundary() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let row = fixture();
    let mut encoded = serde_json::to_value(&row).unwrap();
    let pin = valid_pin(&row);
    encoded["native_stop_user_pin"] = pin.clone();
    write_raw(&row, &encoded);
    let loaded = load_inflight_state(&ProviderKind::Claude, row.channel_id).unwrap();
    assert_eq!(serde_json::to_value(&loaded).unwrap(), encoded);

    let mut successor = loaded;
    successor.turn_nonce = Some("next-episode".into());
    successor.user_msg_id += 1;
    save_inflight_state(&successor).unwrap();
    let successor = load_inflight_state(&ProviderKind::Claude, row.channel_id).unwrap();
    assert!(successor.native_stop_user_pin.is_none());

    for legacy_pin in [None, Some(Value::Null)] {
        let mut legacy = encoded.clone();
        match legacy_pin {
            Some(value) => legacy["native_stop_user_pin"] = value,
            None => {
                legacy
                    .as_object_mut()
                    .unwrap()
                    .remove("native_stop_user_pin");
            }
        }
        write_raw(&row, &legacy);
        let loaded = load_inflight_state(&ProviderKind::Claude, row.channel_id).unwrap();
        assert!(loaded.native_stop_user_pin.is_none());
        assert_eq!(
            serde_json::to_value(loaded).unwrap(),
            serde_json::to_value(&row).unwrap()
        );
    }
}

#[test]
fn f6_invalid_non_pin_field_still_rejects_inflight_row() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let row = fixture();
    for malformed in ["user_msg_id", "last_offset", "managed_submission"] {
        let mut encoded = serde_json::to_value(&row).unwrap();
        encoded["native_stop_user_pin"] = json!("broken");
        encoded[malformed] = json!("broken");
        write_raw(&row, &encoded);
        assert!(load_inflight_state(&ProviderKind::Claude, row.channel_id).is_none());
    }
}
