use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::*;
use crate::services::discord::inflight::{RelayOwnerKind, TurnSource};
use crate::services::discord::tui_direct_pending_start::{PendingStartState, delete, persist};
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::turn_mode::{TestConfirmation, transcript_turns};

fn selecting(channel: u64) -> TurnConfig {
    TurnConfig {
        channels: [channel].into(),
        all_owned: Some(false),
    }
}

/// A parent Codex rollout whose first turn opened with a prompt and has not answered yet.
fn open_rollout(root: &Path, channel: u64) -> SourceBinding {
    let path = root.join(format!("rollout-{channel}.jsonl"));
    let lines = [
        json!({"type":"session_meta","payload":{"id":"parent","cwd":root,"source":"cli"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"hi"}]}}),
    ];
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&path, text).unwrap();
    SourceBinding {
        channel_id: channel,
        provider: ShadowProvider::Codex,
        source: source_id_for("parent", &path).unwrap(),
    }
}

fn answer(source: &SourceBinding) {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source.source.path)
        .unwrap();
    for line in [
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":"m1",
            "content":[{"type":"output_text","text":"done"}]}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1"}}),
    ] {
        writeln!(file, "{line}").unwrap();
    }
}

/// Legacy's synthetic turn row for the takeover turn, before or after its terminal delivery.
fn save_row(channel: u64, terminal: bool) -> PathBuf {
    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        channel,
        None,
        1,
        7,
        0,
        "hi".into(),
        None,
        Some("codex-takeover-fixture".into()),
        None,
        None,
        0,
    );
    row.turn_source = TurnSource::ExternalInput;
    row.set_relay_owner_kind(RelayOwnerKind::Watcher);
    row.terminal_delivery_committed = terminal;
    let root = inflight::inflight_runtime_root().unwrap();
    let path = inflight::inflight_state_path(&root, &ProviderKind::Codex, channel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _guard = inflight::lock_inflight_state_path(&path).unwrap();
    std::fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
    path
}

fn pending(channel: u64) -> TuiDirectPendingStart {
    TuiDirectPendingStart {
        hosted_source: None,
        hosted_source_checked: false,
        provider: "codex".into(),
        channel_id: channel,
        tmux_session_name: "codex-takeover-fixture".into(),
        prompt_text: "next".into(),
        anchor_message_id: 8,
        lease_relay_owner: "tmux_watcher".into(),
        lease_runtime_kind: Some("codex_tui".into()),
        lease_turn_id: None,
        lease_session_key: None,
        generation: 0,
        created_at_ms: 0,
        observed_at_ms: 0,
        state: PendingStartState::Waiting,
        attempt_count: 0,
        captured_source: None,
        native_turn_id: None,
    }
}

fn retry(confirmation: Confirmation) -> String {
    match confirmation {
        Confirmation::Retry(detail) => detail,
        other => panic!("expected a retry, got {other:?}"),
    }
}

/// An open, metadata-only or missing rollout keeps turn mode off and Legacy's turn row in place;
/// the boot selection alone never confirms.
#[test]
fn codex_confirmation_waits_for_the_takeover_turn_to_end_in_its_rollout() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let channel = 64_700_301;
    let source = open_rollout(dir.path(), channel);
    let row = save_row(channel, true);
    let config = selecting(channel);

    assert_eq!(
        confirm_codex_channel(channel, None, Some(&source)),
        Confirmation::Unselected
    );
    let detail = retry(confirm_codex_channel(channel, Some(&config), Some(&source)));
    assert!(detail.contains("Open"), "{detail}");
    assert!(row.exists() && !transcript_turns(channel));

    let meta_only = dir.path().join("meta-only.jsonl");
    std::fs::copy(&source.source.path, &meta_only).unwrap();
    let header = std::fs::read_to_string(&meta_only).unwrap();
    std::fs::write(&meta_only, format!("{}\n", header.lines().next().unwrap())).unwrap();
    let meta_only = SourceBinding {
        source: source_id_for("parent", &meta_only).unwrap(),
        ..source.clone()
    };
    retry(confirm_codex_channel(
        channel,
        Some(&config),
        Some(&meta_only),
    ));
    std::fs::remove_file(&meta_only.source.path).unwrap();
    retry(confirm_codex_channel(
        channel,
        Some(&config),
        Some(&meta_only),
    ));
    assert!(row.exists() && !transcript_turns(channel));

    answer(&source);
    assert_eq!(
        confirm_codex_channel(channel, Some(&config), Some(&source)),
        Confirmation::Confirmed
    );
    let _confirmed = TestConfirmation::confirmed(channel);
    assert!(!row.exists(), "the ended turn's synthetic row retires");
}

/// After the turn ended, a pending start, an unfinished row or a held start lock each refuse with
/// nothing retired; once Legacy released all three the same call confirms.
#[test]
fn codex_confirmation_retires_nothing_while_legacy_holds_an_unfinished_turn() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let channel = 64_700_302;
    let source = open_rollout(dir.path(), channel);
    answer(&source);
    let config = selecting(channel);
    let record = pending(channel);
    persist(&record).unwrap();
    let row = save_row(channel, false);

    let detail = retry(confirm_codex_channel(channel, Some(&config), Some(&source)));
    assert!(detail.contains("pending start"), "{detail}");
    let durable = records::<TuiDirectPendingStart>(root(), &ProviderKind::Codex, channel);
    assert_eq!(durable.unwrap().len(), 1, "the pending start is kept");
    assert!(pending_synthetic_start_present("codex", channel) && row.exists());
    delete(&record);
    let detail = retry(confirm_codex_channel(channel, Some(&config), Some(&source)));
    assert!(detail.contains("no terminal delivery"), "{detail}");
    assert!(row.exists() && !transcript_turns(channel));

    save_row(channel, true);
    let lock = channel_lock("codex", channel);
    let held = lock.try_lock().unwrap();
    let detail = retry(confirm_codex_channel(channel, Some(&config), Some(&source)));
    assert!(detail.contains("start claim"), "{detail}");
    assert!(row.exists() && !transcript_turns(channel));
    drop(held);

    assert_eq!(
        confirm_codex_channel(channel, Some(&config), Some(&source)),
        Confirmation::Confirmed
    );
    let _confirmed = TestConfirmation::confirmed(channel);
    assert!(!row.exists());
}

/// A record longer than one read budget still lets the rollout reach its turn end, so a long tool
/// output never keeps the takeover turn open.
#[test]
fn codex_confirmation_reads_past_a_record_longer_than_one_read() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let channel = 64_700_303;
    let source = open_rollout(dir.path(), channel);
    let output = "x".repeat(3 << 20);
    let long = json!({"type":"response_item","payload":{"type":"function_call_output",
        "call_id":"c1","output":output}});
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source.source.path)
        .unwrap();
    writeln!(file, "{long}").unwrap();
    drop(file);
    answer(&source);

    assert_eq!(
        confirm_codex_channel(channel, Some(&selecting(channel)), Some(&source)),
        Confirmation::Confirmed
    );
    let _confirmed = TestConfirmation::confirmed(channel);
}
