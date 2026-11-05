use super::*;
use serde_json::json;
const CHANNEL: u64 = 7787992;
fn seed() -> (std::path::PathBuf, InflightTurnState) {
    let root = inflight_runtime_root().unwrap();
    let path = inflight_state_path(&root, &ProviderKind::Claude, CHANNEL);
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        CHANNEL,
        Some("fixture".into()),
        7,
        7787001,
        900001,
        "original".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_nonce = Some("birth-one".into());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
    (path, row)
}
fn js_remove() -> serde_json::Value {
    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        crate::engine::ops::register_globals_with_supervisor_and_pg(
            &ctx,
            None,
            crate::supervisor::BridgeHandle::new(),
        )
        .unwrap();
        let out: String = ctx
            .eval(format!(
                "JSON.stringify(agentdesk.inflight.remove('claude','{CHANNEL}'))"
            ))
            .unwrap();
        serde_json::from_str(&out).unwrap()
    })
}
#[tokio::test]
async fn public_policy_remove_matches_nonzero_birth_once_and_missing_never_notifies() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (path, _) = seed();
    let observed = watch(&ProviderKind::Claude, CHANNEL).unwrap().unwrap();
    assert_eq!(js_remove()["ok"], true);
    assert!(!path.exists());
    assert!(observed.0.removed.load(Ordering::Acquire));
    observed.removed().await;
    assert_eq!(js_remove()["ok"], false);
    assert!(watch(&ProviderKind::Claude, CHANNEL).unwrap().is_none());
}
#[test]
fn mismatch_io_protected_and_other_birth_keep_notification_pending() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (path, mut row) = seed();
    let observed = watch(&ProviderKind::Claude, CHANNEL).unwrap().unwrap();
    let mut other = row.clone();
    other.turn_nonce = Some("successor".into());
    assert_ne!(
        crate::services::discord::inflight::clear_inflight_state_for_snapshot(
            &ProviderKind::Claude,
            &other
        ),
        GuardedClearOutcome::Cleared
    );
    assert!(!observed.0.removed.load(Ordering::Acquire));
    let lock = lock_inflight_state_path(&path).unwrap();
    assert_eq!(js_remove()["reason"], "Busy");
    assert!(!observed.0.removed.load(Ordering::Acquire));
    drop(lock);
    let bytes = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(unlink(&path).is_err());
    assert!(!observed.0.removed.load(Ordering::Acquire));
    fs::remove_dir(&path).unwrap();
    fs::write(&path, &bytes).unwrap();
    let mut protected: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    protected["herdr_before_start_stop"] = json!({"protocol":1});
    fs::write(&path, serde_json::to_vec(&protected).unwrap()).unwrap();
    assert_eq!(js_remove()["reason"], "ProtectedReceipt");
    assert!(!observed.0.removed.load(Ordering::Acquire));
    row.turn_nonce = Some("other".into());
    fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
    assert_eq!(js_remove()["ok"], true);
    assert!(!observed.0.removed.load(Ordering::Acquire));
}
#[tokio::test]
async fn removal_before_wait_is_latched_and_duplicates_cannot_add_wakes() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (path, row) = seed();
    let observed = watch(&ProviderKind::Claude, CHANNEL).unwrap().unwrap();
    let signal = observed.0.clone();
    unlink(&path).unwrap();
    removed(&path, Some(InflightEpisodePin::from_state(&row)));
    assert!(signal.removed.load(Ordering::Acquire));
    observed.removed().await;
    // The single permit is consumed; a duplicate does not leave another one behind.
    assert!(
        tokio::time::timeout(std::time::Duration::ZERO, signal.ready.notified())
            .await
            .is_err()
    );
}
