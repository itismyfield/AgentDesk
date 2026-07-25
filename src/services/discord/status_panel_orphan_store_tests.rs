use std::sync::Mutex;

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
fn pending_bind_write_failure_is_reported() {
    let root = tempfile::tempdir().expect("tempdir");
    let blocked_root = root.path().join("blocked");
    fs::write(&blocked_root, "not a directory").expect("blocking file");

    let error =
        enqueue_pending_bind_in_root(&blocked_root, &ProviderKind::Claude, "tok", 100, 5001, None)
            .expect_err("pending-bind durability failure must reach the caller");

    assert!(!error.is_empty());
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
fn corrupt_legacy_store_fails_closed_without_overwrite_4891() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let provider = ProviderKind::Claude;
    let token = "tok";
    let channel_id = 100;
    let legacy = channel_file_path_in_root(root, &provider, token, channel_id);
    fs::create_dir_all(legacy.parent().expect("parent")).expect("mkdir");
    fs::write(&legacy, "{").expect("corrupt legacy");

    assert!(enqueue_in_root(root, &provider, token, channel_id, 5001).is_err());
    assert!(remove_in_root_checked(root, &provider, token, channel_id, 5001).is_err());
    assert_eq!(fs::read_to_string(&legacy).expect("legacy unchanged"), "{");
    assert!(!entry_path_in_root(root, &provider, token, channel_id, 5001).exists());
    assert!(!tombstone_path_in_root(root, &provider, token, channel_id, 5001).exists());
}

#[test]
fn per_panel_writer_survives_legacy_aggregate_rewrite_4891() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let provider = ProviderKind::Claude;
    let token = "tok";
    let channel_id = 100;
    let legacy = channel_file_path_in_root(root, &provider, token, channel_id);
    fs::create_dir_all(legacy.parent().expect("parent")).expect("mkdir");
    fs::write(&legacy, "[5001]").expect("legacy seed");

    enqueue_in_root(root, &provider, token, channel_id, 5002).expect("new writer entry");
    fs::write(&legacy, "[5001,5003]").expect("rolling legacy rewrite");

    let mut pending = load_pending_in_root(root, &provider, token);
    pending.sort();
    assert_eq!(
        pending,
        vec![(channel_id, 5001), (channel_id, 5002), (channel_id, 5003)]
    );
}

#[test]
fn removal_without_legacy_entry_leaves_no_tombstone_4891() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let provider = ProviderKind::Claude;
    let token = "tok";
    let channel_id = 100;
    let panel_id = 5001;

    enqueue_in_root(root, &provider, token, channel_id, panel_id).expect("enqueue");
    remove_in_root_checked(root, &provider, token, channel_id, panel_id).expect("remove");

    assert!(load_pending_in_root(root, &provider, token).is_empty());
    assert!(
        !tombstone_path_in_root(root, &provider, token, channel_id, panel_id).exists(),
        "new-only entries do not need a permanent legacy-suppression tombstone"
    );
}

#[test]
fn legacy_removal_keeps_suppression_tombstone_4891() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let provider = ProviderKind::Claude;
    let token = "tok";
    let channel_id = 100;
    let panel_id = 5001;
    let legacy = channel_file_path_in_root(root, &provider, token, channel_id);
    fs::create_dir_all(legacy.parent().expect("parent")).expect("mkdir");
    fs::write(&legacy, "[5001]").expect("legacy seed");

    remove_in_root_checked(root, &provider, token, channel_id, panel_id).expect("remove legacy");

    assert!(load_pending_in_root(root, &provider, token).is_empty());
    assert!(tombstone_path_in_root(root, &provider, token, channel_id, panel_id).exists());
}

#[test]
fn reenqueue_removes_tombstone_and_restores_same_panel_4891() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let provider = ProviderKind::Claude;
    let token = "tok";
    let channel_id = 100;
    let panel_id = 5001;
    let legacy = channel_file_path_in_root(root, &provider, token, channel_id);
    fs::create_dir_all(legacy.parent().expect("parent")).expect("mkdir");
    fs::write(&legacy, "[5001]").expect("legacy seed");

    remove_in_root_checked(root, &provider, token, channel_id, panel_id).expect("remove");
    assert!(load_pending_in_root(root, &provider, token).is_empty());
    assert!(tombstone_path_in_root(root, &provider, token, channel_id, panel_id).exists());

    enqueue_in_root(root, &provider, token, channel_id, panel_id).expect("reenqueue");
    assert_eq!(
        load_pending_in_root(root, &provider, token),
        vec![(channel_id, panel_id)]
    );
    assert!(!tombstone_path_in_root(root, &provider, token, channel_id, panel_id).exists());
}

const CROSS_PROCESS_WRITER_ROOT_4891: &str = "AGENTDESK_ORPHAN_WRITER_ROOT_4891";
const CROSS_PROCESS_WRITER_PANEL_4891: &str = "AGENTDESK_ORPHAN_WRITER_PANEL_4891";

#[test]
fn cross_process_writer_helper_4891() {
    let Ok(root) = std::env::var(CROSS_PROCESS_WRITER_ROOT_4891) else {
        return;
    };
    let panel_id = std::env::var(CROSS_PROCESS_WRITER_PANEL_4891)
        .expect("writer panel id")
        .parse::<u64>()
        .expect("numeric writer panel id");
    enqueue_in_root(
        Path::new(&root),
        &ProviderKind::Claude,
        "tok",
        100,
        panel_id,
    )
    .expect("cross-process enqueue");
}

#[test]
fn actual_cross_process_writers_keep_distinct_panel_entries_4891() {
    let root = tempfile::tempdir().expect("tempdir");
    let executable = std::env::current_exe().expect("test executable");
    let helper = concat!(
        "services::discord::status_panel_orphan_store::tests::",
        "cross_process_writer_helper_4891"
    );
    let mut writers = Vec::new();
    for panel_id in 5001..5009 {
        writers.push(
            std::process::Command::new(&executable)
                .args(["--exact", helper])
                .env(CROSS_PROCESS_WRITER_ROOT_4891, root.path())
                .env(CROSS_PROCESS_WRITER_PANEL_4891, panel_id.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn orphan writer"),
        );
    }
    for mut writer in writers {
        let status = writer.wait().expect("wait for orphan writer");
        assert!(
            status.success(),
            "cross-process orphan writer failed: {status}"
        );
    }

    let pending = load_pending_in_root(root.path(), &ProviderKind::Claude, "tok");
    assert_eq!(pending.len(), 8);
    assert_eq!(pending.first(), Some(&(100, 5001)));
    assert_eq!(pending.last(), Some(&(100, 5008)));
}

#[test]
fn concurrent_new_writers_keep_distinct_panel_entries_4891() {
    let root = tempfile::tempdir().expect("tempdir");
    let root_path = root.path().to_path_buf();
    let mut writers = Vec::new();
    for panel_id in 5001..5017 {
        let root_path = root_path.clone();
        writers.push(std::thread::spawn(move || {
            enqueue_in_root(&root_path, &ProviderKind::Claude, "tok", 100, panel_id)
                .expect("concurrent enqueue");
        }));
    }
    for writer in writers {
        writer.join().expect("writer join");
    }

    let mut pending = load_pending_in_root(&root_path, &ProviderKind::Claude, "tok");
    pending.sort();
    assert_eq!(pending.len(), 16);
    assert_eq!(pending.first(), Some(&(100, 5001)));
    assert_eq!(pending.last(), Some(&(100, 5016)));
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
fn pending_bind_drain_defers_bound_panel_until_singleton_commit_4891() {
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
    )
    .expect("seed pending bind");

    let outcome = prepare_pending_bind_for_drain_in_root(
        root,
        &provider,
        token,
        channel_id,
        panel_id,
        Some(&live),
    );

    assert_eq!(outcome, PendingBindDrainOutcome::Deferred);
    assert_eq!(
        load_pending_in_root(root, &provider, token),
        vec![(channel_id, panel_id)],
        "inflight ownership alone must not purge recovery before singleton commit"
    );
}

#[tokio::test]
async fn bound_pending_bind_survives_drain_until_commit_then_purges_4891() {
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let runtime_root = tempfile::tempdir().expect("runtime root");
    let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        runtime_root.path(),
    );
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let token = "tok";
    let channel_id = 48_930;
    let panel_id = 5_001;
    let live = test_inflight(
        &provider,
        channel_id,
        7_001,
        Some(panel_id),
        6_001,
        Some(10),
    );
    crate::services::discord::inflight::save_inflight_state(&live).expect("persist bound inflight");
    enqueue_pending_bind(
        &provider,
        token,
        channel_id,
        panel_id,
        Some(InflightTurnIdentity::from_state(&live)),
    )
    .expect("persist pending bind");

    let deletes = Arc::new(Mutex::new(Vec::new()));
    let observed = deletes.clone();
    assert_eq!(
        drain_with_delete(&shared, &provider, token, move |channel, message| {
            let observed = observed.clone();
            async move {
                observed
                    .lock()
                    .expect("delete observations lock")
                    .push((channel, message));
                Ok(())
            }
        })
        .await,
        0
    );
    assert!(is_queued(&provider, token, channel_id, panel_id));
    assert!(deletes.lock().expect("delete observations lock").is_empty());

    crate::services::discord::status_panel_singleton_store::bind_if_owned(
        &provider, token, channel_id, panel_id, None,
    )
    .expect("commit singleton");
    remove_pending_bind(&provider, token, channel_id, panel_id);
    assert!(
        !is_queued(&provider, token, channel_id, panel_id),
        "only successful singleton commit may purge the bound PendingBind"
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
    )
    .expect("seed pending bind");
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
        PendingBindOwnedRemovalOutcome::NotOwned
    );
    assert_eq!(
        load_pending_in_root(&orphan_root, &provider, token),
        vec![(channel_id, panel_id)]
    );
}

#[test]
fn pending_bind_read_failure_is_durability_failure_not_mismatch_4891() -> Result<(), String> {
    let root = tempfile::tempdir().map_err(|error| error.to_string())?;
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
    )?;
    let inflight_path = crate::services::discord::inflight::inflight_state_path(
        &inflight_root,
        &provider,
        channel_id,
    );
    fs::create_dir_all(&inflight_path).map_err(|error| error.to_string())?;

    assert!(matches!(
        remove_pending_bind_if_owned_in_root(
            &orphan_root,
            &inflight_root,
            &provider,
            token,
            channel_id,
            panel_id,
            &InflightTurnIdentity::from_state(&live),
        ),
        PendingBindOwnedRemovalOutcome::DurabilityFailure(_)
    ));
    assert_eq!(
        load_pending_in_root(&orphan_root, &provider, token),
        vec![(channel_id, panel_id)],
        "orphan read IO ambiguity must retain pending-bind protection"
    );
    Ok(())
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
    )
    .expect("seed pending bind");
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
        PendingBindOwnedRemovalOutcome::Removed
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
        PendingBindOwnedRemovalOutcome::NotOwned
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
    )
    .expect("seed pending bind");

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
    )
    .expect("seed pending bind");

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
        stranded_orphan_drain_should_delete(
            None,
            &crate::services::discord::status_panel_singleton_store::StatusPanelSingletonLoadOutcome::Missing,
            panel_id,
        ),
        "case (c): after two grace cycles an unclaimed pending bind follows the normal stranded delete path"
    );
}

#[test]
fn stranded_orphan_drain_preserves_current_completed_singleton_4891() {
    let provider = ProviderKind::Claude;
    let channel_id = 48_911;
    let completed_panel = 5001;
    let next_turn = test_inflight(&provider, channel_id, 7002, Some(5002), 6002, Some(20));

    assert!(
        !stranded_orphan_drain_should_delete(
            Some(&next_turn),
            &crate::services::discord::status_panel_singleton_store::StatusPanelSingletonLoadOutcome::Present(
                crate::services::discord::status_panel_singleton_store::StatusPanelSingletonBinding {
                    panel_message_id: completed_panel,
                    generation: 1,
                },
            ),
            completed_panel,
        ),
        "the 3-to-4 orphan drain link must not delete the current completed singleton after inflight moved"
    );
    assert!(
        stranded_orphan_drain_should_delete(
            Some(&next_turn),
            &crate::services::discord::status_panel_singleton_store::StatusPanelSingletonLoadOutcome::Present(
                crate::services::discord::status_panel_singleton_store::StatusPanelSingletonBinding {
                    panel_message_id: 5002,
                    generation: 2,
                },
            ),
            completed_panel,
        ),
        "once a newer durable singleton supersedes it, the stranded old panel remains reclaimable"
    );
    assert!(
        !stranded_orphan_drain_should_delete(
            Some(&next_turn),
            &crate::services::discord::status_panel_singleton_store::StatusPanelSingletonLoadOutcome::DurabilityFailure(
                "malformed singleton".to_string(),
            ),
            completed_panel,
        ),
        "singleton read failure must fail closed and defer orphan deletion"
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

#[tokio::test]
async fn watcher_orphan_drain_does_not_delete_completed_current_singleton_4891() {
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let runtime_root = tempfile::tempdir().expect("runtime root");
    let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        runtime_root.path(),
    );
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let token = "tok";
    let channel_id = 48_913;
    let completed_panel = 5001;
    let next_panel = 5002;

    let completed = test_inflight(
        &provider,
        channel_id,
        7001,
        Some(completed_panel),
        6001,
        Some(10),
    );
    crate::services::discord::inflight::save_inflight_state(&completed)
        .expect("persist completed owner");
    crate::services::discord::status_panel_singleton_store::bind_if_owned(
        &provider,
        token,
        channel_id,
        completed_panel,
        None,
    )
    .expect("bind completed singleton");
    let next_turn = test_inflight(
        &provider,
        channel_id,
        7002,
        Some(next_panel),
        6002,
        Some(20),
    );
    crate::services::discord::inflight::save_inflight_state(&next_turn)
        .expect("move inflight to next panel");
    enqueue(&provider, token, channel_id, completed_panel);
    assert!(is_queued(&provider, token, channel_id, completed_panel));

    let deletes = Arc::new(Mutex::new(Vec::new()));
    let observed = deletes.clone();
    let cleared = drain_with_delete(&shared, &provider, token, move |channel, message| {
        let observed = observed.clone();
        async move {
            observed
                .lock()
                .expect("delete observations lock")
                .push((channel.get(), message.get()));
            Ok(())
        }
    })
    .await;

    assert_eq!(cleared, 0);
    assert!(deletes.lock().expect("delete observations lock").is_empty());
    assert!(
        is_queued(&provider, token, channel_id, completed_panel),
        "the completed current singleton must stay queued for the next turn's bind-then-retire convergence"
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
