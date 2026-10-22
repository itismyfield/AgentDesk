use super::super::{
    ApplyResult, QuestionResult,
    workflow::{Effect, Workflow},
};
use super::*;
use std::{collections::BTreeSet, io::Write, path::PathBuf};

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
    fn effects(&self) -> BTreeSet<String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(error) => panic!("read fake durable sink: {error}"),
        }
    }
    fn effect(&mut self, operation: &'static str, key: &SettlementKey) -> Result<(), String> {
        self.calls.push(operation);
        if operation == "wake" && self.fail_wake {
            return Err("crash after durable release, before wake/ack".into());
        }
        let mut effects = self.effects();
        effects.insert(format!(
            "{operation}:{}",
            serde_json::to_string(key).unwrap()
        ));
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
impl CustodySink for FakeSink {
    fn release(&mut self, key: &SettlementKey, _: &IntentRef) -> Result<(), String> {
        self.effect("release", key)
    }
    fn wake(&mut self, key: &SettlementKey, _: &IntentRef) -> Result<(), String> {
        self.effect("wake", key)
    }
}

#[test]
fn workflow_restart_after_applied_receipt_releases_once() {
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
    let settled = release(&reopened, &recovered.consent, &mut sink).unwrap();
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
    release(&reopened, &recovered.consent, &mut sink).unwrap();
    assert!(
        sink.calls.is_empty(),
        "settlement_ack_suppresses_duplicate_effects_contract"
    );
    assert_eq!(sink.effects().len(), 2);
}

#[test]
fn settlement_crash_after_release_replays_the_same_key_before_acknowledging() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("consents");
    let store = ConsentStore::new(Some(directory.clone()));
    let applied = applied(&store);
    let mut sink = FakeSink::new(root.path().join("sink.json"));
    sink.fail_wake = true;
    let failed = release(&store, &applied, &mut sink);
    assert!(matches!(failed, Err(ref error) if error.contains("crash after durable release")));
    assert_eq!(
        store.unresolved().unwrap().len(),
        1,
        "settlement_never_acknowledges_before_effects_contract"
    );
    assert_eq!(sink.effects().len(), 1);
    let store = ConsentStore::new(Some(directory));
    let mut sink = FakeSink::new(root.path().join("sink.json"));
    release(&store, &applied, &mut sink).unwrap();
    assert_eq!(
        sink.effects().len(),
        2,
        "settlement_crash_replays_idempotent_key_contract"
    );
    assert!(store.unresolved().unwrap().is_empty());
}

#[test]
fn settlement_requires_a_durable_receipt_and_keeps_new_preview_intents_held() {
    let root = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(root.path().join("consents")));
    let pending = store.hold(question("first"), 100).unwrap().into_consent();
    let mut sink = FakeSink::new(root.path().join("sink.json"));
    assert!(
        release(&store, &pending, &mut sink).is_err(),
        "settlement_receipt_before_release_contract"
    );
    assert!(sink.calls.is_empty());
    let accepted = store.decide(&pending, 1, true, 101).unwrap();
    assert!(release(&store, &accepted, &mut sink).is_err());
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
    let settled = release(&store, &applied, &mut sink).unwrap();
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
            br#"{"schema":2,"future":true}"#.to_vec(),
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

#[test]
fn settlement_ack_failure_replays_completed_effects_without_duplicate_payloads() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("consents");
    let store = ConsentStore::new(Some(directory.clone()));
    let applied = applied(&store);
    let marker = directory.join(format!("{}.pending", durable::key(&applied.key).unwrap()));
    let mut sink = FakeSink::new(root.path().join("sink.json"));
    sink.pending_on_wake = Some(marker.clone());
    assert!(
        release(&store, &applied, &mut sink).is_err(),
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
    release(&reopened, &applied, &mut sink).unwrap();
    assert_eq!(
        sink.effects().len(),
        2,
        "settlement_completed_effect_replay_is_idempotent_contract"
    );
    assert!(reopened.unresolved().unwrap().is_empty());
}
