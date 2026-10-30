use super::*;

#[test]
fn disabled_floor_touches_no_paths_even_when_the_parent_is_a_file() {
    let root = tempfile::tempdir().unwrap();
    let blocked = root.path().join("blocked");
    fs::write(&blocked, b"unchanged").unwrap();
    before_append(&blocked.join("o_store"), false).unwrap();
    assert_eq!(fs::read(&blocked).unwrap(), b"unchanged");
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn floor_is_idempotent_and_survives_without_a_notice_entry() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("o_store");
    before_append(&store, true).unwrap();
    assert_eq!(fs::read(store.join(FILE_NAME)).unwrap(), CONTENT);
    before_append(&store, true).unwrap();
    assert_eq!(fs::read(store.join(FILE_NAME)).unwrap(), CONTENT);
    assert_eq!(fs::read_dir(&store).unwrap().count(), 1);
}

#[test]
fn invalid_existing_floor_is_preserved_and_refused() {
    let root = tempfile::tempdir().unwrap();
    for bytes in [b"".as_slice(), b"2\n", b"1", b"not json"] {
        let store = root.path().join("o_store");
        fs::create_dir_all(&store).unwrap();
        let path = store.join(FILE_NAME);
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            before_append(&store, true).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[cfg(unix)]
#[test]
fn floor_write_failure_prevents_the_preappend_step() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("o_store");
    let path = store.join(FILE_NAME);
    let failure = durable::fault::plant(
        &store,
        durable::fault::Step::Write,
        io::ErrorKind::StorageFull,
        None,
    );
    assert_eq!(
        before_append(&store, true).unwrap_err().kind(),
        io::ErrorKind::StorageFull
    );
    assert!(!path.exists());
    drop(failure);
    before_append(&store, true).unwrap();
    assert_eq!(fs::read(path).unwrap(), CONTENT);
}

#[cfg(unix)]
#[test]
fn a_floor_symlink_is_never_followed_or_replaced() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("o_store");
    fs::create_dir(&store).unwrap();
    let target = root.path().join("target");
    fs::write(&target, CONTENT).unwrap();
    let floor = store.join(FILE_NAME);
    std::os::unix::fs::symlink(&target, &floor).unwrap();
    assert_eq!(
        before_append(&store, true).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert!(
        fs::symlink_metadata(floor)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(target).unwrap(), CONTENT);
}
