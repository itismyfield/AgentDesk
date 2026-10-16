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
