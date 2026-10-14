use super::*;

#[test]
fn r0l_placeholder_bulk_distinguishes_root_empty_partial_and_cleanup() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    assert_eq!(
        load_entries_report(None, &provider, "r0l").issues,
        vec![PlaceholderIssue::RootUnavailable]
    );
    assert!(
        load_queued_placeholders_report(&provider, "r0l")
            .issues
            .is_empty()
    );
    let path = channel_file_path(&provider, "r0l", ChannelId::new(632521)).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        br#"[{"user_message_id":7,"placeholder_message_id":17}]"#,
    )
    .unwrap();
    let bad = path.with_file_name("632522.json");
    fs::write(&bad, b"bad").unwrap();
    fs::create_dir(path.with_file_name("632523.json")).unwrap();
    let bytes = fs::read(&path).unwrap();
    SCANS.set(0);
    let report = load_queued_placeholders_report(&provider, "r0l");
    assert_eq!(SCANS.get(), 1, "single load");
    assert_eq!(report.entries.len(), 1);
    assert_eq!(
        report.entries[&(ChannelId::new(632521), MessageId::new(7))].get(),
        17
    );
    assert!(
        report
            .issues
            .iter()
            .any(|issue| matches!(issue, PlaceholderIssue::Parse(632522, _)))
    );
    assert!(
        report
            .issues
            .iter()
            .any(|issue| matches!(issue, PlaceholderIssue::Read(632523, _)))
    );
    assert_eq!(report.cleanup_attempts, 1);
    assert!(!bad.exists());
    assert_eq!(fs::read(path).unwrap(), bytes);
    ENTRY_ERROR.set(true);
    let entry_error = load_queued_placeholders_report(&provider, "r0l");
    assert!(
        entry_error
            .issues
            .iter()
            .any(|issue| matches!(issue, PlaceholderIssue::Entry(_)))
    );
    let blocked = store_root()
        .unwrap()
        .join(provider.as_str())
        .join("r0l-blocked");
    fs::write(blocked, b"not a directory").unwrap();
    assert!(matches!(
        load_queued_placeholders_report(&provider, "r0l-blocked")
            .issues
            .as_slice(),
        [PlaceholderIssue::ReadDir(_)]
    ));
}

#[test]
fn r0l_placeholder_malformed_cleanup_respects_fence_and_pending_clear_store() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(632524);
    let gate = fence::Gate::protect(provider.clone(), channel.get()).unwrap();
    let _health = fence::test_health::Clear::new(&gate);
    let path = channel_file_path(&provider, "r0l-fence", channel).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"bad protected row").unwrap();
    let _closing = gate.close().unwrap();
    let report = load_queued_placeholders_report(&provider, "r0l-fence");
    assert!(report.entries.is_empty());
    assert!(
        report
            .issues
            .contains(&PlaceholderIssue::Protected(channel.get()))
    );
    assert!(
        report
            .issues
            .iter()
            .any(|issue| matches!(issue, PlaceholderIssue::Cleanup(_)))
    );
    assert_eq!(fs::read(path).unwrap(), b"bad protected row");
    let clear =
        pending_clear_channel_file_path(&provider, "r0l-fence", ChannelId::new(632525)).unwrap();
    fs::create_dir_all(clear.parent().unwrap()).unwrap();
    fs::write(
        &clear,
        br#"[{"user_message_id":8,"placeholder_message_id":18}]"#,
    )
    .unwrap();
    let loaded = load_queue_exit_placeholder_clears(&provider, "r0l-fence");
    assert_eq!(
        loaded[&(ChannelId::new(632525), MessageId::new(8))].get(),
        18
    );
    assert!(clear.exists());
}

thread_local! {
    pub(super) static SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static SECOND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ENTRY_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
pub(super) fn mutant(name: &str) -> bool {
    std::env::var("ADK_R0L_MUTANT").ok().as_deref() == Some(name)
}
pub(super) fn entry(entry: std::io::Result<fs::DirEntry>) -> std::io::Result<fs::DirEntry> {
    if ENTRY_ERROR.replace(false) {
        Err(std::io::Error::other("injected entry failure"))
    } else {
        entry
    }
}
