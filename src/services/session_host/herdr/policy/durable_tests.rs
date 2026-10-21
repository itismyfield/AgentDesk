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
    for stage in [
        "marker_sync",
        "candidate_sync",
        "before_rename",
        "after_rename",
        "parent_sync",
        "before_marker_clear",
        "after_marker_clear",
    ] {
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
            if ["marker_sync", "candidate_sync", "before_rename"].contains(&stage) {
                b"old"
            } else {
                b"new"
            }
        );
    }
}

#[test]
fn durable_existing_snapshot_permissions_and_nonregular_paths_are_not_missing() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let store = Directory(Some(temp.path().join("records")));
    let name = key(&"private").unwrap();
    store.change(&name, |_| Ok(b"committed".to_vec())).unwrap();
    let path = store.path(&name, "json").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let failure = store.read(&name).unwrap_err();
    assert!(
        failure.contains("0600")
            && failure.contains(&path.display().to_string())
            && failure.contains("restart"),
        "durable_loaded_permissions_are_verified_contract: {failure}"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(store.read(&name).unwrap().unwrap(), b"committed");
    let other = key(&"symlink").unwrap();
    std::os::unix::fs::symlink(&path, store.path(&other, "json").unwrap()).unwrap();
    assert!(
        store.read(&other).is_err(),
        "durable_symlink_snapshot_never_loaded_contract"
    );
    fs::set_permissions(store.0.as_ref().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        store.read(&name).is_err(),
        "durable_loaded_directory_permissions_contract"
    );
}

#[test]
fn durable_failed_update_and_equal_cas_never_publish_another_generation() {
    let temp = tempfile::tempdir().unwrap();
    let store = Directory(Some(temp.path().join("records")));
    let name = key(&"cas").unwrap();
    store.change(&name, |_| Ok(b"old".to_vec())).unwrap();
    let path = store.path(&name, "json").unwrap();
    let inode = fs::metadata(&path).unwrap().ino();
    assert!(
        store
            .change(&name, |_| Err("injected write owner failure".into()))
            .is_err(),
        "durable_failed_update_never_advances_snapshot_contract"
    );
    assert_eq!(store.read(&name).unwrap().unwrap(), b"old");
    assert_eq!(
        store
            .change(&name, |previous| Ok(previous.unwrap()))
            .unwrap(),
        b"old"
    );
    assert_eq!(
        fs::metadata(&path).unwrap().ino(),
        inode,
        "durable_equal_cas_skips_publication_contract"
    );
    assert!(!store.path(&name, "pending").unwrap().exists());
}

#[test]
fn durable_repair_requires_verified_snapshot_and_explicit_marker_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let store = Directory(Some(temp.path().join("records")));
    let name = key(&"repair").unwrap();
    store.change(&name, |_| Ok(b"old".to_vec())).unwrap();
    FAIL.with(|fail| fail.set(Some("after_rename")));
    assert!(store.change(&name, |_| Ok(b"new".to_vec())).is_err());
    FAIL.with(|fail| fail.set(None));
    let path = store.path(&name, "json").unwrap();
    let pending = store.path(&name, "pending").unwrap();
    let restarted = Directory(store.0.clone());
    let failure = restarted.read(&name).unwrap_err();
    assert!(
        failure.contains(&path.display().to_string())
            && failure.contains(&pending.display().to_string())
            && failure.contains("verifying")
            && failure.contains("restart"),
        "durable_recovery_action_names_actual_paths_contract: {failure}"
    );
    assert!(
        restarted
            .change(&name, |_| Ok(b"unverified".to_vec()))
            .is_err(),
        "durable_pending_marker_blocks_republication_contract"
    );
    // The fixture operator has verified/restored the intended bytes before clearing the marker.
    fs::write(&path, b"verified").unwrap();
    fs::remove_file(pending).unwrap();
    assert_eq!(restarted.read(&name).unwrap().unwrap(), b"verified");
}

#[test]
fn durable_new_snapshot_failure_leaves_no_committed_authority_after_restart() {
    for stage in [
        "marker_sync",
        "candidate_sync",
        "before_rename",
        "after_rename",
        "parent_sync",
        "before_marker_clear",
        "after_marker_clear",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("records");
        let store = Directory(Some(root.clone()));
        let name = key(&"fresh").unwrap();
        FAIL.with(|fail| fail.set(Some(stage)));
        let failure = store.change(&name, |_| Ok(b"fresh".to_vec()));
        FAIL.with(|fail| fail.set(None));
        assert!(
            failure.is_err(),
            "durable_new_snapshot_failure_contract: {stage}"
        );
        assert!(
            Directory(Some(root)).read(&name).is_err(),
            "durable_unacknowledged_snapshot_never_admitted_contract: {stage}"
        );
    }
}

#[test]
fn durable_store_root_symlink_is_refused_without_following_or_writing() {
    let temporary = tempfile::tempdir().unwrap();
    let actual = temporary.path().join("actual");
    fs::create_dir(&actual).unwrap();
    let logical = temporary.path().join("linked-store");
    std::os::unix::fs::symlink(&actual, &logical).unwrap();
    let directory = Directory(Some(logical.clone()));
    let name = key(&"must-not-follow").unwrap();
    assert!(
        directory.read(&name).is_err()
            && directory.keys().is_err()
            && directory
                .change(&name, |_| Ok(b"untrusted".to_vec()))
                .is_err(),
        "durable_store_root_symlink_is_not_authority_contract"
    );
    assert_eq!(
        fs::read_dir(&actual).unwrap().count(),
        0,
        "durable_root_symlink_never_writes_through_contract"
    );
    assert_eq!(fs::read_link(logical).unwrap(), actual);
}
