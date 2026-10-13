//! Strict proof, recovery classification, hold tally and the claim-point recheck of the legacy
//! restart replay contract, driven by real rollout files and durable row shapes.

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::{Value, json};

use super::legacy_restart_replay::proof::{ProofRequest, prove};
use super::legacy_restart_replay::{
    ClaimRecheck, CoordinateHold, CoordinateRecoveryDisposition as D, NormalizedOutputProbe,
    ReplayTally, SourcePolicy, classify, probe_normalized_output, recheck_before_claim,
};
use crate::services::agent_protocol::{RuntimeHandoffKind, StreamMessage};
use crate::services::discord::inflight::InflightTurnState;
use crate::services::provider::ProviderKind;

fn session_meta(id: &str) -> Value {
    json!({"type": "session_meta", "payload": {"id": id}})
}
fn task_started(turn: &str) -> Value {
    json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": turn}})
}
fn turn_context(turn: &str) -> Value {
    json!({"type": "turn_context", "payload": {"turn_id": turn}})
}
fn user(text: &str) -> Value {
    json!({"type": "response_item", "payload": {"type": "message", "role": "user",
        "content": [{"type": "input_text", "text": text}]}})
}
fn assistant(text: &str) -> Value {
    json!({"type": "response_item", "payload": {"type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": text}]}})
}
fn task_complete(turn: &str) -> Value {
    json!({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": turn}})
}

/// OLD turn, then the current native turn's prompt, FROZEN, BODY, TAIL and its terminal.
fn restart_rollout() -> Vec<Value> {
    vec![
        session_meta("s1"),
        task_started("old"),
        user("prompt"),
        assistant("OLD\n"),
        task_complete("old"),
        task_started("cur"),
        turn_context("cur"),
        user("prompt"),
        assistant("FROZEN\n"),
        assistant("BODY 가나다\n"),
        assistant("TAIL\n"),
        task_complete("cur"),
    ]
}

fn write_records(path: &Path, records: &[Value]) -> Vec<u64> {
    let mut text = String::new();
    let mut ends = Vec::new();
    for record in records {
        text.push_str(&format!("{record}\n"));
        ends.push(text.len() as u64);
    }
    std::fs::write(path, text).unwrap();
    ends
}

fn request<'a>(path: &'a Path, composed: &'a str) -> ProofRequest<'a> {
    ProofRequest {
        raw_path: path,
        session_id: Some("s1"),
        native_turn_id: Some("cur"),
        composed,
    }
}

fn hold(path: &Path, composed: &str) -> CoordinateHold {
    match prove(&request(path, composed)) {
        Ok(plan) => panic!(
            "unexpected plan ending at {}",
            plan.witness.prefix_record_end
        ),
        Err(hold) => hold,
    }
}

#[cfg(unix)]
#[test]
fn proof_aligns_the_stored_prefix_in_the_selected_native_turn_and_hands_over_the_reader() {
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("rollout.jsonl");
    let ends = write_records(&raw, &restart_rollout());

    let mut plan = prove(&request(&raw, "FROZEN\nBODY 가나다\n")).expect("aligned");
    assert_eq!(
        plan.witness.turn_start, ends[4],
        "the selected turn opens at its own task_started"
    );
    assert_eq!(
        plan.witness.prefix_record_end, ends[9],
        "the BODY record crosses the prefix"
    );
    assert_eq!(plan.witness.native_turn_id, "cur");
    assert_eq!(plan.residual_text, "");
    let mut next = String::new();
    plan.reader.read_line(&mut next).unwrap();
    let tail = serde_json::from_str::<Value>(&next).unwrap();
    assert_eq!(tail, assistant("TAIL\n"));
    assert!(
        matches!(plan.decoder.decode(&tail).as_deref(), Some([StreamMessage::Text { content }]) if content == "TAIL\n"),
        "the handed-over decoder continues the same turn without a message-boundary reset"
    );

    // A boundary inside a Text event splits at the UTF-8 boundary and keeps the rest.
    let plan = prove(&request(&raw, "FROZEN\nBODY 가")).expect("aligned inside a text event");
    assert_eq!(plan.witness.prefix_record_end, ends[9]);
    assert_eq!(plan.residual_text, "나다\n");
}

#[cfg(unix)]
#[test]
fn proof_keeps_the_stored_native_turn_when_a_later_turn_repeats_the_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("rollout.jsonl");
    let mut records = restart_rollout();
    records.extend([task_started("next"), user("prompt"), assistant("OTHER\n")]);
    let ends = write_records(&raw, &records);
    let plan = prove(&request(&raw, "FROZEN\n")).expect("aligned");
    assert_eq!(plan.witness.turn_start, ends[4]);
}

#[cfg(unix)]
#[test]
fn incomplete_evidence_holds_until_the_record_is_complete() {
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("rollout.jsonl");
    let records = restart_rollout();
    let text: String = records[..9].iter().map(|r| format!("{r}\n")).collect();
    let body = format!("{}\n", records[9]);
    let split = body.find('가').unwrap() + 1;
    std::fs::write(&raw, [text.as_bytes(), &body.as_bytes()[..split]].concat()).unwrap();
    assert_eq!(
        hold(&raw, "FROZEN\nBODY 가나다\n"),
        CoordinateHold::AwaitEvidence("partial_record"),
        "a partial UTF-8 record is not read past"
    );

    let mut file = std::fs::OpenOptions::new().append(true).open(&raw).unwrap();
    file.write_all(&body.as_bytes()[split..]).unwrap();
    drop(file);
    assert!(
        prove(&request(&raw, "FROZEN\nBODY 가나다\n")).is_ok(),
        "completing the record converges"
    );

    write_records(&raw, &records[..9]);
    assert_eq!(
        hold(&raw, "FROZEN\nBODY 가나다\n"),
        CoordinateHold::AwaitEvidence("incomplete_prefix"),
        "EOF inside the prefix is not terminal evidence"
    );
    write_records(&raw, &records[..5]);
    assert_eq!(
        hold(&raw, "FROZEN\n"),
        CoordinateHold::AwaitEvidence("turn_not_written")
    );
    assert_eq!(
        hold(&dir.path().join("absent.jsonl"), "FROZEN\n"),
        CoordinateHold::AwaitEvidence("source_missing")
    );
}

#[cfg(unix)]
#[test]
fn unprovable_identity_or_prefix_needs_evidence_and_is_never_guessed() {
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("rollout.jsonl");
    let records = restart_rollout();
    write_records(&raw, &records);
    let prefix = "FROZEN\nBODY 가나다\n";

    let unnamed = ProofRequest {
        native_turn_id: None,
        ..request(&raw, prefix)
    };
    assert_eq!(
        prove(&unnamed).err(),
        Some(CoordinateHold::NeedsEvidence("native_turn_unknown"))
    );
    let foreign = ProofRequest {
        session_id: Some("s2"),
        ..request(&raw, prefix)
    };
    assert_eq!(
        prove(&foreign).err(),
        Some(CoordinateHold::NeedsEvidence("session_mismatch"))
    );
    assert_eq!(
        hold(&raw, "FROZEN\nOTHER\n"),
        CoordinateHold::NeedsEvidence("prefix_mismatch")
    );

    let mut malformed: Vec<String> = records.iter().map(|r| format!("{r}\n")).collect();
    malformed.insert(9, "{not json\n".to_string());
    std::fs::write(&raw, malformed.concat()).unwrap();
    assert_eq!(
        hold(&raw, prefix),
        CoordinateHold::NeedsEvidence("malformed_record")
    );

    let mut reopened = records[..9].to_vec();
    reopened.extend([task_started("cur"), assistant("BODY 가나다\n")]);
    write_records(&raw, &reopened);
    assert_eq!(
        hold(&raw, prefix),
        CoordinateHold::NeedsEvidence("ambiguous_turn")
    );

    let mut bounded = records[..9].to_vec();
    bounded.extend([task_started("next"), assistant("BODY 가나다\n")]);
    write_records(&raw, &bounded);
    assert_eq!(
        hold(&raw, prefix),
        CoordinateHold::NeedsEvidence("prefix_mismatch"),
        "another turn's start bounds the selected turn"
    );

    write_records(
        &raw,
        &[session_meta("s1"), user("prompt"), assistant("FROZEN\n")],
    );
    assert_eq!(
        hold(&raw, "FROZEN\n"),
        CoordinateHold::NeedsEvidence("turns_unnamed")
    );
}

fn codex_row() -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Codex,
        5_925_200,
        None,
        7,
        5_925_201,
        5_925_202,
        "prompt".to_string(),
        Some("s1".to_string()),
        Some("tmux-5925".to_string()),
        Some("/missing/5925/normalized.jsonl".to_string()),
        None,
        0,
    );
    state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
    state.turn_nonce = Some("nonce-5925".to_string());
    state.native_turn_id = Some("cur".to_string());
    state.turn_start_offset = Some(0);
    state
}

fn with_marker(state: &InflightTurnState, phase: &str, installed: bool) -> InflightTurnState {
    let mut row = serde_json::to_value(state).unwrap();
    row["legacy_replay_coordinate"] = json!({
        "version": 1,
        "install_id": "install-1",
        "phase": phase,
        "prior": {"output_path": state.output_path, "last_offset": 0, "turn_start_offset": 0},
        "installed": installed.then(|| json!({
            "output_path": "/managed/normalized.jsonl",
            "file_identity": [3, 4],
            "generation_mtime_ns": 7
        })),
    });
    serde_json::from_value(row).unwrap()
}

fn with_hold(state: &InflightTurnState, slug: &str) -> InflightTurnState {
    let mut state = state.clone();
    state
        .replay_hold_reasons
        .push(format!("p5925_coordinate:{slug}"));
    state
}

#[test]
fn classification_owns_only_coordinate_targets_and_yields_to_existing_authority() {
    use NormalizedOutputProbe::{Missing, Present, Unreadable};
    let legacy = SourcePolicy::Legacy;
    let row = codex_row();
    assert_eq!(
        classify(&row, Present, legacy),
        D::NotApplicable,
        "normal rows stay unowned"
    );
    let mut claude = row.clone();
    claude.provider = ProviderKind::Claude.as_str().to_string();
    claude.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    assert_eq!(classify(&claude, Missing, legacy), D::NotApplicable);

    assert_eq!(classify(&row, Missing, legacy), D::NeedsProof);
    assert_eq!(
        classify(&row, Unreadable, legacy),
        D::NeedsProof,
        "an I/O error is not empty"
    );
    let mut unnamed = row.clone();
    unnamed.native_turn_id = None;
    assert_eq!(classify(&unnamed, Missing, legacy), D::NeedsEvidence);
    assert_eq!(
        classify(&with_hold(&row, "prefix_mismatch"), Missing, legacy),
        D::NeedsEvidence
    );
    assert_eq!(
        classify(&with_hold(&row, "incomplete_prefix"), Missing, legacy),
        D::NeedsProof
    );
    assert_eq!(
        classify(&with_hold(&row, "pending"), Present, legacy),
        D::RetryInstall
    );
    assert_eq!(
        classify(&with_marker(&row, "preparing", false), Present, legacy),
        D::RetryInstall
    );
    let installed = with_marker(&row, "installed", true);
    assert_eq!(
        classify(&installed, Present, legacy),
        D::ResumeManagedSource,
        "an existing installed file is not a normal ready completion"
    );
    assert_eq!(classify(&installed, Missing, legacy), D::NeedsProof);

    assert_eq!(
        classify(&row, Missing, SourcePolicy::VerifiedUnavailable),
        D::RefusedByExistingAuthority
    );
    assert_eq!(
        classify(&row, Missing, SourcePolicy::VerifiedCurrent),
        D::RefusedByExistingAuthority
    );
    let mut committed = row.clone();
    committed.terminal_delivery_committed = true;
    assert_eq!(
        classify(&committed, Missing, legacy),
        D::RefusedByExistingAuthority
    );
    let mut admitted = row.clone();
    admitted.tui_terminal_kind =
        Some(crate::services::agent_protocol::NativeTerminalKind::Completed);
    assert_eq!(
        classify(&admitted, Missing, legacy),
        D::RefusedByExistingAuthority
    );

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("normalized.jsonl");
    std::fs::write(&file, b"").unwrap();
    assert_eq!(probe_normalized_output(file.to_str()), Present);
    assert_eq!(
        probe_normalized_output(dir.path().join("x").to_str()),
        Missing
    );
    assert_eq!(probe_normalized_output(dir.path().to_str()), Unreadable);
    #[cfg(unix)]
    assert_eq!(
        probe_normalized_output(file.join("child").to_str()),
        Unreadable,
        "a metadata error other than NotFound is never read as a missing or empty source"
    );
}

#[test]
fn held_attempts_are_neither_recoveries_nor_a_drained_backlog() {
    let mut tally = ReplayTally::default();
    tally.record(Ok(()));
    tally.record(Err(CoordinateHold::AwaitEvidence("partial_record")));
    tally.record(Err(CoordinateHold::NeedsEvidence("native_turn_unknown")));
    tally.record(Err(CoordinateHold::RetryInstall("persist_failed")));
    assert_eq!((tally.recovered, tally.held), (1, 3));
    assert!(!tally.backlog_drained());
    let mut drained = ReplayTally::default();
    drained.record(Ok(()));
    assert!(drained.backlog_drained());
}

#[test]
fn a_claim_rechecks_the_durable_row_after_the_scan() {
    use NormalizedOutputProbe::{Missing, Present};
    let legacy = SourcePolicy::Legacy;
    let scanned = codex_row();
    assert_eq!(
        recheck_before_claim(&scanned, Some(&scanned), Present, legacy),
        ClaimRecheck::Proceed
    );
    assert_eq!(
        recheck_before_claim(&scanned, None, Present, legacy),
        ClaimRecheck::Refuse("row_absent")
    );
    let preparing = with_hold(&with_marker(&scanned, "preparing", false), "pending");
    assert_eq!(
        recheck_before_claim(&scanned, Some(&preparing), Present, legacy),
        ClaimRecheck::Refuse("coordinate_changed"),
        "a coordinate install that started after the scan keeps the stale candidate off the claim"
    );
    assert_eq!(
        recheck_before_claim(&scanned, Some(&scanned), Missing, legacy),
        ClaimRecheck::Refuse("coordinate_recovery_target"),
        "a source that went missing after the scan belongs to coordinate recovery"
    );
    let mut successor = scanned.clone();
    successor.current_msg_id += 1;
    assert_eq!(
        recheck_before_claim(&scanned, Some(&successor), Present, legacy),
        ClaimRecheck::Refuse("episode_changed")
    );
}
