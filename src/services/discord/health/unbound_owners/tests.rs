use super::*;
use crate::services::discord::make_shared_data_for_tests;

#[tokio::test]
async fn snapshot_preserves_every_runtime_and_ignores_only_duplicate_arcs() {
    let registry = HealthRegistry::new();
    let first = make_shared_data_for_tests();
    let second = make_shared_data_for_tests();
    let standby = make_shared_data_for_tests();
    registry.register("codex".into(), first.clone()).await;
    let initial = registry.unbound_runtime_snapshot().await;
    registry
        .register_worker("codex".into(), first.clone())
        .await;
    let duplicate = registry.unbound_runtime_snapshot().await;
    assert_eq!(duplicate.generation, initial.generation);
    assert_eq!(duplicate.entries.len(), 1);

    registry
        .register_worker("codex".into(), second.clone())
        .await;
    registry
        .register_standby("claude".into(), standby.clone())
        .await;
    let snapshot = registry.unbound_runtime_snapshot().await;
    assert_eq!(snapshot.generation, initial.generation + 2);
    assert_eq!(snapshot.entries.len(), 3);
    for ((name, actual), (expected_name, expected)) in
        snapshot
            .entries
            .iter()
            .zip([("codex", &first), ("codex", &second), ("claude", &standby)])
    {
        assert_eq!(name, expected_name);
        assert!(Arc::ptr_eq(actual, expected));
    }
    let all = registry.all_registered_shared().await;
    assert_eq!(all.len(), snapshot.entries.len());
    assert!(
        all.iter()
            .zip(&snapshot.entries)
            .all(|(runtime, (_, snap))| Arc::ptr_eq(runtime, snap))
    );
}

#[tokio::test]
async fn membership_change_rejects_old_callback_and_current_callback_holds_lock() {
    let registry = HealthRegistry::new();
    registry
        .register("codex".into(), make_shared_data_for_tests())
        .await;
    let old = registry.unbound_runtime_snapshot().await;
    registry
        .register("codex".into(), make_shared_data_for_tests())
        .await;
    let mut calls = 0;
    assert_eq!(
        registry.with_unbound_membership(old.generation, || calls += 1),
        Err("membership")
    );
    assert_eq!(calls, 0);

    let current = registry.unbound_runtime_snapshot().await;
    assert_eq!(
        registry.with_unbound_membership(current.generation, || {
            assert!(registry.providers.try_lock().is_err());
            calls += 1;
            7
        }),
        Ok(7)
    );
    assert_eq!(calls, 1);
    assert!(registry.providers.try_lock().is_ok());
}

#[tokio::test]
async fn busy_membership_lock_never_calls_publication() {
    let registry = HealthRegistry::new();
    registry
        .register("codex".into(), make_shared_data_for_tests())
        .await;
    let snapshot = registry.unbound_runtime_snapshot().await;
    let guard = registry.providers.lock().await;
    let mut calls = 0;
    assert_eq!(
        registry.with_unbound_membership(snapshot.generation, || calls += 1),
        Err("providers_busy")
    );
    assert_eq!(calls, 0);
    drop(guard);
    assert_eq!(
        registry.with_unbound_membership(snapshot.generation, || ()),
        Ok(())
    );
}
