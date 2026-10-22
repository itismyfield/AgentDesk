use super::*;

struct Fixture {
    _root: tempfile::TempDir,
    root: PathBuf,
    path: PathBuf,
    row: InflightTurnState,
}

impl Fixture {
    fn new(owner: RelayOwnerKind) -> Self {
        let root = tempfile::tempdir().unwrap();
        let runtime_root = root.path().to_path_buf();
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            41_249_001,
            None,
            7,
            11,
            12,
            "relay".into(),
            Some("session".into()),
            Some("rewind-test".into()),
            Some("/tmp/rewind-output".into()),
            None,
            8,
        );
        row.started_at = "2000-01-01 00:00:00".into();
        row.updated_at = row.started_at.clone();
        row.turn_nonce = Some("rewind-owner-cas".into());
        row.born_generation = 1;
        row.save_generation = 3;
        row.full_response = "durable delivered body".into();
        row.response_sent_offset = row.full_response.len();
        row.status_message_id = Some(91);
        row.set_relay_owner_kind(owner);
        let path = inflight_state_path(&runtime_root, &ProviderKind::Claude, row.channel_id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        Self {
            _root: root,
            root: runtime_root,
            path,
            row,
        }
    }

    fn seed(&self, row: &InflightTurnState) -> Vec<u8> {
        self.seed_raw(serde_json::to_value(row).unwrap())
    }

    fn seed_raw(&self, mut raw: serde_json::Value) -> Vec<u8> {
        raw["herdr_before_start_stop"] = serde_json::json!({
            "protocol": 1,
            "kind": "unsubmitted_before_start_stop",
            "turn_nonce": "rewind-owner-cas",
            "admitted_generation": 2,
            "basis": "no_attempt",
            "notice": "pending",
            "future": {"nested": [1, null]}
        });
        let bytes = serde_json::to_vec_pretty(&raw).unwrap();
        crate::services::discord::runtime_store::atomic_write(
            &self.path,
            std::str::from_utf8(&bytes).unwrap(),
        )
        .unwrap();
        bytes
    }

    fn loaded(&self) -> InflightTurnState {
        load_inflight_state_unlocked(&self.path).unwrap()
    }

    fn assert_rejected(&self, desired: &InflightTurnState, durable: &InflightTurnState) {
        let before = self.seed(durable);
        assert!(
            !save_inflight_delivery_rewind_if_matches_identity_in_root(
                &self.root,
                desired,
                StreamRelayAuthority::from_state(&self.row),
                InflightDeliveryRewindReason::MissingWatcherReclaim,
            )
            .unwrap()
        );
        assert_eq!(fs::read(&self.path).unwrap(), before);
    }

    fn desired(&self) -> InflightTurnState {
        let mut desired = self.row.clone();
        desired.full_response = "stale error guidance".into();
        desired.response_sent_offset = 0;
        desired.last_offset = 12;
        desired.set_relay_owner_kind(RelayOwnerKind::None);
        desired
    }
}

#[test]
fn rewind_rejects_rebind_owner_without_writing() {
    let f = Fixture::new(RelayOwnerKind::None);
    let desired = f.desired();
    let mut rebound = f.row.clone();
    rebound.set_relay_owner_kind(RelayOwnerKind::Watcher);
    let before = f.seed(&rebound);

    assert!(
        !save_inflight_delivery_rewind_if_matches_identity_in_root(
            &f.root,
            &desired,
            StreamRelayAuthority::from_state(&f.row),
            InflightDeliveryRewindReason::TerminalErrorReset,
        )
        .unwrap(),
        "a same-identity rebind must reject the stale bridge rewind"
    );
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn rewind_rejects_stale_owner_without_writing() {
    let f = Fixture::new(RelayOwnerKind::Watcher);
    let mut durable = f.row.clone();
    durable.set_relay_owner_kind(RelayOwnerKind::StandbyRelay);
    durable.full_response = "new relay body".into();
    durable.response_sent_offset = 4;
    durable.last_offset = 20;
    durable.save_generation += 1;
    durable.updated_at = "2000-01-02 00:00:00".into();
    f.assert_rejected(&f.desired(), &durable);
}

#[test]
fn rewind_applies_when_expected_owner_matches() {
    let mut f = Fixture::new(RelayOwnerKind::Watcher);
    f.row.watcher_owner_channel_id = Some(77);
    let before: serde_json::Value = serde_json::from_slice(&f.seed(&f.row)).unwrap();
    let mut desired = f.desired();
    desired.last_offset = 5;
    desired.status_message_id = None;
    desired.watcher_owner_channel_id = Some(99);

    assert!(
        save_inflight_delivery_rewind_if_matches_identity_in_root(
            &f.root,
            &desired,
            StreamRelayAuthority::from_state(&f.row),
            InflightDeliveryRewindReason::MissingWatcherReclaim,
        )
        .unwrap()
    );

    let updated = f.loaded();
    assert_eq!(updated.full_response, desired.full_response);
    assert_eq!(updated.response_sent_offset, 0);
    assert_eq!(updated.last_offset, f.row.last_offset);
    assert_eq!(updated.effective_relay_owner_kind(), RelayOwnerKind::None);
    assert!(!updated.watcher_owns_live_relay);
    assert_eq!(updated.watcher_owner_channel_id, Some(77));
    assert_eq!(updated.status_message_id, f.row.status_message_id);
    assert_eq!(updated.save_generation, f.row.save_generation + 1);
    assert_ne!(updated.updated_at, f.row.updated_at);
    let after: serde_json::Value = serde_json::from_slice(&fs::read(&f.path).unwrap()).unwrap();
    assert_eq!(
        after["herdr_before_start_stop"],
        before["herdr_before_start_stop"]
    );
}

#[test]
fn terminal_reset_preserves_matching_bridge_authority() {
    let f = Fixture::new(RelayOwnerKind::None);
    let mut durable = f.row.clone();
    durable.save_generation += 5;
    durable.status_message_id = Some(92);
    f.seed(&durable);
    let desired = f.desired();

    assert!(
        save_inflight_delivery_rewind_if_matches_identity_in_root(
            &f.root,
            &desired,
            StreamRelayAuthority::from_state(&f.row),
            InflightDeliveryRewindReason::TerminalErrorReset,
        )
        .unwrap()
    );
    let updated = f.loaded();
    assert_eq!(updated.full_response, desired.full_response);
    assert_eq!(updated.response_sent_offset, 0);
    assert_eq!(updated.last_offset, desired.last_offset);
    assert_eq!(updated.status_message_id, durable.status_message_id);
    assert_eq!(updated.save_generation, durable.save_generation + 1);
    assert_eq!(updated.effective_relay_owner_kind(), RelayOwnerKind::None);
}

#[test]
fn rewind_legacy_watcher_authority_matches() {
    let f = Fixture::new(RelayOwnerKind::Watcher);
    let mut raw = serde_json::to_value(&f.row).unwrap();
    raw.as_object_mut().unwrap().remove("relay_owner_kind");
    f.seed_raw(raw);
    let mut desired = f.loaded();
    assert_eq!(desired.relay_owner_kind, RelayOwnerKind::None);
    assert!(desired.watcher_owns_live_relay);
    assert_eq!(
        desired.effective_relay_owner_kind(),
        RelayOwnerKind::Watcher
    );
    let expected_owner = StreamRelayAuthority::from_state(&desired);
    desired.set_relay_owner_kind(RelayOwnerKind::None);
    desired.full_response = "legacy watcher error guidance".into();
    desired.response_sent_offset = 0;
    f.seed(&f.row);

    assert!(
        save_inflight_delivery_rewind_if_matches_identity_in_root(
            &f.root,
            &desired,
            expected_owner,
            InflightDeliveryRewindReason::TerminalErrorReset,
        )
        .unwrap()
    );
    let updated = f.loaded();
    assert_eq!(updated.full_response, desired.full_response);
    assert_eq!(updated.response_sent_offset, 0);
    assert_eq!(updated.effective_relay_owner_kind(), RelayOwnerKind::None);
}

#[test]
fn rewind_rejects_unknown_owner_without_writing() {
    let f = Fixture::new(RelayOwnerKind::None);
    let mut raw = serde_json::to_value(&f.row).unwrap();
    raw["relay_owner_kind"] = "future_relay".into();
    let before = f.seed_raw(raw);
    assert_eq!(
        f.loaded().effective_relay_owner_kind(),
        RelayOwnerKind::Unknown
    );

    assert!(
        !save_inflight_delivery_rewind_if_matches_identity_in_root(
            &f.root,
            &f.desired(),
            StreamRelayAuthority::from_state(&f.row),
            InflightDeliveryRewindReason::TerminalErrorReset,
        )
        .unwrap()
    );
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn rewind_rejects_owner_channel_change() {
    let f = Fixture::new(RelayOwnerKind::Watcher);
    for channel in [Some(77), None, Some(0)] {
        let mut durable = f.row.clone();
        durable.watcher_owner_channel_id = channel;
        f.assert_rejected(&f.desired(), &durable);
    }
}

#[test]
fn rewind_rejects_legacy_bool_change() {
    let f = Fixture::new(RelayOwnerKind::Watcher);
    let mut durable = f.row.clone();
    durable.watcher_owns_live_relay = false;
    assert_eq!(
        durable.effective_relay_owner_kind(),
        RelayOwnerKind::Watcher
    );
    f.assert_rejected(&f.desired(), &durable);
}

#[test]
fn rewind_identity_and_terminal_guards_win() {
    let f = Fixture::new(RelayOwnerKind::None);
    for axis in 0..5 {
        let mut durable = f.row.clone();
        match axis {
            0 => durable.user_msg_id += 1,
            1 => durable.started_at.push('1'),
            2 => durable.tmux_session_name = Some("replacement-pane".into()),
            3 => durable.turn_start_offset = Some(9),
            4 => durable.terminal_delivery_committed = true,
            _ => unreachable!(),
        }
        f.assert_rejected(&f.desired(), &durable);
    }
}

#[test]
fn rewind_observes_owner_after_lock_contention() {
    use crate::services::discord::inflight::store::boot_observation::Probe;
    use std::sync::mpsc::{self, TryRecvError};
    use std::time::{Duration, Instant};

    let f = Fixture::new(RelayOwnerKind::Watcher);
    f.seed(&f.row);
    let lock = lock_inflight_state_path(&f.path).unwrap();
    let (_release, latch) = tokio::sync::oneshot::channel();
    let probe = Probe::new(f.root.clone(), latch);
    let root = f.root.clone();
    let desired = f.desired();
    let expected_owner = StreamRelayAuthority::from_state(&f.row);
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = save_inflight_delivery_rewind_if_matches_identity_in_root(
            &root,
            &desired,
            expected_owner,
            InflightDeliveryRewindReason::MissingWatcherReclaim,
        );
        sender.send(result).unwrap();
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let observed = loop {
        let attempts = probe
            .counts()
            .sources
            .get(&f.path)
            .copied()
            .unwrap_or_default();
        if attempts[0] == 1 {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::yield_now();
    };
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    let mut durable = f.row.clone();
    durable.set_relay_owner_kind(RelayOwnerKind::StandbyRelay);
    durable.full_response = "third owner delivery frontier".into();
    durable.response_sent_offset = 6;
    durable.save_generation += 1;
    let before = f.seed(&durable);
    drop(lock);

    let result = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
    assert!(observed, "worker never attempted the held canonical lock");
    assert!(
        !result.unwrap(),
        "rewind must compare the post-lock durable owner"
    );
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn rewind_rejects_stale_already_reclaimed_owner() {
    let f = Fixture::new(RelayOwnerKind::Watcher);
    let mut durable = f.row.clone();
    durable.set_relay_owner_kind(RelayOwnerKind::None);
    durable.full_response = "new bridge body".into();
    durable.response_sent_offset = 4;
    f.assert_rejected(&f.desired(), &durable);
}
