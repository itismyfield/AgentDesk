use super::*;
use serde_json::{Value, json};

pub(crate) fn seed(root: &Path, channel: u64) -> (InflightTurnState, Value) {
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        11,
        12,
        "answer".into(),
        Some("session".into()),
        Some("fixture-session".into()),
        Some("/tmp/alpha-output.jsonl".into()),
        None,
        0,
    );
    row.turn_nonce = Some("original-turn".into());
    row.full_response = "answer".into();
    row.response_sent_offset = row.full_response.len();
    let evidence = json!({
        "version": 1,
        "evidence_id": "alpha-evidence-a",
        "origin": {
            "episode_key": "original-episode",
            "provider": "claude", "channel_id": channel,
            "offset_authority_channel_id": channel,
            "open_obligation_id": 41,
            "source": {"session_id": "session", "tmux_session_name": "fixture-session",
                       "output_path": "/tmp/alpha-output.jsonl", "spawn_identity": null},
            "delivery_authority": "watcher_lease",
            "binding": "Unresolved"
        },
        "range_start": 0, "confirmed_end_exclusive": 128,
        "producer": "watcher_direct"
    });
    let mut value = serde_json::to_value(&row).unwrap();
    value["pending_circuit_evidence"] = json!([evidence.clone()]);
    let path = inflight_state_path(root, &ProviderKind::Claude, channel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    (row, evidence)
}

pub(crate) fn handed_off(root: &Path, channel: u64, evidence: &Value) -> bool {
    [
        "discord_circuit_evidence",
        "discord_circuit_evidence_escrow",
    ]
    .into_iter()
    .any(|store| {
        let dir = root
            .parent()
            .unwrap()
            .join(store)
            .join("claude")
            .join(channel.to_string());
        fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry.path().extension().is_some_and(|ext| ext == "json")
                    && fs::read(entry.path())
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                        .as_ref()
                        == Some(evidence)
            })
        })
    })
}

#[test]
fn alpha_abandon_handoffs_without_observer() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime/discord_inflight");
    let (row, evidence) = seed(&root, 57151);
    assert_eq!(
        request_inflight_abandon_if_matches_in_root(
            &root,
            &ProviderKind::Claude,
            row.channel_id,
            row.user_msg_id,
            "fixture-token",
        ),
        GuardedClearOutcome::Cleared
    );
    assert!(!inflight_state_path(&root, &ProviderKind::Claude, row.channel_id).exists());
    assert!(
        handed_off(&root, row.channel_id, &evidence),
        "abandon lost the pending proof before any observer tick"
    );
}

#[test]
fn alpha_no_retroactive_binding() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime/discord_inflight");
    let (row, mut evidence) = seed(&root, 57152);
    evidence["origin"]
        .as_object_mut()
        .unwrap()
        .remove("binding");
    let path = inflight_state_path(&root, &ProviderKind::Claude, row.channel_id);
    let mut value = serde_json::to_value(&row).unwrap();
    value["pending_circuit_evidence"] = json!([evidence]);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    fs::write(
        temp.path().join("beta-current-token.json"),
        r#"{"token":"00000000-0000-4000-8000-000000005715"}"#,
    )
    .unwrap();
    let restored = load_inflight_state_unlocked(&path).unwrap();
    let restored = serde_json::to_value(restored).unwrap();
    assert_eq!(
        restored["pending_circuit_evidence"][0]["origin"]["binding"], "Unresolved",
        "legacy/alpha evidence must never inherit the current beta token"
    );
}

fn abandon(root: &Path, row: &InflightTurnState) -> GuardedClearOutcome {
    request_inflight_abandon_if_matches_in_root(
        root,
        &ProviderKind::Claude,
        row.channel_id,
        row.user_msg_id,
        "fixture-token",
    )
}

#[test]
fn alpha_journal_failure_escrows() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime/discord_inflight");
    let (row, evidence) = seed(&root, 57154);
    fs::write(
        root.parent().unwrap().join("discord_circuit_evidence"),
        "blocked",
    )
    .unwrap();
    assert_eq!(abandon(&root, &row), GuardedClearOutcome::Cleared);
    assert!(handed_off(&root, row.channel_id, &evidence));
}

#[test]
fn alpha_storage_failure_preserves_row() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime/discord_inflight");
    let (row, evidence) = seed(&root, 57155);
    let path = inflight_state_path(&root, &ProviderKind::Claude, row.channel_id);
    let before = fs::read(&path).unwrap();
    for store in [
        "discord_circuit_evidence",
        "discord_circuit_evidence_escrow",
    ] {
        fs::write(root.parent().unwrap().join(store), "blocked").unwrap();
    }
    assert_eq!(abandon(&root, &row), GuardedClearOutcome::IoError);
    assert_eq!(fs::read(&path).unwrap(), before);
    for store in [
        "discord_circuit_evidence",
        "discord_circuit_evidence_escrow",
    ] {
        fs::remove_file(root.parent().unwrap().join(store)).unwrap();
    }
    assert_eq!(abandon(&root, &row), GuardedClearOutcome::Cleared);
    assert!(handed_off(&root, row.channel_id, &evidence));
}

#[test]
fn alpha_pending_carry_forward_keeps_origin() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime/discord_inflight");
    let (mut snapshot, evidence) = seed(&root, 57156);
    let path = inflight_state_path(&root, &ProviderKind::Claude, snapshot.channel_id);
    snapshot.current_msg_id = 33;
    let _lock = lock_inflight_state_path(&path).unwrap();
    persist_under_lock(&root, &path, &snapshot, "alpha_stale_snapshot").unwrap();
    let written: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(written["pending_circuit_evidence"], json!([evidence]));
    assert_eq!(written["current_msg_id"], 33);
}

#[test]
fn alpha_distinct_unresolved_evidence_retained() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime/discord_inflight");
    let (row, first) = seed(&root, 57157);
    let path = inflight_state_path(&root, &ProviderKind::Claude, row.channel_id);
    let mut second = first.clone();
    second["evidence_id"] = json!("alpha-evidence-b");
    second["confirmed_end_exclusive"] = json!(250);
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["pending_circuit_evidence"] = json!([first.clone(), second.clone()]);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(abandon(&root, &row), GuardedClearOutcome::Cleared);
    assert!(handed_off(&root, row.channel_id, &first));
    assert!(handed_off(&root, row.channel_id, &second));
}

#[test]
fn alpha_unknown_evidence_version_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime/discord_inflight");
    let (row, mut evidence) = seed(&root, 57158);
    evidence["version"] = json!(999);
    let path = inflight_state_path(&root, &ProviderKind::Claude, row.channel_id);
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["pending_circuit_evidence"] = json!([evidence]);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let before = fs::read(&path).unwrap();
    assert_eq!(abandon(&root, &row), GuardedClearOutcome::IoError);
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn alpha_origin_and_range_survive_path_reuse() {
    use crate::services::discord::relay_recovery::circuit_evidence::{
        CircuitCoordinateBinding, CircuitDeliveryEvidence, CircuitEvidenceOrigin,
        CircuitSourceDescriptor,
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime/discord_inflight");
    let (mut row, seed) = seed(&root, 57159);
    let output = temp.path().join("output.jsonl");
    row.output_path = Some(output.to_string_lossy().into_owned());
    row.last_offset = 300;
    let mut source: CircuitSourceDescriptor =
        serde_json::from_value(seed["origin"]["source"].clone()).unwrap();
    source.output_path = row.output_path.clone();
    fs::write(&output, "original output").unwrap();
    let origin = CircuitEvidenceOrigin::capture(&row, source, "watcher_lease", Some(41));
    let original_key = origin.episode_key.clone();
    assert!(
        CircuitDeliveryEvidence::from_success(
            origin.clone(),
            (64, 64),
            "watcher_direct",
            "display-time".into()
        )
        .is_none()
    );
    assert!(
        CircuitDeliveryEvidence::from_success(
            origin.clone(),
            (250, 64),
            "watcher_direct",
            "display-time".into()
        )
        .is_none()
    );
    let proof = CircuitDeliveryEvidence::from_success(
        origin.clone(),
        (64, 250),
        "watcher_direct",
        "display-time".into(),
    )
    .unwrap();
    assert_eq!(proof.confirmed_end_exclusive, 250);
    let zero = CircuitDeliveryEvidence::from_success(
        origin,
        (0, 128),
        "watcher_direct",
        "display-time".into(),
    )
    .unwrap();
    assert_eq!(zero.confirmed_end_exclusive, 128);
    row.current_msg_id = 44;
    fs::write(&output, "").unwrap();
    fs::write(&output, "replacement output").unwrap();
    assert_ne!(
        crate::services::discord::relay_recovery::circuit_evidence::episode_key(&row),
        original_key
    );
    let runtime = root.parent().unwrap();
    crate::services::discord::relay_recovery::circuit_evidence::handoff(runtime, &proof).unwrap();
    crate::services::discord::relay_recovery::circuit_evidence::handoff(runtime, &proof).unwrap();
    let directory = runtime.join("discord_circuit_evidence/claude/57159");
    let files: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .collect();
    assert_eq!(files.len(), 1);
    let restored: CircuitDeliveryEvidence =
        serde_json::from_slice(&fs::read(files[0].path()).unwrap()).unwrap();
    assert_eq!(restored, proof);
    assert_eq!(restored.origin.episode_key, original_key);
    assert_eq!(
        restored.origin.binding,
        CircuitCoordinateBinding::Unresolved
    );
}

#[test]
fn alpha_no_evidence_eviction() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime/discord_inflight");
    let (row, evidence) = seed(&root, 57160);
    let path = inflight_state_path(&root, &ProviderKind::Claude, row.channel_id);
    let proofs: Vec<_> = (0..40)
        .map(|index| {
            let mut proof = evidence.clone();
            proof["evidence_id"] = json!(format!("unresolved-{index}"));
            proof
        })
        .collect();
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["pending_circuit_evidence"] = json!(proofs);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(abandon(&root, &row), GuardedClearOutcome::Cleared);
    for proof in &proofs {
        assert!(
            handed_off(&root, row.channel_id, proof),
            "lost proof {}",
            proof["evidence_id"]
        );
    }
}

#[test]
fn alpha_conflicting_identity_cannot_fork_into_escrow() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime/discord_inflight");
    let (row, original) = seed(&root, 57161);
    assert_eq!(abandon(&root, &row), GuardedClearOutcome::Cleared);
    let (row, mut conflict) = seed(&root, 57161);
    conflict["confirmed_end_exclusive"] = json!(250);
    let path = inflight_state_path(&root, &ProviderKind::Claude, row.channel_id);
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["pending_circuit_evidence"] = json!([conflict]);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let before = fs::read(&path).unwrap();
    assert_eq!(abandon(&root, &row), GuardedClearOutcome::IoError);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(handed_off(&root, row.channel_id, &original));
}
