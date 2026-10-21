use super::*;
use crate::services::discord::tui_direct_pending_start as pending;
use crate::services::provider::ProviderKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const CHANNEL: u64 = 7_186_300_100;
const LOGICAL: &str = "clear-waiting-private-n1";

struct Rig {
    _worker: std::sync::MutexGuard<'static, ()>,
    _env: crate::config::TestEnvVarGuard,
    _env_lock: std::sync::MutexGuard<'static, ()>,
    temp: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        let worker = pending::tests::worker_test_lock();
        let env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        reset_present_for_tests();
        CLEARED
            .lock()
            .unwrap()
            .retain(|(_, channel, _)| *channel != CHANNEL);
        Self {
            _worker: worker,
            _env: env,
            _env_lock: env_lock,
            temp,
        }
    }

    fn record(&self, anchor: u64) -> TuiDirectPendingStart {
        TuiDirectPendingStart {
            provider: "claude".into(),
            channel_id: CHANNEL,
            tmux_session_name: LOGICAL.into(),
            prompt_text: format!("old waiting input {anchor}"),
            anchor_message_id: anchor,
            lease_relay_owner: "bridge_adapter".into(),
            lease_runtime_kind: Some("claude_tui".into()),
            lease_turn_id: Some(format!("original-external-{anchor}")),
            lease_session_key: Some("claude/private/clear-waiting".into()),
            captured_source: Some((
                self.temp
                    .path()
                    .join("original.jsonl")
                    .to_string_lossy()
                    .into(),
                17,
            )),
            ..Default::default()
        }
    }

    fn path(&self, record: &TuiDirectPendingStart) -> PathBuf {
        let root = root().unwrap();
        assert!(
            root.starts_with(self.temp.path()),
            "fixture must stay in its private root"
        );
        root.join(format!("{}.json", record.file_stem()))
    }

    fn discard(&self, excluded: &BTreeSet<MessageId>) -> Result<Vec<MessageId>, String> {
        discard_legacy_waiting("claude", CHANNEL, LOGICAL, excluded)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        BEFORE_UNLINK.with(|slot| *slot.borrow_mut() = None);
        CLEARED
            .lock()
            .unwrap()
            .retain(|(_, channel, _)| *channel != CHANNEL);
        reset_present_for_tests();
    }
}

fn claimed_row(record: &TuiDirectPendingStart) -> inflight::InflightTurnState {
    let mut row = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        record.channel_id,
        None,
        1,
        record.anchor_message_id,
        record.anchor_message_id + 1,
        record.prompt_text.clone(),
        Some(format!("native-{}", record.anchor_message_id)),
        Some(record.tmux_session_name.clone()),
        record
            .captured_source
            .as_ref()
            .map(|source| source.0.clone()),
        None,
        17,
    );
    row.session_key = record.lease_session_key.clone();
    row.external_turn_id = record.lease_turn_id.clone();
    row.turn_source = inflight::TurnSource::ExternalInput;
    row
}

#[test]
fn clear_discards_only_exact_legacy_waiting_and_preserves_proved_or_active_inputs() {
    let rig = Rig::new();
    let selected = rig.record(101);
    let active = rig.record(102);
    let modern = TuiDirectPendingStart {
        hosted_source_checked: true,
        ..rig.record(103)
    };
    let pinned = TuiDirectPendingStart {
        hosted_source: Some(
            crate::services::tui_prompt_dedupe::hosted_source::HostedSourcePin {
                provider: "claude".into(),
                channel_id: CHANNEL,
                logical_key: LOGICAL.into(),
                execution_nonce: "11111111111111111111111111111111".into(),
                hosted_admission_required: true,
                source: crate::services::tui_o::shadow::SourceId {
                    session_id: "original-native".into(),
                    path: PathBuf::from(&selected.captured_source.as_ref().unwrap().0),
                    dev: 1,
                    ino: 2,
                },
            },
        ),
        ..rig.record(104)
    };
    let n2 = TuiDirectPendingStart {
        tmux_session_name: "clear-waiting-private-n2".into(),
        captured_source: Some((
            rig.temp.path().join("n2.jsonl").to_string_lossy().into(),
            29,
        )),
        ..rig.record(105)
    };
    let other_provider = TuiDirectPendingStart {
        provider: "codex".into(),
        ..rig.record(106)
    };
    let other_channel = TuiDirectPendingStart {
        channel_id: CHANNEL + 1,
        ..rig.record(107)
    };
    let uncaptured = TuiDirectPendingStart {
        captured_source: None,
        ..rig.record(108)
    };
    let zero_anchor = rig.record(0);
    let preserved = [
        active,
        modern,
        pinned,
        n2,
        other_provider,
        other_channel,
        uncaptured,
        zero_anchor,
    ];
    persist(&selected).unwrap();
    let before: Vec<_> = preserved
        .iter()
        .map(|record| {
            persist(record).unwrap();
            (rig.path(record), std::fs::read(rig.path(record)).unwrap())
        })
        .collect();
    assert_eq!(
        load_all().len(),
        before.len() + 1,
        "all fixtures reached the durable store"
    );
    assert_eq!(
        rig.discard(&BTreeSet::from([MessageId::new(102)])).unwrap(),
        vec![MessageId::new(101)]
    );
    assert!(
        !rig.path(&selected).exists(),
        "the selected old waiting JSON must be removed"
    );
    assert!(cleared(&selected.key()));
    for (path, bytes) in before {
        assert_eq!(
            std::fs::read(path).unwrap(),
            bytes,
            "preserved pending must remain byte-identical"
        );
    }
    for record in &preserved {
        assert!(
            !cleared(&record.key()),
            "a preserved pending must not be revoked"
        );
    }
    assert_eq!(load_all().len(), preserved.len());
}

#[test]
fn cleared_legacy_waiting_rejects_late_writers_and_preserves_a_valid_n2_row() {
    let rig = Rig::new();
    let mut old = rig.record(201);
    persist(&old).unwrap();
    assert_eq!(
        rig.discard(&BTreeSet::new()).unwrap(),
        vec![MessageId::new(201)]
    );
    reset_present_for_tests();
    assert!(
        load_all().is_empty(),
        "a restart scan must have no discarded input to restore"
    );
    update_claim_attempt_count(&mut old, 7);
    assert!(
        load_all().is_empty(),
        "a late attempt update must not recreate the discarded JSON"
    );
    assert!(
        persist(&old).is_err(),
        "a late persist must reject a discarded anchor"
    );
    assert!(!pending_synthetic_start_present("claude", CHANNEL));
    assert!(load_all().is_empty());
    let old_row = claimed_row(&old);
    assert!(inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL).is_none());
    assert!(
        !save_row_unless_retired(&old_row).unwrap(),
        "a discarded anchor must not save even into an empty slot"
    );
    assert!(inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL).is_none());

    let next = rig.record(202);
    let mut next_row = claimed_row(&next);
    next_row.hosted_record_id = Some(902);
    next_row.hosted_execution_nonce = Some("22222222222222222222222222222222".into());
    assert!(
        save_row_unless_retired(&next_row).unwrap(),
        "the next valid anchor remains claimable"
    );
    let stored = inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL).unwrap();
    assert_eq!(stored.user_msg_id, 202);
    assert!(!save_row_unless_retired(&old_row).unwrap());
    assert_eq!(
        serde_json::to_value(
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL).unwrap()
        )
        .unwrap(),
        serde_json::to_value(stored).unwrap(),
        "a late N1 save must preserve the complete current N2 row"
    );
    assert!(take_retired(&old));
    assert!(
        take_retired(&old),
        "clear revocation must not be consumed by one worker probe"
    );
    assert!(!take_retired(&next));
    assert!(load_all().is_empty());
}

fn finalized_view(
    views: Arc<AtomicUsize>,
    pause: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
) -> ViewFn {
    Box::new(move |_shared, _record| {
        let views = views.clone();
        let pause = pause.clone();
        Box::pin(async move {
            views.fetch_add(1, Ordering::SeqCst);
            if let Some((entered, resume)) = pause {
                entered.notify_one();
                resume.notified().await;
            }
            Some(PriorTurnObservation {
                view: PriorTurnView {
                    inflight_present: false,
                    inflight_is_own_anchor: false,
                    mailbox_blocking_turn_present: false,
                    mailbox_turn_is_own_anchor: false,
                    runtime_binding_present: true,
                },
                foreign_inflight_identity: None,
            })
        })
    })
}

fn recording_claim(calls: Arc<AtomicUsize>) -> ClaimFn {
    Box::new(move |_shared, _record| {
        let calls = calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            true
        })
    })
}

fn recording_abort(calls: Arc<AtomicUsize>) -> AbortCleanupFn {
    Box::new(move |_shared, _record, _foreign| {
        let calls = calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
        })
    })
}

#[tokio::test(flavor = "current_thread")]
async fn clear_stops_an_entered_worker_and_repeated_old_worker_restore_before_any_claim() {
    let rig = Rig::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mut old = rig.record(301);
    old.generation = shared.restart.current_generation;
    persist(&old).unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let views = Arc::new(AtomicUsize::new(0));
    let claims = Arc::new(AtomicUsize::new(0));
    let aborts = Arc::new(AtomicUsize::new(0));
    let worker = pending::run_worker(
        shared.clone(),
        old.clone(),
        finalized_view(views.clone(), Some((entered.clone(), resume.clone()))),
        recording_claim(claims.clone()),
        recording_abort(aborts.clone()),
        Box::new(|_shared, _record| Box::pin(async { ReclaimStaleForeignOutcome::None })),
    );
    let controller = async {
        entered.notified().await;
        assert_eq!(
            views.load(Ordering::SeqCst),
            1,
            "the actual worker entered its wait observation"
        );
        assert_eq!(
            rig.discard(&BTreeSet::new()).unwrap(),
            vec![MessageId::new(301)]
        );
        resume.notify_one();
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(worker, controller);
    })
    .await
    .expect("the cleared worker must stand down without waiting for the backstop");
    assert_eq!(
        claims.load(Ordering::SeqCst),
        0,
        "the entered old worker must not claim after clear"
    );
    assert_eq!(aborts.load(Ordering::SeqCst), 0);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        pending::run_worker(
            shared,
            old,
            finalized_view(views.clone(), None),
            recording_claim(claims.clone()),
            recording_abort(aborts.clone()),
            Box::new(|_shared, _record| Box::pin(async { ReclaimStaleForeignOutcome::None })),
        ),
    )
    .await
    .expect("a second in-memory restore must also stand down");
    assert_eq!(
        views.load(Ordering::SeqCst),
        2,
        "both actual workers reached the production retired gate"
    );
    assert_eq!(claims.load(Ordering::SeqCst), 0);
    assert_eq!(aborts.load(Ordering::SeqCst), 0);
    assert!(load_all().is_empty());
    assert!(!pending_synthetic_start_present("claude", CHANNEL));
}

#[test]
fn clear_reports_unknown_for_malformed_or_unreadable_pending_inventory() {
    for unreadable in [false, true] {
        let rig = Rig::new();
        let selected = rig.record(401);
        persist(&selected).unwrap();
        let bad = rig.path(&rig.record(499));
        if unreadable {
            std::fs::create_dir(&bad).unwrap();
        } else {
            std::fs::write(&bad, b"{malformed").unwrap();
        }
        assert!(
            rig.discard(&BTreeSet::new()).is_err(),
            "failed inventory cannot report a known discard count"
        );
        assert!(
            !rig.path(&selected).exists(),
            "a valid sibling still reaches the real discard path"
        );
        assert!(bad.exists());
    }
    let rig = Rig::new();
    let store = root().unwrap();
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    std::fs::write(&store, "not a directory").unwrap();
    assert!(
        rig.discard(&BTreeSet::new()).is_err(),
        "a failed directory read cannot become successful zero"
    );
    assert!(store.is_file());
}

#[test]
fn clear_unlink_failure_reports_unknown_and_keeps_late_writers_revoked() {
    let rig = Rig::new();
    let control = rig.record(501);
    persist(&control).unwrap();
    assert_eq!(
        rig.discard(&BTreeSet::new()).unwrap(),
        vec![MessageId::new(501)]
    );
    let mut old = rig.record(502);
    persist(&old).unwrap();
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let calls = hook_calls.clone();
    BEFORE_UNLINK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |path: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            std::fs::remove_file(path).unwrap();
            std::fs::create_dir(path).unwrap();
        }))
    });
    assert!(
        rig.discard(&BTreeSet::new()).is_err(),
        "a real remove_file failure cannot report a known count"
    );
    assert_eq!(
        hook_calls.load(Ordering::SeqCst),
        1,
        "the real unlink boundary was reached"
    );
    assert!(rig.path(&old).is_dir());
    assert!(cleared(&old.key()));
    assert!(persist(&old).is_err());
    update_claim_attempt_count(&mut old, 9);
    assert!(
        rig.path(&old).is_dir(),
        "a late writer must not replace the failed-unlink entry"
    );
    assert!(take_retired(&old));
    assert!(take_retired(&old));
}
