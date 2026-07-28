use super::*;
use crate::services::provider::ProviderKind;

const BRIDGE_ENTRY_CALLER: &str = "turn_bridge::spawn_turn_bridge::bridge_entry_test";

fn bridge_entry_state(channel_id: u64, user_msg_id: u64) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        Some("adk-4259-r4".to_string()),
        343_742_347_365_974_026,
        user_msg_id,
        18,
        "user prompt".to_string(),
        Some("session".to_string()),
        Some(format!("AgentDesk-codex-4259-r4-{user_msg_id}")),
        Some(format!("/tmp/AgentDesk-codex-4259-r4-{user_msg_id}.jsonl")),
        Some(format!("/tmp/AgentDesk-codex-4259-r4-{user_msg_id}.input")),
        512,
    )
}

fn state_path(root: &Path, channel_id: u64) -> PathBuf {
    inflight_state_path(root, &ProviderKind::Codex, channel_id)
}

#[test]
fn bridge_entry_guarded_save_persists_same_owner() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let mut owner = bridge_entry_state(4_259_401, 77_010);
    save_inflight_state_in_root(temp.path(), &owner).expect("seed owner row");

    owner.current_msg_id = 42;
    owner.full_response = "same-owner bridge entry".to_string();
    assert_eq!(
        save_inflight_state_if_identity_unchanged_in_root(
            temp.path(),
            &owner,
            BRIDGE_ENTRY_CALLER,
        ),
        GuardedSaveOutcome::Saved
    );

    let persisted: InflightTurnState = serde_json::from_slice(
        &fs::read(state_path(temp.path(), owner.channel_id)).expect("read saved owner row"),
    )
    .expect("parse saved owner row");
    assert_eq!(persisted.user_msg_id, 77_010);
    assert_eq!(persisted.current_msg_id, 42);
    assert_eq!(persisted.full_response, "same-owner bridge entry");
}

#[test]
fn bridge_entry_guarded_save_preserves_newer_owner_bytes_on_identity_mismatch() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let mut stale_owner = bridge_entry_state(4_259_402, 77_010);
    save_inflight_state_in_root(temp.path(), &stale_owner).expect("seed original owner row");

    let mut newer_owner = bridge_entry_state(stale_owner.channel_id, 99_999);
    newer_owner.full_response = "newer owner bytes".to_string();
    newer_owner.last_offset = 8_192;
    save_inflight_state_in_root(temp.path(), &newer_owner)
        .expect("replace disk row with newer owner");
    let path = state_path(temp.path(), stale_owner.channel_id);
    let newer_owner_bytes = fs::read(&path).expect("read newer owner bytes");

    stale_owner.current_msg_id = 42;
    stale_owner.full_response = "stale owner overwrite".to_string();
    assert_eq!(
        save_inflight_state_if_identity_unchanged_in_root(
            temp.path(),
            &stale_owner,
            BRIDGE_ENTRY_CALLER,
        ),
        GuardedSaveOutcome::IdentityMismatch
    );
    assert_eq!(
        fs::read(&path).expect("read row after declined stale save"),
        newer_owner_bytes,
        "identity mismatch must leave the newer owner's serialized row byte-for-byte unchanged"
    );
}

#[test]
fn bridge_entry_guarded_save_does_not_resurrect_a_deleted_row() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let mut stale_owner = bridge_entry_state(4_259_403, 77_010);
    save_inflight_state_in_root(temp.path(), &stale_owner).expect("seed owner row");
    let path = state_path(temp.path(), stale_owner.channel_id);
    fs::remove_file(&path).expect("delete durable owner row");

    stale_owner.current_msg_id = 42;
    assert_eq!(
        save_inflight_state_if_identity_unchanged_in_root(
            temp.path(),
            &stale_owner,
            BRIDGE_ENTRY_CALLER,
        ),
        GuardedSaveOutcome::Missing
    );
    assert!(
        !path.exists(),
        "missing guarded save must not recreate the deleted inflight row"
    );
}
