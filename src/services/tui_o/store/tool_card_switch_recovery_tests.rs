use super::*;

fn reader(root: &Path, channel: u64) -> Arc<dyn Fn() -> bool + Send + Sync> {
    ToolCardSwitch::for_root(root, channel, Arc::new(|| true))
}

fn unreadable_preference(root: &Path, channel: u64) {
    fs::create_dir(root.join(DIR_NAME)).unwrap();
    fs::create_dir(path(root, channel)).unwrap();
    assert!(
        read(root, channel).is_err(),
        "fixture must cause a real read error"
    );
}

#[test]
fn a_new_reader_recovers_a_cached_read_error_and_updates_existing_readers() {
    let root = tempfile::tempdir().unwrap();
    let channel = 584_641;
    unreadable_preference(root.path(), channel);
    let original = reader(root.path(), channel);
    assert!(!original());
    assert!(status(root.path(), channel).is_err());

    fs::remove_dir(path(root.path(), channel)).unwrap();
    let restored = Override {
        schema: 1,
        enabled: true,
        by: 456,
        at: Utc::now(),
    };
    fs::write(
        path(root.path(), channel),
        serde_json::to_vec(&restored).unwrap(),
    )
    .unwrap();
    assert!(!original(), "repair alone does not bypass the shared cache");
    let recovered = reader(root.path(), channel);
    assert!(recovered());
    assert!(
        original(),
        "recovery must publish to readers already in use"
    );
    assert_eq!(status(root.path(), channel), Ok(Some(restored.clone())));
    assert_eq!(read(root.path(), channel), Ok(Some(restored)));
}

#[test]
fn a_missing_file_after_a_read_error_recovers_the_default_on_value() {
    let root = tempfile::tempdir().unwrap();
    let channel = 584_642;
    unreadable_preference(root.path(), channel);
    let original = reader(root.path(), channel);
    assert!(!original());
    fs::remove_dir(path(root.path(), channel)).unwrap();
    assert!(reader(root.path(), channel)());
    assert!(original());
    assert_eq!(status(root.path(), channel), Ok(None));
}

#[test]
fn retries_keep_corrupt_or_unsupported_preferences_disabled() {
    for bytes in [
        b"broken".to_vec(),
        br#"{"schema":2,"enabled":true,"by":1,"at":"2026-10-19T00:00:00Z"}"#.to_vec(),
        vec![b' '; MAX_BYTES as usize + 1],
    ] {
        let root = tempfile::tempdir().unwrap();
        let channel = 584_643;
        fs::create_dir(root.path().join(DIR_NAME)).unwrap();
        fs::write(path(root.path(), channel), bytes).unwrap();
        let original = reader(root.path(), channel);
        assert!(!original());
        for _ in 0..2 {
            assert!(!reader(root.path(), channel)());
            assert!(!original());
            assert!(status(root.path(), channel).is_err());
        }
    }
}
