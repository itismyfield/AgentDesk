//! Rebind rollback may only undo the adoption it committed.

use super::*;
use crate::services::discord::inflight::{
    InflightEpisodePin, InflightTurnIdentity, LockedInflightEpisode, RelayOwnerKind,
    adopt_and_lock_inflight_episode, load_inflight_state,
    mark_readopted_from_inflight_if_identity_unchanged, save_inflight_state,
    touch_inflight_state_if_matches_identity,
};

#[derive(Clone, Copy, Debug)]
enum AdoptionPath {
    Episode,
    Identity,
}

fn env() -> (tempfile::TempDir, crate::config::TestEnvVarGuard) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let guard = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
    (tmp, guard)
}

fn seed(channel_id: u64) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        None,
        1,
        channel_id + 1,
        channel_id + 2,
        "rollback pin".to_string(),
        Some("session".to_string()),
        Some("AgentDesk-codex-original".to_string()),
        Some("/tmp/original.jsonl".to_string()),
        None,
        0,
    );
    row.turn_nonce = Some(format!("nonce-{channel_id}"));
    row.status_message_id = Some(channel_id + 3);
    row.set_relay_owner_kind(RelayOwnerKind::None);
    save_inflight_state(&row).expect("seed row");
    let row = load_inflight_state(&ProviderKind::Codex, channel_id).expect("load seed");
    assert!(!row.readopted_from_inflight && row.restart_mode.is_none());
    row
}

fn disk(channel_id: u64) -> InflightTurnState {
    load_inflight_state(&ProviderKind::Codex, channel_id).expect("durable row")
}

fn disk_bytes(channel_id: u64) -> String {
    let root = inflight_runtime_root().expect("inflight root");
    fs::read_to_string(inflight_state_path(&root, &ProviderKind::Codex, channel_id))
        .expect("row bytes")
}

/// Mirrors `adopt_coordinates`: returns the committed row and, on the episode
/// path, the still-held adoption guard.
fn adopt(
    path: AdoptionPath,
    durable: &InflightTurnState,
) -> (InflightTurnState, Option<LockedInflightEpisode>) {
    let expected = InflightTurnIdentity::from_state(durable);
    let mut local = durable.clone();
    local.tmux_session_name = Some("AgentDesk-codex-adopted".to_string());
    local.output_path = Some("/tmp/adopted.jsonl".to_string());
    local.set_relay_owner_kind(RelayOwnerKind::Watcher);
    match path {
        AdoptionPath::Episode => {
            let guard = adopt_and_lock_inflight_episode(
                &local,
                &expected,
                &InflightEpisodePin::from_state(durable),
                durable.turn_start_offset,
                None,
            )
            .expect("episode adoption");
            (guard.state().clone(), Some(guard))
        }
        AdoptionPath::Identity => (
            save_existing_inflight_rebind_adoption_committed(
                &local,
                &expected,
                durable.turn_start_offset,
                None,
            )
            .expect("identity adoption"),
            None,
        ),
    }
}

fn mark_readopted(
    path: AdoptionPath,
    committed: &InflightTurnState,
    guard: Option<LockedInflightEpisode>,
) {
    let outcome = match (path, guard) {
        (AdoptionPath::Episode, Some(mut guard)) => guard.mark_readopted_under_guard(),
        (AdoptionPath::Identity, None) => mark_readopted_from_inflight_if_identity_unchanged(
            &ProviderKind::Codex,
            committed.channel_id,
            &InflightTurnIdentity::from_state(committed),
        ),
        (path, guard) => panic!("{path:?} adoption guard mismatch: {}", guard.is_some()),
    };
    assert_eq!(outcome, GuardedSaveOutcome::Saved);
}

/// The rebind's own rollback: restore `original`, pinned to `committed`.
fn roll_back(
    path: AdoptionPath,
    original: &InflightTurnState,
    committed: &InflightTurnState,
) -> GuardedSaveOutcome {
    let episode =
        matches!(path, AdoptionPath::Episode).then(|| InflightEpisodePin::from_state(committed));
    restore_inflight_rebind_adoption_if_pinned(
        original,
        &InflightTurnIdentity::from_state(committed),
        episode.as_ref(),
        committed.turn_start_offset,
        None,
        committed,
    )
}

fn assert_restored(path: AdoptionPath, original: &InflightTurnState, outcome: GuardedSaveOutcome) {
    assert_eq!(outcome, GuardedSaveOutcome::Saved, "{path:?}");
    let row = disk(original.channel_id);
    assert_eq!(
        row.effective_relay_owner_kind(),
        RelayOwnerKind::None,
        "{path:?}"
    );
    assert_eq!(
        row.tmux_session_name, original.tmux_session_name,
        "{path:?}"
    );
    assert_eq!(row.output_path, original.output_path, "{path:?}");
}

fn stale_rollback_refuses_successor_adoption(path: AdoptionPath, channel_id: u64) {
    let (_tmp, _env) = env();
    let original = seed(channel_id);
    let (committed, guard) = adopt(path, &original);
    drop(guard);
    let (successor, guard) = adopt(path, &disk(channel_id));
    drop(guard);
    // A lone successor adoption is exactly one generation step and no marker.
    assert_eq!(successor.save_generation, committed.save_generation + 1);
    assert!(!successor.readopted_from_inflight);
    let before = disk_bytes(channel_id);

    let outcome = roll_back(path, &original, &committed);

    assert_eq!(outcome, GuardedSaveOutcome::SuccessorOwned, "{path:?}");
    let row = disk(channel_id);
    assert_eq!(row.effective_relay_owner_kind(), RelayOwnerKind::Watcher);
    assert_eq!(row.save_generation, successor.save_generation);
    assert_eq!(disk_bytes(channel_id), before);
}

#[test]
fn stale_rollback_refuses_successor_adoption_episode() {
    stale_rollback_refuses_successor_adoption(AdoptionPath::Episode, 6_214_100);
}

#[test]
fn stale_rollback_refuses_successor_adoption_identity() {
    stale_rollback_refuses_successor_adoption(AdoptionPath::Identity, 6_214_110);
}

fn stale_rollback_refuses_successor_after_own_marker(path: AdoptionPath, channel_id: u64) {
    let (_tmp, _env) = env();
    let original = seed(channel_id);
    let (committed, guard) = adopt(path, &original);
    mark_readopted(path, &committed, guard);
    let (successor, guard) = adopt(path, &disk(channel_id));
    drop(guard);
    assert_eq!(successor.save_generation, committed.save_generation + 2);
    let before = disk_bytes(channel_id);

    let outcome = roll_back(path, &original, &committed);

    assert_eq!(outcome, GuardedSaveOutcome::SuccessorOwned, "{path:?}");
    assert_eq!(
        disk(channel_id).effective_relay_owner_kind(),
        RelayOwnerKind::Watcher
    );
    assert_eq!(disk_bytes(channel_id), before);
}

#[test]
fn stale_rollback_refuses_successor_after_own_marker_episode() {
    stale_rollback_refuses_successor_after_own_marker(AdoptionPath::Episode, 6_214_200);
}

#[test]
fn stale_rollback_refuses_successor_after_own_marker_identity() {
    stale_rollback_refuses_successor_after_own_marker(AdoptionPath::Identity, 6_214_210);
}

fn rollback_restores_own_adoption(path: AdoptionPath, channel_id: u64) {
    let (_tmp, _env) = env();
    let original = seed(channel_id);
    let (committed, guard) = adopt(path, &original);
    drop(guard);

    assert_restored(path, &original, roll_back(path, &original, &committed));
}

#[test]
fn rollback_restores_own_adoption_episode() {
    rollback_restores_own_adoption(AdoptionPath::Episode, 6_214_300);
}

#[test]
fn rollback_restores_own_adoption_identity() {
    rollback_restores_own_adoption(AdoptionPath::Identity, 6_214_310);
}

fn rollback_restores_own_adoption_after_own_readoption_marker(path: AdoptionPath, channel_id: u64) {
    let (_tmp, _env) = env();
    let original = seed(channel_id);
    let (committed, guard) = adopt(path, &original);
    mark_readopted(path, &committed, guard);
    let marked = disk(channel_id);
    assert!(marked.readopted_from_inflight);

    assert_restored(path, &original, roll_back(path, &original, &committed));
}

#[test]
fn rollback_restores_own_adoption_after_own_readoption_marker_episode() {
    rollback_restores_own_adoption_after_own_readoption_marker(AdoptionPath::Episode, 6_214_400);
}

#[test]
fn rollback_restores_own_adoption_after_own_readoption_marker_identity() {
    rollback_restores_own_adoption_after_own_readoption_marker(AdoptionPath::Identity, 6_214_410);
}

fn rollback_refuses_after_foreign_generation_advance(path: AdoptionPath, channel_id: u64) {
    let (_tmp, _env) = env();
    let original = seed(channel_id);
    let (committed, guard) = adopt(path, &original);
    drop(guard);
    assert_eq!(
        touch_inflight_state_if_matches_identity(
            &ProviderKind::Codex,
            channel_id,
            &InflightTurnIdentity::from_state(&committed),
            "rollback_pin_tests",
        ),
        GuardedSaveOutcome::Saved
    );
    assert!(!disk(channel_id).readopted_from_inflight);

    // Fail-closed cost: an unrelated write in between keeps the adoption.
    let outcome = roll_back(path, &original, &committed);

    assert_eq!(outcome, GuardedSaveOutcome::SuccessorOwned, "{path:?}");
    assert_eq!(
        disk(channel_id).effective_relay_owner_kind(),
        RelayOwnerKind::Watcher
    );
}

#[test]
fn rollback_refuses_after_foreign_generation_advance_episode() {
    rollback_refuses_after_foreign_generation_advance(AdoptionPath::Episode, 6_214_500);
}

#[test]
fn rollback_refuses_after_foreign_generation_advance_identity() {
    rollback_refuses_after_foreign_generation_advance(AdoptionPath::Identity, 6_214_510);
}

#[test]
fn rollback_restores_after_generation_preserving_status_panel_write() {
    let (_tmp, _env) = env();
    let channel_id = 6_214_590;
    let original = seed(channel_id);
    let (committed, guard) = adopt(AdoptionPath::Identity, &original);
    drop(guard);
    crate::services::discord::status_panel_singleton_store::bind_if_owned(
        &ProviderKind::Codex,
        "rollback-pin-token",
        channel_id,
        channel_id + 3,
        Some(committed.status_panel_generation + 1),
    )
    .expect("status panel bind");
    let panel = disk(channel_id);
    assert_eq!(
        panel.status_panel_generation,
        committed.status_panel_generation + 1
    );
    assert_eq!(panel.save_generation, committed.save_generation);

    assert_restored(
        AdoptionPath::Identity,
        &original,
        roll_back(AdoptionPath::Identity, &original, &committed),
    );
}

#[test]
fn rollback_pin_refuses_rows_that_fail_to_serialize() {
    let (_tmp, _env) = env();
    let mut committed = seed(6_214_600);
    committed.save_generation = 7;
    let mut on_disk = committed.clone();
    on_disk.readopted_from_inflight = true;
    on_disk.save_generation = 8;
    assert!(rollback_pin_holds(&committed, &on_disk));

    // Beyond u64, `serde_json::to_value` fails; both sides failing is not equality.
    committed.claude_e_process_starttime = Some(u128::MAX);
    on_disk.claude_e_process_starttime = Some(u128::MAX);
    assert!(serde_json::to_value(&on_disk).is_err());
    assert!(!rollback_pin_holds(&committed, &on_disk));
    on_disk.claude_e_process_starttime = None;
    assert!(!rollback_pin_holds(&committed, &on_disk));
}

#[test]
fn rollback_pin_is_compared_under_the_adoption_flock() {
    let source = include_str!("identity_gate.rs");
    let start = source
        .find("fn lock_and_save_existing_inflight_rebind_adoption_impl_in_root(")
        .expect("W1 impl");
    let end = source[start..]
        .find("\n}\n")
        .map(|relative| start + relative)
        .expect("W1 impl end");
    let body = &source[start..end];
    let lock = body.find("lock_inflight_state_path(").expect("flock");
    let compare = body.find("rollback_pin_holds(").expect("pin comparison");
    let write = body.find("atomic_write(").expect("row write");
    assert!(lock < compare && compare < write);

    let entry = include_str!("rebind_adoption.rs");
    let start = entry
        .find("fn restore_inflight_rebind_adoption_if_pinned(")
        .expect("rollback entry");
    let end = entry[start..]
        .find("\n}\n")
        .map(|relative| start + relative)
        .expect("rollback entry end");
    let body = &entry[start..end];
    for unlocked_read in [
        "read_to_string",
        "load_inflight_state",
        "rollback_pin_holds(",
    ] {
        assert!(!body.contains(unlocked_read), "{unlocked_read}");
    }
}
