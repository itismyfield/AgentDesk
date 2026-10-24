use super::*;

#[test]
fn alpha_existing_open_identity_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("legacy-open.json");
    let old = serde_json::json!({
        "version": 1, "episode_key": "legacy-episode", "baseline_relay_offset": 0,
        "attempts": 5, "alert_queued": true, "staged_alert_id": 41,
        "orphaned_staged_alert_ids": [], "open_generation": 2
    });
    fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    fs::write(
        temp.path().join("beta-current-token.json"),
        r#"{"token":"00000000-0000-4000-8000-000000005715"}"#,
    )
    .unwrap();
    let loaded = load_record(&path).unwrap().unwrap();
    let restored = serde_json::to_value(&loaded).unwrap();
    assert_eq!(restored["coordinate_binding"], "Unresolved");
    assert_eq!(restored["staged_alert_id"], old["staged_alert_id"]);
    assert_eq!(restored["episode_key"], old["episode_key"]);
    assert_eq!(restored["open_generation"], old["open_generation"]);
    persist_record(&path, &loaded).unwrap();
    let reloaded = load_record(&path).unwrap().unwrap();
    assert_eq!(serde_json::to_value(reloaded).unwrap(), restored);
}

/// The episode budget record is not flushed past a failed rename, is flushed with the new bytes
/// once published, and a flush failure after its rename still counts as persisted.
#[test]
fn circuit_record_is_published_durably() {
    use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
    use crate::services::discord::runtime_store::{PARENT_DIR_FSYNC_FLUSHES, rename_fault};
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp
        .path()
        .join("runtime")
        .join("discord_relay_recovery_circuit");
    let path = circuit_path(&root, &ProviderKind::Codex, 7_234_900);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let old = serde_json::json!({
        "version": 1, "episode_key": "durable-episode", "baseline_relay_offset": 0,
        "attempts": 1, "alert_queued": false
    });
    fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    let mut record = load_record(&path).unwrap().unwrap();
    record.attempts = 2;
    let watch = Watch::new(temp.path(), Plan::default());
    {
        let _armed = rename_fault::Armed::new(&path);
        persist_record(&path, &record).unwrap_err();
    }
    assert_eq!(watch.count(Step::ParentSync), 0);
    assert_eq!(load_record(&path).unwrap().unwrap().attempts, 1);
    persist_record(&path, &record).unwrap();
    let synced = watch.events().into_iter().any(|event| {
        event.step == Step::ParentSync
            && event.ok
            && event
                .target_bytes
                .as_deref()
                .is_some_and(|b| b.contains("\"attempts\": 2"))
    });
    assert!(synced, "parent flushed with the new attempt count visible");
    assert!(watch.flushed(Step::Ancestors).contains(&root));
    drop(watch);

    let plan = Plan {
        fail: vec![Step::ParentSync],
        ..Plan::default()
    };
    let _failing = Watch::new(temp.path(), plan);
    record.attempts = 3;
    persist_record(&path, &record).unwrap();
    assert_eq!(load_record(&path).unwrap().unwrap().attempts, 3);
}
