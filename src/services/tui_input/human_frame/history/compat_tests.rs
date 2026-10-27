use super::*;

#[test]
fn history_v2_envelope_is_not_a_v1_store_with_an_ignored_version() {
    let snapshot = compat::HistoryV2 {
        version: 2,
        live: Vec::new(),
        passive: vec![serde_json::json!({"attempt": "A", "row": "native-A"})],
    };
    let raw = serde_json::to_vec(&snapshot).unwrap();
    assert!(
        serde_json::from_slice::<Store>(&raw).is_err(),
        "A24: an old V1 reader must reject the V2 envelope"
    );
    let compat::HistoryRead::ReadOnlyV2(read) = compat::inspect(&raw).unwrap() else {
        panic!("V2 read-only")
    };
    assert_eq!(read.passive[0]["row"], "native-A");
    assert!(compat::inspect(br#"{"version":3,"live":[],"passive":[]}"#).is_err());
    assert!(compat::inspect(br#"{"version":2,"entries":[],"live":[],"passive":[]}"#).is_err());
    assert!(matches!(
        compat::inspect(br#"{"entries":[]}"#).unwrap(),
        compat::HistoryRead::Legacy(_)
    ));
}

#[test]
fn history_v2_update_refuses_without_changing_recognition_evidence() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let path = path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = br#"{"version":2,"live":[],"passive":[{"attempt":"A","row":"native-A"}]}"#;
    std::fs::write(&path, bytes).unwrap();
    let applied = std::cell::Cell::new(false);
    let result = update(|store| {
        applied.set(true);
        store.entries.clear();
        Ok(())
    });
    assert!(result.is_err(), "V2 is inspection-only for this writer");
    assert!(
        !applied.get(),
        "read-only refusal must precede mutation callback"
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}
