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

/// The production readers a rolled-back binary uses still see frames a V2 history records.
#[test]
fn history_v2_frames_stay_known_to_the_production_readers() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let transcript = root.path().join("native.jsonl");
    std::fs::write(&transcript, "").unwrap();
    let session = format!("rollback-{}", uuid::Uuid::new_v4());
    let frame = super::super::idle("ann", "720101", &session);
    register(
        ("claude", &session, "attempt-r", "720101"),
        &transcript,
        0,
        &frame,
    )
    .unwrap();
    let path = path().unwrap();
    let v1: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let v2 = serde_json::json!({"version": 2, "live": v1["entries"], "passive": []});
    std::fs::write(&path, v2.to_string()).unwrap();
    assert!(
        known(&frame),
        "a V2-recorded frame is still an injected frame"
    );
    assert!(
        screen_pending(&session, &frame),
        "and still owns its composer"
    );
    assert!(
        update(|_| Ok(())).is_err(),
        "while the V2 file stays read-only"
    );
}

/// A kept history parse never outlives a rewrite, by this process or by another writer, even one
/// that keeps the length and restores the mtime.
#[cfg(unix)]
#[test]
fn settled_history_is_not_reread_until_rewritten() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let session = format!("stamped-{}", uuid::Uuid::new_v4());
    let [first, second, third] = ["a", "b", "c"].map(|n| super::super::idle(n, "1", &session));
    let registry = v2::Registry::runtime().unwrap();
    let plan = |id: u64, frame: &str| {
        let attempt = serde_json::json!({"nonce": format!("stamped-{id}"), "source": {}});
        let input = serde_json::json!({"identity": {"sources": [id]}, "attempt": attempt,
            "original": {"author_id": 1}, "payload": {"frame": frame}});
        registry.prepare("stamped", &input).unwrap();
    };
    let path = path().unwrap();
    let at = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let settle = || {
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(at).unwrap();
    };
    plan(1, &first);
    settle();
    assert!(known(&first));
    HISTORY_READS.with(|reads| reads.set(0));
    assert!(known(&first) && !known(&second));
    assert_eq!(HISTORY_READS.with(|reads| reads.get()), 0);
    plan(2, &second);
    assert!(known(&second), "this process's registration is seen");
    settle();
    assert!(known(&second));
    let mut raw = std::fs::read_to_string(&path).unwrap();
    for (old, new) in class_hashes(&second).iter().zip(class_hashes(&third)) {
        raw = raw.replace(old.as_str(), &new);
    }
    let len = std::fs::metadata(&path).unwrap().len();
    std::fs::write(&path, raw).unwrap();
    settle();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), len);
    assert!(
        known(&third) && !known(&second),
        "an outside same-length rewrite is seen"
    );
}
