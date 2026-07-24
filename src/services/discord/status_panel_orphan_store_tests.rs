use super::*;

fn test_inflight(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    status_message_id: Option<u64>,
    current_msg_id: u64,
    turn_start_offset: Option<u64>,
) -> InflightTurnState {
    let mut state: InflightTurnState = serde_json::from_value(serde_json::json!({
        "version": 9,
        "provider": provider.as_str(),
        "channel_id": channel_id,
        "channel_name": "orphan-store-test",
        "request_owner_user_id": user_msg_id,
        "user_msg_id": user_msg_id,
        "current_msg_id": current_msg_id,
        "current_msg_len": 0,
        "user_text": "test",
        "source": "text",
        "session_id": null,
        "tmux_session_name": "AgentDesk-test",
        "output_path": null,
        "input_fifo_path": null,
        "last_offset": 0,
        "full_response": "",
        "response_sent_offset": 0,
        "started_at": "2026-01-01 00:00:00",
        "updated_at": "2026-01-01 00:00:00"
    }))
    .expect("test inflight state");
    state.status_message_id = status_message_id;
    state.turn_start_offset = turn_start_offset;
    state
}

#[test]
fn enqueue_is_idempotent_and_removable() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let provider = ProviderKind::Codex;
    let token = "tok";
    enqueue_in_root(root, &provider, token, 100, 5001);
    enqueue_in_root(root, &provider, token, 100, 5001);
    enqueue_in_root(root, &provider, token, 100, 5002);
    let mut pending = load_pending_in_root(root, &provider, token);
    pending.sort();
    assert_eq!(pending, vec![(100, 5001), (100, 5002)]);

    remove_in_root(root, &provider, token, 100, 5001);
    assert_eq!(
        load_pending_in_root(root, &provider, token),
        vec![(100, 5002)]
    );

    remove_in_root(root, &provider, token, 100, 5002);
    assert!(load_pending_in_root(root, &provider, token).is_empty());
}

#[test]
fn legacy_id_files_load_as_stranded_entries() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let provider = ProviderKind::Claude;
    let token = "tok";
    let path = channel_file_path_in_root(root, &provider, token, 100);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, "[5001,5002]").expect("legacy ids");

    let entries = load_channel_in_root(root, &provider, token, 100);

    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.kind == StatusPanelOrphanKind::Stranded)
    );
    assert_eq!(
        entries.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        vec![5001, 5002]
    );
}

#[test]
fn orphan_drain_placeholder_is_live_defers_only_exact_live_anchor() {
    assert!(orphan_drain_placeholder_is_live(Some(5555), 5555));
    assert!(!orphan_drain_placeholder_is_live(Some(0), 0));
    assert!(!orphan_drain_placeholder_is_live(Some(9999), 5555));
    assert!(!orphan_drain_placeholder_is_live(None, 5555));
}

#[test]
fn pending_bind_drain_removes_record_when_bind_landed_without_delete() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let provider = ProviderKind::Claude;
    let token = "tok";
    let channel_id = 100;
    let panel_id = 5001;
    let live = test_inflight(&provider, channel_id, 7001, Some(panel_id), 6001, Some(10));
    enqueue_pending_bind_in_root(
        root,
        &provider,
        token,
        channel_id,
        panel_id,
        Some(InflightTurnIdentity::from_state(&live)),
    );

    let outcome = prepare_pending_bind_for_drain_in_root(
        root,
        &provider,
        token,
        channel_id,
        panel_id,
        Some(&live),
    );

    assert_eq!(outcome, PendingBindDrainOutcome::RemovedBoundPanel);
    assert!(
        load_pending_in_root(root, &provider, token).is_empty(),
        "case (a): once inflight owns the pending id, drain removes the record and never deletes the live panel"
    );
}

#[test]
fn pending_bind_exact_owner_remove_keeps_record_after_reownership() {
    let root = tempfile::tempdir().expect("tempdir");
    let orphan_root = root.path().join("orphans");
    let inflight_root = root.path().join("inflight");
    let provider = ProviderKind::Claude;
    let token = "tok";
    let channel_id = 100;
    let panel_id = 5001;
    let original = test_inflight(&provider, channel_id, 7001, Some(panel_id), 6001, Some(10));
    let replacement = test_inflight(&provider, channel_id, 7002, Some(5002), 6002, Some(20));
    enqueue_pending_bind_in_root(
        &orphan_root,
        &provider,
        token,
        channel_id,
        panel_id,
        Some(InflightTurnIdentity::from_state(&original)),
    );
    let inflight_path = crate::services::discord::inflight::inflight_state_path(
        &inflight_root,
        &provider,
        channel_id,
    );
    fs::create_dir_all(inflight_path.parent().expect("inflight parent")).expect("mkdir inflight");
    fs::write(
        &inflight_path,
        serde_json::to_string_pretty(&replacement).expect("replacement json"),
    )
    .expect("persist replacement");

    assert_eq!(
        remove_pending_bind_if_owned_in_root(
            &orphan_root,
            &inflight_root,
            &provider,
            token,
            channel_id,
            panel_id,
            &InflightTurnIdentity::from_state(&original),
        ),
        PendingBindOwnedRemovalOutcome::OwnershipMismatch
    );
    assert_eq!(
        load_pending_in_root(&orphan_root, &provider, token),
        vec![(channel_id, panel_id)]
    );
}

#[test]
fn pending_bind_exact_owner_remove_clears_matching_record() {
    let root = tempfile::tempdir().expect("tempdir");
    let orphan_root = root.path().join("orphans");
    let inflight_root = root.path().join("inflight");
    let provider = ProviderKind::Claude;
    let token = "tok";
    let channel_id = 100;
    let panel_id = 5001;
    let live = test_inflight(&provider, channel_id, 7001, Some(panel_id), 6001, Some(10));
    enqueue_pending_bind_in_root(
        &orphan_root,
        &provider,
        token,
        channel_id,
        panel_id,
        Some(InflightTurnIdentity::from_state(&live)),
    );
    let inflight_path = crate::services::discord::inflight::inflight_state_path(
        &inflight_root,
        &provider,
        channel_id,
    );
    fs::create_dir_all(inflight_path.parent().expect("inflight parent")).expect("mkdir inflight");
    fs::write(
        &inflight_path,
        serde_json::to_string_pretty(&live).expect("live json"),
    )
    .expect("persist live owner");

    assert_eq!(
        remove_pending_bind_if_owned_in_root(
            &orphan_root,
            &inflight_root,
            &provider,
            token,
            channel_id,
            panel_id,
            &InflightTurnIdentity::from_state(&live),
        ),
        PendingBindOwnedRemovalOutcome::OwnedRecordRemoved
    );
    assert!(load_pending_in_root(&orphan_root, &provider, token).is_empty());
}

#[test]
fn pending_bind_exact_owner_remove_restores_missing_protection_after_reownership() {
    let root = tempfile::tempdir().expect("tempdir");
    let orphan_root = root.path().join("orphans");
    let inflight_root = root.path().join("inflight");
    let provider = ProviderKind::Claude;
    let token = "tok";
    let channel_id = 100;
    let panel_id = 5001;
    let original = test_inflight(&provider, channel_id, 7001, Some(panel_id), 6001, Some(10));
    let replacement = test_inflight(&provider, channel_id, 7002, Some(5002), 6002, Some(20));
    let inflight_path = crate::services::discord::inflight::inflight_state_path(
        &inflight_root,
        &provider,
        channel_id,
    );
    fs::create_dir_all(inflight_path.parent().expect("inflight parent")).expect("mkdir inflight");
    fs::write(
        &inflight_path,
        serde_json::to_string_pretty(&replacement).expect("replacement json"),
    )
    .expect("persist replacement");

    assert_eq!(
        remove_pending_bind_if_owned_in_root(
            &orphan_root,
            &inflight_root,
            &provider,
            token,
            channel_id,
            panel_id,
            &InflightTurnIdentity::from_state(&original),
        ),
        PendingBindOwnedRemovalOutcome::OwnershipMismatch
    );
    assert_eq!(
        load_pending_in_root(&orphan_root, &provider, token),
        vec![(channel_id, panel_id)]
    );
}

#[test]
fn pending_bind_drain_defers_same_turn_unbound_window() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let provider = ProviderKind::Claude;
    let token = "tok";
    let channel_id = 100;
    let panel_id = 5001;
    let live = test_inflight(&provider, channel_id, 7001, Some(4999), 6001, Some(10));
    enqueue_pending_bind_in_root(
        root,
        &provider,
        token,
        channel_id,
        panel_id,
        Some(InflightTurnIdentity::from_state(&live)),
    );

    let outcome = prepare_pending_bind_for_drain_in_root(
        root,
        &provider,
        token,
        channel_id,
        panel_id,
        Some(&live),
    );
    let entries = load_channel_in_root(root, &provider, token, channel_id);

    assert_eq!(outcome, PendingBindDrainOutcome::Deferred);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, StatusPanelOrphanKind::PendingBind);
    assert_eq!(
        entries[0].pending_bind_drain_cycles, 0,
        "case (b): same-turn live row is still inside the bind window, not aging toward delete"
    );
}

#[test]
fn pending_bind_unclaimed_after_grace_reclassifies_to_stranded_delete_path() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let provider = ProviderKind::Claude;
    let token = "tok";
    let channel_id = 100;
    let panel_id = 5001;
    let original = test_inflight(&provider, channel_id, 7001, None, 6001, Some(10));
    enqueue_pending_bind_in_root(
        root,
        &provider,
        token,
        channel_id,
        panel_id,
        Some(InflightTurnIdentity::from_state(&original)),
    );

    assert_eq!(
        prepare_pending_bind_for_drain_in_root(root, &provider, token, channel_id, panel_id, None),
        PendingBindDrainOutcome::Deferred
    );
    assert_eq!(
        prepare_pending_bind_for_drain_in_root(root, &provider, token, channel_id, panel_id, None),
        PendingBindDrainOutcome::Deferred
    );
    assert_eq!(
        prepare_pending_bind_for_drain_in_root(root, &provider, token, channel_id, panel_id, None),
        PendingBindDrainOutcome::ReclassifiedToStranded
    );
    let entries = load_channel_in_root(root, &provider, token, channel_id);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, StatusPanelOrphanKind::Stranded);
    assert!(
        stranded_orphan_drain_should_delete(None, panel_id),
        "case (c): after two grace cycles an unclaimed pending bind follows the normal stranded delete path"
    );
}

#[test]
fn enqueue_skips_zero_ids_and_scopes_by_token() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let provider = ProviderKind::Claude;
    enqueue_in_root(root, &provider, "tok2", 0, 5001);
    enqueue_in_root(root, &provider, "tok2", 100, 0);
    assert!(load_pending_in_root(root, &provider, "tok2").is_empty());

    enqueue_in_root(root, &provider, "bot_a", 100, 5001);
    enqueue_in_root(root, &provider, "bot_b", 100, 6001);
    assert_eq!(
        load_pending_in_root(root, &provider, "bot_a"),
        vec![(100, 5001)]
    );
    assert_eq!(
        load_pending_in_root(root, &provider, "bot_b"),
        vec![(100, 6001)]
    );
}

#[test]
fn footer_mode_status_panel_orphan_enqueue_is_noop_at_store_api() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let provider = ProviderKind::Claude;

    enqueue_separate_status_panel_orphan_in_root_for_flags(
        root, true, true, &provider, "tok", 100, 5001,
    );

    assert!(
        load_pending_in_root(root, &provider, "tok").is_empty(),
        "flag-on footer-mode turns must not create panel orphan records"
    );
}

#[test]
fn flag_off_status_panel_orphan_enqueue_preserves_original_store_behavior() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let provider = ProviderKind::Claude;

    enqueue_separate_status_panel_orphan_in_root_for_flags(
        root, false, true, &provider, "tok", 100, 5001,
    );

    assert_eq!(
        load_pending_in_root(root, &provider, "tok"),
        vec![(100, 5001)]
    );
}

#[test]
fn drain_committed_delete_emits_relay_delete() {
    let _guard = crate::services::observability::test_runtime_lock();
    crate::services::observability::reset_for_tests();

    let ok: Result<(), serenity::Error> = Ok(());
    emit_orphan_drain_delete(&ProviderKind::Codex, 4242, 9001, &ok);

    let events = crate::services::observability::events::recent(50);
    let event = events
        .iter()
        .find(|event| event.event_type == "relay_delete")
        .expect("relay_delete should be in the recent ring");
    assert_eq!(event.channel_id, Some(4242));
    assert_eq!(event.payload["message_id"], 9001);
    assert_eq!(event.payload["source"], "status_panel_orphan_store_drain");
    assert_eq!(event.payload["operation_kind"], "delete_nonterminal");
    assert_eq!(event.payload["outcome"], "committed");
    assert_eq!(event.payload["status"], "committed");
}
