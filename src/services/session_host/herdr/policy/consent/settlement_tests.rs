use super::super::{
    ApplyResult, QuestionResult,
    workflow::{Effect, Workflow},
};
use super::*;
use std::{io::Write, path::PathBuf};

fn question(id: &str) -> super::super::Question {
    super::super::tests::question(id)
}

fn applied(store: &ConsentStore) -> Consent {
    let held = store.hold(question("first"), 100).unwrap().into_consent();
    let accepted = store.decide(&held, 1, true, 101).unwrap();
    store
        .record_apply(
            &accepted,
            ApplyResult {
                before: Some([1; 32]),
                after: Some([4; 32]),
                backup: Some("/fixture/backup".into()),
                detail: None,
            },
        )
        .unwrap()
}

struct FakeSink {
    path: PathBuf,
    calls: Vec<&'static str>,
    fail_wake: bool,
    pending_on_wake: Option<PathBuf>,
}

impl FakeSink {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            calls: Vec::new(),
            fail_wake: false,
            pending_on_wake: None,
        }
    }
    fn effects(&self) -> Vec<String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read fake durable sink: {error}"),
        }
    }
    fn effect(&mut self, operation: &'static str, key: &SettlementKey) -> Result<(), String> {
        self.calls.push(operation);
        if operation == "wake" && std::mem::take(&mut self.fail_wake) {
            return Err("crash after durable release, before wake/ack".into());
        }
        let mut effects = self.effects();
        let effect = format!(
            "{operation}:{}",
            serde_json::to_string(&(&key.store, &key.id)).unwrap()
        );
        if !effects.contains(&effect) {
            effects.push(effect);
        }
        let mut file = std::fs::File::create(&self.path).map_err(|error| error.to_string())?;
        file.write_all(&serde_json::to_vec(&effects).unwrap())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        if operation == "wake"
            && let Some(marker) = self.pending_on_wake.take()
        {
            std::fs::write(marker, b"interrupted acknowledgement")
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}
#[async_trait::async_trait]
impl CustodySink for FakeSink {
    async fn release(&mut self, key: &SettlementKey, _: &IntentRef) -> Result<(), String> {
        self.effect("release", key)
    }
    async fn wake(&mut self, key: &SettlementKey, _: &IntentRef) -> Result<(), String> {
        self.effect("wake", key)
    }
}

#[tokio::test]
async fn workflow_restart_after_applied_receipt_releases_once() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("consents");
    let store = ConsentStore::new(Some(directory.clone()));
    let applied = applied(&store);
    let reopened = ConsentStore::new(Some(directory.clone()));
    let recovered = Workflow(&reopened).recover(&applied.key).unwrap().unwrap();
    assert_eq!(
        recovered.effect,
        Effect::ReleaseThenWake,
        "workflow_applied_recovery_releases_contract"
    );
    assert_eq!(
        reopened.unresolved().unwrap().len(),
        1,
        "applied_receipt_is_not_settlement_contract"
    );
    let mut sink = FakeSink::new(root.path().join("sink.json"));
    let settled = release(&reopened, &recovered.consent, &mut sink)
        .await
        .unwrap();
    assert_eq!(
        sink.calls,
        ["release", "wake"],
        "settlement_effect_order_and_one_call_contract"
    );
    assert!(settled.unresolved_intents().is_empty());
    assert_eq!(sink.effects().len(), 2);
    let reopened = ConsentStore::new(Some(directory));
    assert!(
        reopened.unresolved().unwrap().is_empty(),
        "settled_intent_does_not_hold_after_restart_contract"
    );
    let recovered = Workflow(&reopened).recover(&applied.key).unwrap().unwrap();
    let mut sink = FakeSink::new(root.path().join("sink.json"));
    release(&reopened, &recovered.consent, &mut sink)
        .await
        .unwrap();
    assert!(
        sink.calls.is_empty(),
        "settlement_ack_suppresses_duplicate_effects_contract"
    );
    assert_eq!(sink.effects().len(), 2);
}

#[tokio::test]
async fn settlement_crash_after_release_replays_the_same_key_before_acknowledging() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("consents");
    let store = ConsentStore::new(Some(directory.clone()));
    let applied = applied(&store);
    let mut sink = FakeSink::new(root.path().join("sink.json"));
    sink.fail_wake = true;
    let failed = release(&store, &applied, &mut sink).await;
    assert!(matches!(failed, Err(ref error) if error.contains("crash after durable release")));
    assert_eq!(
        store.unresolved().unwrap().len(),
        1,
        "settlement_never_acknowledges_before_effects_contract"
    );
    assert_eq!(sink.effects().len(), 1);
    let store = ConsentStore::new(Some(directory));
    let mut sink = FakeSink::new(root.path().join("sink.json"));
    release(&store, &applied, &mut sink).await.unwrap();
    assert_eq!(
        sink.effects().len(),
        2,
        "settlement_crash_replays_idempotent_key_contract"
    );
    assert!(store.unresolved().unwrap().is_empty());
}

#[tokio::test]
async fn settlement_requires_a_durable_receipt_and_keeps_new_preview_intents_held() {
    let root = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(root.path().join("consents")));
    let pending = store.hold(question("first"), 100).unwrap().into_consent();
    let mut sink = FakeSink::new(root.path().join("sink.json"));
    assert!(
        release(&store, &pending, &mut sink).await.is_err(),
        "settlement_receipt_before_release_contract"
    );
    assert!(sink.calls.is_empty());
    let accepted = store.decide(&pending, 1, true, 101).unwrap();
    assert!(release(&store, &accepted, &mut sink).await.is_err());
    assert!(sink.calls.is_empty());
    let applied = store
        .record_apply(
            &accepted,
            ApplyResult {
                before: Some([1; 32]),
                after: Some([4; 32]),
                backup: None,
                detail: None,
            },
        )
        .unwrap();
    let mut revised = question("second");
    revised.preview.revision += 1;
    revised.preview.target_hash = [8; 32];
    assert!(matches!(
        store.hold(revised, 102).unwrap(),
        QuestionResult::RevisionChanged(_)
    ));
    let settled = release(&store, &applied, &mut sink).await.unwrap();
    assert_eq!(
        sink.calls,
        ["release", "wake"],
        "settlement_old_yes_never_releases_new_revision_contract"
    );
    assert_eq!(
        settled
            .unresolved_intents()
            .iter()
            .map(|intent| intent.id.as_str())
            .collect::<Vec<_>>(),
        ["second"],
        "settlement_old_yes_never_releases_new_revision_contract"
    );
    let second = settled.unresolved_intents().remove(0);
    assert!(store.acknowledge_discard(&pending, &second).is_ok());
    assert!(
        store.unresolved().unwrap().is_empty(),
        "settlement_verified_clear_resolves_only_exact_reference_contract"
    );
}

#[test]
fn consent_bulk_inventory_distinguishes_absence_unknown_newer_and_corruption() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("consents");
    let store = ConsentStore::new(Some(directory.clone()));
    assert!(store.load_all().unwrap().is_empty());
    assert!(
        ConsentStore::new(None).load_all().is_err(),
        "consent_bulk_unavailable_root_is_not_absence_contract"
    );
    assert!(!directory.exists(), "consent_bulk_absence_is_lazy_contract");
    let held = store.hold(question("first"), 100).unwrap().into_consent();
    let name = durable::key(&held.key).unwrap();
    let path = directory.join(format!("{name}.json"));
    let original = std::fs::read(&path).unwrap();
    for (bytes, expected) in [
        (b"{".to_vec(), "consent_snapshot_corrupt"),
        (
            br#"{"schema":3,"future":true}"#.to_vec(),
            "consent_snapshot_newer",
        ),
    ] {
        std::fs::write(&path, bytes).unwrap();
        let loaded = store.unresolved();
        assert!(
            loaded.is_err(),
            "consent_bulk_read_failure_is_not_empty_contract"
        );
        let error = loaded.unwrap_err();
        assert!(
            error.contains(expected),
            "consent_bulk_read_failure_is_not_empty_contract: {error}"
        );
        if expected.contains("newer") {
            assert!(!error.contains("corrupt"));
        }
    }
    std::fs::write(&path, &original).unwrap();
    let other = directory.join(format!("{}.json", "f".repeat(64)));
    std::fs::rename(&path, &other).unwrap();
    assert!(
        store
            .load_all()
            .unwrap_err()
            .contains("filename does not match"),
        "consent_bulk_filename_identity_contract"
    );
    std::fs::rename(&other, &path).unwrap();
    std::fs::write(directory.join(format!("{name}.pending")), b"interrupted").unwrap();
    assert!(
        store
            .load_all()
            .unwrap_err()
            .contains("policy_publication_pending"),
        "consent_bulk_unknown_marker_is_not_empty_contract"
    );
    std::fs::remove_file(directory.join(format!("{name}.pending"))).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.load_all().unwrap_err().contains("0600"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(store.load_all().unwrap(), [held]);
}

#[test]
fn consent_legacy_schema_defaults_to_unresolved_and_duplicate_scopes_are_separate() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("consents");
    let store = ConsentStore::new(Some(directory.clone()));
    let held = store.hold(question("first"), 100).unwrap().into_consent();
    let path = directory.join(format!("{}.json", durable::key(&held.key).unwrap()));
    let mut value = serde_json::to_value(&held).unwrap();
    value.as_object_mut().unwrap().remove("settlements");
    value.as_object_mut().unwrap().remove("intent_previews");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(
        store.unresolved().unwrap()[0].1,
        held.intents,
        "consent_legacy_unsettled_custody_survives_contract"
    );
    let mut second = question("first");
    second.key.node.push('x');
    store.hold(second, 100).unwrap();
    assert_eq!(
        store.unresolved().unwrap().len(),
        2,
        "consent_bulk_scope_collision_contract"
    );
}

#[tokio::test]
async fn settlement_ack_failure_replays_completed_effects_without_duplicate_payloads() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("consents");
    let store = ConsentStore::new(Some(directory.clone()));
    let applied = applied(&store);
    let marker = directory.join(format!("{}.pending", durable::key(&applied.key).unwrap()));
    let mut sink = FakeSink::new(root.path().join("sink.json"));
    sink.pending_on_wake = Some(marker.clone());
    assert!(
        release(&store, &applied, &mut sink).await.is_err(),
        "settlement_ack_failure_is_not_success_contract"
    );
    assert_eq!(sink.effects().len(), 2);
    assert!(
        store.unresolved().is_err(),
        "settlement_unknown_ack_preserves_fail_closed_contract"
    );
    let raw: Consent = serde_json::from_slice(
        &std::fs::read(directory.join(format!("{}.json", durable::key(&applied.key).unwrap())))
            .unwrap(),
    )
    .unwrap();
    assert!(
        raw.settlements.is_empty(),
        "settlement_ack_is_after_confirmed_effects_contract"
    );
    std::fs::remove_file(marker).unwrap();
    let reopened = ConsentStore::new(Some(directory));
    let mut sink = FakeSink::new(root.path().join("sink.json"));
    release(&reopened, &applied, &mut sink).await.unwrap();
    assert_eq!(
        sink.effects().len(),
        2,
        "settlement_completed_effect_replay_is_idempotent_contract"
    );
    assert!(reopened.unresolved().unwrap().is_empty());
}

#[tokio::test]
async fn settlement_incomplete_applied_receipt_is_corrupt_and_never_releases() {
    for missing in ["apply", "after"] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("consents");
        let store = ConsentStore::new(Some(directory.clone()));
        let valid = applied(&store);
        let path = directory.join(format!("{}.json", durable::key(&valid.key).unwrap()));
        let mut raw = serde_json::to_value(&valid).unwrap();
        if missing == "apply" {
            raw["apply"] = serde_json::Value::Null;
        } else {
            raw["apply"]["after"] = serde_json::Value::Null;
        }
        let expected: Consent = serde_json::from_value(raw.clone()).unwrap();
        let bytes = serde_json::to_vec(&raw).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let mut sink = FakeSink::new(root.path().join("sink.json"));
        let result = release(&store, &expected, &mut sink).await;
        assert!(
            matches!(&result, Err(error) if error.contains("consent_snapshot_corrupt")),
            "settlement_incomplete_receipt_never_authorizes_release_contract: {missing}: {result:?}"
        );
        assert!(
            sink.calls.is_empty(),
            "settlement_incomplete_receipt_effects_zero_contract: {missing}"
        );
        assert!(
            matches!(store.load_all(), Err(error) if error.contains("consent_snapshot_corrupt")),
            "settlement_incomplete_receipt_bulk_is_corrupt_contract: {missing}"
        );
        assert!(
            matches!(store.unresolved(), Err(error) if error.contains("consent_snapshot_corrupt")),
            "settlement_incomplete_receipt_never_becomes_empty_holds_contract: {missing}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "settlement_incomplete_receipt_preserves_snapshot_contract: {missing}"
        );
    }
}

impl CustodySinkFixture for FakeSink {
    fn effect_counts(&self, key: &SettlementKey) -> (usize, usize) {
        let encoded = serde_json::to_string(&(&key.store, &key.id)).unwrap();
        let effects = self.effects();
        (
            effects
                .iter()
                .filter(|effect| **effect == format!("release:{encoded}"))
                .count(),
            effects
                .iter()
                .filter(|effect| **effect == format!("wake:{encoded}"))
                .count(),
        )
    }

    fn fail_next_wake(&mut self) {
        self.fail_wake = true;
    }
}

#[tokio::test]
async fn custody_sink_durable_idempotence_conformance() {
    let root = tempfile::tempdir().unwrap();
    custody_sink_conformance(|| FakeSink::new(root.path().join("sink.json"))).await;
}

// The factory reopens the same durable fixture, including after an injected failure.
pub(crate) trait CustodySinkFixture: CustodySink {
    /// Counts total observable effects by (store, id) across all nonces; deduped calls do not count.
    fn effect_counts(&self, key: &SettlementKey) -> (usize, usize);
    /// Injects one failure before the next wake has an observable effect.
    fn fail_next_wake(&mut self);
}

pub(crate) async fn custody_sink_conformance<S: CustodySinkFixture>(make: impl Fn() -> S) {
    let intent = IntentRef {
        store: "conformance-custody".into(),
        id: "intent-1".into(),
        provider: "conformance-provider".into(),
        channel: 1,
        source_message: 2,
        fifo: 0,
    };
    let first = SettlementKey {
        nonce: "generation-1".into(),
        store: intent.store.clone(),
        id: intent.id.clone(),
    };
    let mut sink = make();
    for _ in 0..2 {
        sink.release(&first, &intent)
            .await
            .expect("conformance first release");
        sink.wake(&first, &intent)
            .await
            .expect("conformance first wake");
    }
    assert_eq!(
        sink.effect_counts(&first),
        (1, 1),
        "custody_conformance_repeated_key_has_one_effect_contract"
    );
    drop(sink);

    let mut sink = make();
    assert_eq!(
        sink.effect_counts(&first),
        (1, 1),
        "custody_conformance_effects_survive_recreation_contract"
    );
    sink.release(&first, &intent)
        .await
        .expect("conformance release after recreation");
    sink.wake(&first, &intent)
        .await
        .expect("conformance wake after recreation");
    assert_eq!(
        sink.effect_counts(&first),
        (1, 1),
        "custody_conformance_recreated_sink_does_not_repeat_payload_contract"
    );

    let another_nonce = SettlementKey {
        nonce: "generation-2".into(),
        ..first.clone()
    };
    // The receipt may change, but a custody reference still names the same payload.
    let _ = sink.release(&another_nonce, &intent).await;
    let _ = sink.wake(&another_nonce, &intent).await;
    assert_eq!(
        sink.effect_counts(&another_nonce),
        (1, 1),
        "custody_conformance_same_reference_never_repeats_across_nonces_contract"
    );

    let failed_intent = IntentRef {
        id: "intent-2".into(),
        ..intent.clone()
    };
    let failed = SettlementKey {
        id: failed_intent.id.clone(),
        ..first.clone()
    };
    sink.fail_next_wake();
    sink.release(&failed, &failed_intent)
        .await
        .expect("release before failure");
    assert!(
        sink.wake(&failed, &failed_intent).await.is_err(),
        "custody_conformance_injected_wake_failure_is_real_contract"
    );
    assert_eq!(
        sink.effect_counts(&failed),
        (1, 0),
        "custody_conformance_release_survives_lost_wake_contract"
    );
    drop(sink);
    let mut sink = make();
    sink.release(&failed, &failed_intent)
        .await
        .expect("failure replay release");
    sink.wake(&failed, &failed_intent)
        .await
        .expect("failure replay wake");
    assert_eq!(
        sink.effect_counts(&failed),
        (1, 1),
        "custody_conformance_failure_replay_has_one_effect_contract"
    );
    assert_eq!(
        sink.effect_counts(&first),
        (1, 1),
        "custody_conformance_distinct_reference_is_independent_contract"
    );
}

#[test]
fn consent_bulk_waits_for_live_publication_but_not_an_abandoned_marker() {
    use std::{sync::mpsc, time::Duration};
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("consents");
    let store = ConsentStore::new(Some(directory.clone()));
    let held = store
        .hold(question("live-writer"), 100)
        .unwrap()
        .into_consent();
    let name = durable::key(&held.key).unwrap();
    let marker = directory.join(format!("{name}.pending"));
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.join(format!("{name}.lock")))
        .unwrap();
    lock.lock().unwrap();
    std::fs::write(&marker, b"active publication").unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let reader_root = directory.clone();
    let reader = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        result_tx
            .send(ConsentStore::new(Some(reader_root)).load_all())
            .unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(
            result_rx.recv_timeout(Duration::from_millis(150)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "consent_bulk_live_marker_waits_for_writer_contract"
    );
    std::fs::remove_file(&marker).unwrap();
    drop(lock);
    let result = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    reader.join().unwrap();
    assert_eq!(
        result.unwrap(),
        [held],
        "consent_bulk_live_writer_finishes_without_failure_contract"
    );
    std::fs::write(marker, b"abandoned publication").unwrap();
    assert!(
        matches!(store.load_all(), Err(error) if error.contains("policy_publication_pending")),
        "consent_bulk_abandoned_marker_remains_failure_contract"
    );
}

#[tokio::test]
async fn workflow_new_generation_never_rereleases_an_unacknowledged_intent() {
    for fail_ack in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("consents");
        let store = ConsentStore::new(Some(directory.clone()));
        let applied = applied(&store);
        let marker = directory.join(format!("{}.pending", durable::key(&applied.key).unwrap()));
        let mut sink = FakeSink::new(root.path().join("sink.json"));
        if fail_ack {
            sink.pending_on_wake = Some(marker.clone());
        } else {
            sink.fail_wake = true;
        }
        assert!(release(&store, &applied, &mut sink).await.is_err());
        if fail_ack {
            std::fs::remove_file(&marker).unwrap();
        }
        let mut changed = question("changed");
        changed.preview.revision += 1;
        changed.preview.target_hash = [9; 32];
        let next_preview = changed.preview.clone();
        store.hold(changed, 120).unwrap();
        let mut retry_request = question("retry");
        retry_request.preview = next_preview.clone();
        retry_request.message = 0;
        let retry = Workflow(&store)
            .retry(retry_request, 1, Some(1), "one", 130)
            .unwrap();
        assert_eq!(
            retry.consent.nonce, applied.nonce,
            "workflow_unacknowledged_release_keeps_original_nonce_contract"
        );
        assert_eq!(
            retry.effect,
            Effect::ReleaseThenWake,
            "workflow_unacknowledged_release_replays_before_generation_contract"
        );
        assert!(retry.consent.history.is_empty());
        let settled = release(&store, &retry.consent, &mut sink).await.unwrap();
        let first = SettlementKey {
            nonce: applied.nonce.clone(),
            store: applied.intents[0].store.clone(),
            id: applied.intents[0].id.clone(),
        };
        assert_eq!(
            sink.effect_counts(&first),
            (1, 1),
            "workflow_gen1_replay_payload_once_contract"
        );
        assert!(settled.releasable_intents().is_empty());
        let mut open = question("open-generation");
        open.preview = next_preview;
        open.message = 0;
        let next = Workflow(&store)
            .retry(open, 1, Some(1), "one", 140)
            .unwrap()
            .consent;
        assert_ne!(
            next.nonce, applied.nonce,
            "workflow_generation_opens_after_old_release_ack_contract"
        );
        assert_eq!(next.history.len(), 1);
        let bound = store.bind_card(&next, 42).unwrap();
        let accepted = store.decide(&bound, 1, true, 141).unwrap();
        let applied_next = store
            .record_apply(
                &accepted,
                ApplyResult {
                    before: Some([9; 32]),
                    after: Some([8; 32]),
                    backup: None,
                    detail: None,
                },
            )
            .unwrap();
        release(&store, &applied_next, &mut sink).await.unwrap();
        assert_eq!(
            sink.effect_counts(&first),
            (1, 1),
            "workflow_old_intent_not_released_by_new_generation_contract"
        );
    }
}
