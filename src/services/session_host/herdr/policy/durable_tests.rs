use super::*;

#[test]
fn durable_directory_is_lazy_and_missing_is_distinct_from_failure() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("not-created");
    let store = Directory(Some(root.clone()));
    let name = key(&"missing").unwrap();
    assert_eq!(store.read(&name).unwrap(), None);
    assert!(
        !root.exists(),
        "store_constructor_and_read_are_lazy_contract"
    );
    assert!(Directory(None).read(&name).is_err());
    fs::write(&root, b"not a directory").unwrap();
    assert!(store.read(&name).is_err());
}

#[test]
fn durable_publication_preserves_bytes_and_private_modes() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("private/records");
    let store = Directory(Some(root.clone()));
    let name = key(&"one").unwrap();
    assert_eq!(
        store.change(&name, |_| Ok(b"one".to_vec())).unwrap(),
        b"one"
    );
    assert_eq!(store.read(&name).unwrap().unwrap(), b"one");
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700,
        "durable_directory_private_mode_contract"
    );
    assert_eq!(
        fs::metadata(store.path(&name, "json").unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "durable_snapshot_private_mode_contract"
    );
    assert!(!store.path(&name, "pending").unwrap().exists());
    assert_eq!(store.keys().unwrap(), [name]);
}

#[test]
fn durable_interrupted_publications_are_never_loaded_as_committed() {
    for stage in ["before_rename", "after_rename"] {
        let temp = tempfile::tempdir().unwrap();
        let store = Directory(Some(temp.path().join("records")));
        let name = key(&"crash").unwrap();
        store.change(&name, |_| Ok(b"old".to_vec())).unwrap();
        FAIL.with(|fail| fail.set(Some(stage)));
        let result = store.change(&name, |_| Ok(b"new".to_vec()));
        FAIL.with(|fail| fail.set(None));
        assert!(
            result.is_err(),
            "durable_crash_failure_is_reported_contract: {stage}"
        );
        let restored = Directory(store.0.clone());
        assert!(
            restored.read(&name).is_err(),
            "durable_interrupted_publication_refused_contract: {stage}"
        );
        assert!(restored.keys().is_err());
        let on_disk = fs::read(store.path(&name, "json").unwrap()).unwrap();
        assert_eq!(
            on_disk,
            if stage == "before_rename" {
                b"old"
            } else {
                b"new"
            }
        );
    }
}
