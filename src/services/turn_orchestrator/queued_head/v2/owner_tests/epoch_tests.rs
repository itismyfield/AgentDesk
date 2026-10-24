use super::*;

#[test]
fn bootstrap_excludes_preparation_and_payload_commit_is_idempotent() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(727410);
    owner.enqueue_fixture(&item(410)).unwrap();
    let key = owner.reserve(attempt(410, "bootstrap"), payload()).unwrap();
    let bytes = serde_json::to_value(&key).unwrap();
    let writer = owner
        .arm_bootstrap(&key, 1000)
        .unwrap()
        .consume(&key)
        .unwrap();
    owner.tick(100_000).unwrap();
    assert!(
        owner.snapshot.retained_inputs.is_empty(),
        "bootstrap cannot consume confirmation time"
    );
    assert!(
        owner
            .snapshot
            .head_injection
            .as_ref()
            .unwrap()
            .confirmation_deadline_ms
            .is_none()
    );
    owner.payload_started(&key, 100_000).unwrap();
    owner.payload_started(&key, 900_000).unwrap();
    assert_eq!(
        owner
            .snapshot
            .head_injection
            .as_ref()
            .unwrap()
            .confirmation_deadline_ms,
        Some(130_000),
        "payload deadline is committed once"
    );
    assert_eq!(
        serde_json::to_value(owner.active_attempt().unwrap()).unwrap(),
        bytes,
        "payload phase cannot change attempt identity"
    );
    owner.tick(129_999).unwrap();
    assert!(owner.snapshot.head_injection.is_some());
    owner.tick(130_000).unwrap();
    assert_eq!(owner.snapshot.retained_inputs.len(), 1);
    drop(writer);
}

#[test]
fn bootstrap_restart_preserves_raw_and_never_rearms_without_a_deadline() {
    let _root = TestRuntimeRootGuard::new();
    for (channel, started) in [(727411, false), (727412, true)] {
        let mut owner = owner(channel);
        owner.enqueue_fixture(&item(channel)).unwrap();
        let key = owner
            .reserve(attempt(channel, "restart-bootstrap"), payload())
            .unwrap();
        let writer = owner
            .arm_bootstrap(&key, 1000)
            .unwrap()
            .consume(&key)
            .unwrap();
        if started {
            owner.payload_started(&key, 10_000).unwrap();
        }
        let original =
            serde_json::to_value(&owner.snapshot.head_injection.as_ref().unwrap().original)
                .unwrap();
        drop(writer);
        drop(owner);
        let mut restored = super::owner(channel);
        assert!(
            restored.arm_bootstrap(&key, 50_000).is_err(),
            "restart never infers NotSent from missing deadline"
        );
        assert_eq!(
            serde_json::to_value(&restored.snapshot.head_injection.as_ref().unwrap().original)
                .unwrap(),
            original
        );
        restored.tick(50_000).unwrap();
        assert_eq!(restored.snapshot.retained_inputs.len(), 1);
        assert_eq!(
            serde_json::to_value(&restored.snapshot.retained_inputs[0].input.original).unwrap(),
            original
        );
    }
}

#[test]
fn physical_supplement_preserves_logical_lease_across_epoch_and_labels() {
    let _root = TestRuntimeRootGuard::new();
    let mut a = owner(727413);
    let mut b = owner(727414);
    a.enqueue_fixture(&item(413)).unwrap();
    b.enqueue_fixture(&item(414)).unwrap();
    let ka = a.reserve(attempt(413, "logical-a"), payload()).unwrap();
    let kb = b.reserve(attempt(414, "logical-b"), payload()).unwrap();
    let mut wa = a.arm_bootstrap(&ka, 1).unwrap().consume(&ka).unwrap();
    let mut wb = b.arm_bootstrap(&kb, 1).unwrap().consume(&kb).unwrap();
    wa.add_physical("same server/pane/pid".into()).unwrap();
    assert!(
        wb.add_physical("same server/pane/pid".into()).is_err(),
        "different labels cannot overlap the same physical pane"
    );
    a.payload_started(&ka, 1).unwrap();
    a.tick(30_001).unwrap();
    assert!(
        wb.add_physical("same server/pane/pid".into()).is_err(),
        "retaining a head cannot release its writer"
    );
    drop(wa);
    wb.add_physical("same server/pane/pid".into()).unwrap();
    drop(wb);
}
