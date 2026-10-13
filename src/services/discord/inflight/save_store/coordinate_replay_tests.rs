//! The coordinate replay RMW is the only writer of the coordinate group, re-checks its plan under
//! the lock, and general writers can neither add, drop nor resurrect that group.

use super::coordinate_replay::{
    CoordinateChange, CoordinateExpectation, CoordinateRefusal, CoordinateRmwOutcome,
    CoordinateSourceWitness, admit_source_patch, apply_coordinate_change,
};
use super::*;
use crate::services::discord::inflight::episode_guard::lock_inflight_episode_in_root;
use crate::services::discord::inflight::model::{
    CoordinateInstallPhase, InstalledSourceWitness, RawTurnWitness,
};

pub(super) const CHANNEL: u64 = 5_925_100;

/// A Watcher-owned missing-normalized Codex row with a nonzero prior normalized cursor.
pub(super) fn legacy_row(channel_id: u64) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        None,
        7,
        5_925_001,
        5_925_002,
        "prompt".to_string(),
        Some("session-5925".to_string()),
        Some("tmux-5925".to_string()),
        Some("/missing/5925/normalized.jsonl".to_string()),
        None,
        40,
    );
    state.turn_start_offset = Some(0);
    state.turn_nonce = Some("nonce-5925".to_string());
    state.native_turn_id = Some("turn-5925".to_string());
    state.full_response = "FROZEN BODY".to_string();
    state.response_sent_offset = 7;
    state.last_watcher_relayed_offset = Some(40);
    state.set_relay_owner_kind(RelayOwnerKind::Watcher);
    state.replay_hold_reasons = vec!["receipt 9 held".to_string()];
    state
}

pub(super) fn row_path(root: &Path, channel_id: u64) -> PathBuf {
    inflight_state_path(root, &ProviderKind::Codex, channel_id)
}

pub(super) fn load(root: &Path, channel_id: u64) -> InflightTurnState {
    load_inflight_state_unlocked(&row_path(root, channel_id)).expect("durable row")
}

pub(super) fn seed(root: &Path, state: &InflightTurnState) -> InflightTurnState {
    save_inflight_state_in_root(root, state).expect("seed row");
    load(root, state.channel_id)
}

fn raw_witness() -> RawTurnWitness {
    RawTurnWitness {
        path: "/raw/rollout.jsonl".to_string(),
        file_identity: (1, 2),
        session_id: "session-5925".to_string(),
        native_turn_id: "turn-5925".to_string(),
        turn_start: 100,
        prefix_record_end: 300,
        prefix_len: 11,
    }
}

pub(super) fn installed_witness() -> InstalledSourceWitness {
    InstalledSourceWitness {
        output_path: "/managed/5925/normalized.jsonl".to_string(),
        file_identity: (3, 4),
        generation_mtime_ns: 77,
    }
}

pub(super) fn change(
    root: &Path,
    channel_id: u64,
    change: CoordinateChange,
) -> CoordinateRmwOutcome {
    let durable = load(root, channel_id);
    let expected = CoordinateExpectation::of(&durable).expect("named episode");
    let mut locked = lock_inflight_episode_in_root(
        root,
        &ProviderKind::Codex,
        channel_id,
        &InflightEpisodePin::from_state(&durable),
    )
    .expect("exact episode lock");
    let outcome = apply_coordinate_change(&mut locked, &expected, change);
    if outcome == CoordinateRmwOutcome::Saved {
        assert!(
            InflightEpisodePin::from_state(locked.state()).matches_state(&load(root, channel_id)),
            "the lock holder adopts the persisted row, so its rebuilt pin names the durable episode"
        );
    }
    outcome
}

/// Prepare then Install through the RMW; returns the durable row after the install.
pub(super) fn install(root: &Path, channel_id: u64) -> InflightTurnState {
    let prepare = CoordinateChange::Prepare {
        install_id: "install-1".to_string(),
        raw: raw_witness(),
    };
    assert_eq!(
        change(root, channel_id, prepare),
        CoordinateRmwOutcome::Saved
    );
    let install = CoordinateChange::Install {
        install_id: "install-1".to_string(),
        installed: installed_witness(),
    };
    assert_eq!(
        change(root, channel_id, install),
        CoordinateRmwOutcome::Saved
    );
    load(root, channel_id)
}

fn coordinate_holds(state: &InflightTurnState) -> Vec<&str> {
    state
        .replay_hold_reasons
        .iter()
        .map(String::as_str)
        .filter(|reason| reason.starts_with(COORDINATE_HOLD_PREFIX))
        .collect()
}

#[test]
fn coordinate_rmw_prepares_installs_and_releases_one_exact_episode() {
    let root = tempfile::tempdir().unwrap();
    seed(root.path(), &legacy_row(CHANNEL));

    let prepare = CoordinateChange::Prepare {
        install_id: "install-1".to_string(),
        raw: raw_witness(),
    };
    assert_eq!(
        change(root.path(), CHANNEL, prepare),
        CoordinateRmwOutcome::Saved
    );
    let prepared = load(root.path(), CHANNEL);
    let marker = prepared.legacy_replay_coordinate.as_ref().expect("marker");
    assert_eq!(marker.phase, CoordinateInstallPhase::Preparing);
    assert_eq!(
        marker.prior.last_offset, 40,
        "the prior normalized coordinate is named"
    );
    assert_eq!(
        prepared.last_offset, 40,
        "preparing does not move the prior source cursor"
    );
    assert_eq!(coordinate_holds(&prepared), ["p5925_coordinate:pending"]);

    let install = CoordinateChange::Install {
        install_id: "install-1".to_string(),
        installed: installed_witness(),
    };
    assert_eq!(
        change(root.path(), CHANNEL, install),
        CoordinateRmwOutcome::Saved
    );
    let installed = load(root.path(), CHANNEL);
    assert_eq!(
        installed.output_path.as_deref(),
        Some("/managed/5925/normalized.jsonl")
    );
    assert_eq!(
        (installed.last_offset, installed.turn_start_offset),
        (0, Some(0))
    );
    assert_eq!(installed.last_watcher_relayed_offset, None);
    assert_eq!(installed.last_watcher_relayed_generation_mtime_ns, None);
    let marker = installed.legacy_replay_coordinate.as_ref().expect("marker");
    assert_eq!(marker.phase, CoordinateInstallPhase::Installed);
    assert_eq!(marker.prior.last_offset, 40);
    assert_eq!(coordinate_holds(&installed), ["p5925_coordinate:pending"]);

    assert_eq!(
        change(root.path(), CHANNEL, CoordinateChange::Release),
        CoordinateRmwOutcome::Saved
    );
    let released = load(root.path(), CHANNEL);
    assert!(coordinate_holds(&released).is_empty());
    assert_eq!(
        released.replay_hold_reasons,
        ["receipt 9 held"],
        "other namespaces stay"
    );
    assert_eq!(
        released
            .legacy_replay_coordinate
            .as_ref()
            .map(|marker| marker.phase),
        Some(CoordinateInstallPhase::Installed),
        "releasing the hold does not end the install's delivery obligation"
    );
    let mut marker_only = released.clone();
    marker_only.replay_hold_reasons.clear();
    assert!(
        marker_only.replay_rerun_blocked(),
        "an unfinished install forbids an input rerun even without a hold reason"
    );
}

#[test]
fn coordinate_rmw_refuses_a_plan_the_locked_row_no_longer_matches() {
    let root = tempfile::tempdir().unwrap();
    let durable = seed(root.path(), &legacy_row(CHANNEL + 1));
    let path = row_path(root.path(), CHANNEL + 1);
    let lock = |durable: &InflightTurnState| {
        lock_inflight_episode_in_root(
            root.path(),
            &ProviderKind::Codex,
            CHANNEL + 1,
            &InflightEpisodePin::from_state(durable),
        )
        .unwrap()
    };
    let hold = || CoordinateChange::Hold("incomplete_prefix");

    // A write between the scan snapshot and the lock makes the plan stale even when it leaves
    // the identity and seed alone.
    let stale = CoordinateExpectation::of(&durable).unwrap();
    let mut progressed = durable.clone();
    progressed.current_tool_line = Some("tool".to_string());
    let mut locked = lock(&durable);
    assert_eq!(
        locked.persist_verified_under_guard(&progressed, "test progress"),
        GuardedSaveOutcome::Saved
    );
    let before = fs::read(&path).unwrap();
    assert_eq!(
        apply_coordinate_change(&mut locked, &stale, hold()),
        CoordinateRmwOutcome::Refused(CoordinateRefusal::StaleSnapshot)
    );
    drop(locked);

    let current = load(root.path(), CHANNEL + 1);
    let mut seed_changed = current.clone();
    seed_changed.native_turn_id = Some("turn-other".to_string());
    let mut locked = lock(&current);
    assert_eq!(
        apply_coordinate_change(
            &mut locked,
            &CoordinateExpectation::of(&seed_changed).unwrap(),
            hold()
        ),
        CoordinateRmwOutcome::Refused(CoordinateRefusal::SeedChanged)
    );
    let install = CoordinateChange::Install {
        install_id: "install-1".to_string(),
        installed: installed_witness(),
    };
    assert_eq!(
        apply_coordinate_change(
            &mut locked,
            &CoordinateExpectation::of(&current).unwrap(),
            install
        ),
        CoordinateRmwOutcome::Refused(CoordinateRefusal::PhaseOrder),
        "an install needs its own prepared marker"
    );
    drop(locked);
    assert_eq!(fs::read(&path).unwrap(), before, "refusals write nothing");

    let mut unnamed = current.clone();
    unnamed.turn_nonce = Some("  ".to_string());
    assert_eq!(
        CoordinateExpectation::of(&unnamed),
        None,
        "no nonce, no replay"
    );

    let mut committed = current.clone();
    committed.terminal_delivery_committed = true;
    let committed = seed(root.path(), &committed);
    let mut locked = lock(&committed);
    assert_eq!(
        apply_coordinate_change(
            &mut locked,
            &CoordinateExpectation::of(&committed).unwrap(),
            hold()
        ),
        CoordinateRmwOutcome::Refused(CoordinateRefusal::TerminalCommitted)
    );
}

#[test]
fn general_writes_cannot_add_drop_or_resurrect_the_coordinate_group() {
    let root = tempfile::tempdir().unwrap();
    let channel = CHANNEL + 2;
    let unmarked = seed(root.path(), &legacy_row(channel));
    let path = row_path(root.path(), channel);
    let general_write = |snapshot: &InflightTurnState| {
        let durable = load(root.path(), channel);
        let mut locked = lock_inflight_episode_in_root(
            root.path(),
            &ProviderKind::Codex,
            channel,
            &InflightEpisodePin::from_state(&durable),
        )
        .unwrap();
        locked.persist_verified_under_guard(snapshot, "test general write")
    };

    // Adding: a general snapshot carrying a coordinate hold.
    let mut adds = unmarked.clone();
    adds.replay_hold_reasons
        .push("p5925_coordinate:pending".to_string());
    let before = fs::read(&path).unwrap();
    assert_eq!(general_write(&adds), GuardedSaveOutcome::AuthorityPinned);
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "refused general write leaves bytes untouched"
    );

    // Dropping: a pre-install snapshot written over the installed row.
    let installed = install(root.path(), channel);
    let before = fs::read(&path).unwrap();
    assert_eq!(
        general_write(&unmarked),
        GuardedSaveOutcome::AuthorityPinned
    );
    assert_eq!(fs::read(&path).unwrap(), before);

    // Resurrecting: a released hold coming back with a snapshot taken before the release.
    assert_eq!(
        change(root.path(), channel, CoordinateChange::Release),
        CoordinateRmwOutcome::Saved
    );
    let before = fs::read(&path).unwrap();
    assert_eq!(
        general_write(&installed),
        GuardedSaveOutcome::AuthorityPinned
    );
    assert_eq!(fs::read(&path).unwrap(), before);

    // Keeping the group as it is remains an ordinary write.
    let mut progress = load(root.path(), channel);
    progress.full_response.push_str(" TAIL");
    assert_eq!(general_write(&progress), GuardedSaveOutcome::Saved);
    let durable = load(root.path(), channel);
    assert_eq!(durable.full_response, "FROZEN BODY TAIL");
    assert!(durable.coordinate_group_eq(&progress));
}

#[test]
fn a_watcher_patch_from_the_replaced_source_is_refused_after_install() {
    let root = tempfile::tempdir().unwrap();
    let channel = CHANNEL + 3;
    let mut legacy0 = legacy_row(channel);
    legacy0.last_offset = 0;
    legacy0.last_watcher_relayed_offset = None;
    let before_install = seed(root.path(), &legacy0);
    let replaced_source = CoordinateSourceWitness::of(&before_install);
    assert_eq!(
        admit_source_patch(&before_install, &replaced_source),
        Ok(())
    );

    let installed = install(root.path(), channel);
    assert_eq!(
        admit_source_patch(&installed, &replaced_source),
        Err(CoordinateRefusal::SourceChanged),
        "a patch produced against the legacy0 source must not touch the installed source"
    );
    assert_eq!(
        admit_source_patch(&installed, &CoordinateSourceWitness::of(&installed)),
        Ok(())
    );
}
