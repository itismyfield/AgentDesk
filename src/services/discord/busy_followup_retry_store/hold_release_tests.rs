use super::*;

#[test]
fn a2_release_changes_only_live_sources_held_on_the_observed_session() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let channel = 7_314_502;
    for (input, session) in [(101, "pane-a"), (102, "pane-b"), (103, "pane-a")] {
        bind_notice_if_absent(&provider, channel, input, 700 + input).unwrap();
        record_attempt(
            &provider,
            (channel, input, 700 + input),
            &[],
            Duration::from_secs(90),
            Some(session),
        )
        .unwrap();
    }
    let pass = release_draft_holds(
        &provider,
        channel,
        "pane-a",
        &std::collections::HashSet::from([101, 102]),
    );
    assert_eq!(
        pass.released, 1,
        "neither another pane's source nor a cancelled source is released"
    );
    assert!(!load(&provider, channel, 101).unwrap().draft_hold);
    assert!(load(&provider, channel, 102).unwrap().draft_hold);
    assert!(load(&provider, channel, 103).unwrap().draft_hold);

    let root = runtime_store::discord_busy_followup_retries_root().unwrap();
    let held_path = input_file_path_in_root(&root, &provider, channel, 103);
    assert_eq!(
        expiry_stamp_ms(&held_path, &fs::read(&held_path).unwrap()).unwrap(),
        None,
        "a no-charge preparation hold never ages into an absent binding"
    );
    let legacy = input_file_path_in_root(&root, &provider, channel, 104);
    fs::write(legacy, r#"{"notice_message_id":804,"busy_retry_count":2,"first_busy_retry_at_ms":1000,"draft_hold":true}"#).unwrap();
    bind_legacy_draft_session(&provider, channel, 104, "pane-a").unwrap();
    let restored = load(&provider, channel, 104).unwrap();
    assert_eq!(restored.draft_session.as_deref(), Some("pane-a"));
    assert_eq!(
        restored.busy_retry_count, 2,
        "binding an old hold does not spend or erase its budget"
    );
    let released = release_draft_holds(
        &provider,
        channel,
        "pane-a",
        &std::collections::HashSet::from([104]),
    );
    assert_eq!(released.released, 1);
    let restored = load(&provider, channel, 104).unwrap();
    assert_eq!(
        (
            restored.notice_message_id,
            restored.busy_retry_count,
            restored.active_retry_ms
        ),
        (804, 0, 0)
    );
}
