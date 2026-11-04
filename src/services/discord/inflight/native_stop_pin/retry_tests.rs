use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider::CancelToken;
use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding};
use std::io::Write;
use std::sync::Arc;

#[test]
fn native_pin_successor_stops_observing_across_a_thousand_reader_lines() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 775_600_101;
    let path = crate::config::runtime_root()
        .unwrap()
        .join("pin-successor.jsonl");
    let session = "AgentDesk-claude-pin-successor-7756";
    let user = serde_json::json!({"type":"user","uuid":"pin-original","sessionId":"retry-sid",
        "message":{"role":"user","content":"original prompt"}})
    .to_string()
        + "\n";
    std::fs::write(&path, &user).unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(session, "spawn_nonce"),
        "retry-pane",
    )
    .unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(session, "generation"),
        "1",
    )
    .unwrap();
    tui_prompt_dedupe::register_tmux_runtime_binding(
        session,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("retry-sid".into()),
            last_offset: user.len() as u64,
            relay_last_offset: None,
        },
    );
    let pin = crate::services::claude_tui::input::UserReceipt::native_pin_before_tail(
        session,
        "original prompt",
        Some("retry-sid"),
        user.len() as u64,
    )
    .expect("the exact original native input is verified");
    let token = Arc::new(CancelToken::new());
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        channel + 1,
        channel + 2,
        "original prompt".into(),
        Some("retry-sid".into()),
        Some(session.into()),
        Some(path.display().to_string()),
        None,
        0,
    );
    row.turn_nonce = token.turn_nonce().map(str::to_owned);
    row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    row.turn_start_offset = Some(0);
    save_inflight_state_create_new(&row).unwrap();
    bind_native_stop_pin(&row, &token);
    let mut successor = row.clone();
    successor.turn_nonce = Some("successor-episode".into());
    successor.user_msg_id += 100;
    save_inflight_state(&successor).unwrap();
    token.native_stop.receipt_sink().adopt(pin);
    let mut transcript = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    for _ in 0..1000 {
        writeln!(
            transcript,
            "{{\"type\":\"system\",\"subtype\":\"progress\"}}"
        )
        .unwrap();
    }
    writeln!(
        transcript,
        "{{\"type\":\"system\",\"subtype\":\"stop_hook_summary\"}}"
    )
    .unwrap();
    drop(transcript);
    let (tx, _rx) = std::sync::mpsc::channel();
    let (_, stats) =
        crate::services::session_backend::read_claude_tui_output_file_until_result_with_harvest(
            path.to_str().unwrap(),
            user.len() as u64,
            tx,
            Some(token),
            crate::services::provider::SessionProbe::process(|| true),
        )
        .unwrap();
    assert!(
        stats.decoded_terminal,
        "the actual native reader reached its terminal"
    );
    let attempts = native_pin_observation_count(channel);
    assert!(
        (1..=2).contains(&attempts),
        "a terminal successor refusal ran {attempts} storage callbacks"
    );
    let stored = load_inflight_state(&ProviderKind::Claude, channel).unwrap();
    assert_eq!(stored.turn_nonce, successor.turn_nonce);
    assert!(
        serde_json::to_value(stored)
            .unwrap()
            .get("native_stop_user_pin")
            .is_none()
    );
}
