use super::*;

fn pending(id: u64, nonce: &str) -> Pending {
    Pending {
        version: 1,
        id,
        nonce: nonce.into(),
        session: None,
        path: None,
        file: None,
        start: None,
        input: "input".into(),
        body: "body".into(),
        captured_ms: now_epoch_ms(),
        manifest: None,
    }
}

#[test]
fn i7441b_delayed_projection_keeps_latest_queue_time_and_cap_order() {
    let mut ledger = CompletedTurnLedger::default();
    insert(&mut ledger, &pending(1, "B"), 1000);
    insert(&mut ledger, &pending(1, "A"), 100);
    assert_eq!(ledger.settled_commit_ms().get(&1), Some(&1000));
    insert(&mut ledger, &pending(1, "B"), 90);
    assert_eq!(ledger.settled_commit_ms().get(&1), Some(&1000));
    for id in 2..502 {
        insert(&mut ledger, &pending(id, "other"), 200 + id);
    }
    prune_ledger(&mut ledger, 1000);
    assert_eq!(ledger.entries.len(), LEDGER_ENTRY_CAP);
    assert_eq!(ledger.settled_commit_ms().get(&1), Some(&1000));
    assert!(
        !ledger
            .entries
            .iter()
            .any(|entry| entry.user_msg_id == 1 && entry.turn_nonce.as_deref() == Some("A"))
    );
}

#[tokio::test]
async fn i7441b_unresolved_context_stays_pending_without_completion() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let f = super::super::super::o_delivery_evidence::tests::Fixture::new(
        744105,
        ProviderKind::Claude,
        &["body"],
    )
    .await;
    let (_http, _http_guard) = f.http().await;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
        f.row.channel_id,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )]);
    capture(&ProviderKind::Claude, &f.row);
    let pending_path = path(&ProviderKind::Claude, f.row.channel_id).unwrap();
    let before = std::fs::read(&pending_path).unwrap();
    f.port.messages.lock().unwrap().clear();
    project(&shared, &ProviderKind::Claude, f.row.channel_id).await;
    assert_eq!(std::fs::read(&pending_path).unwrap(), before);
    assert!(read_ledger(&ProviderKind::Claude, f.row.channel_id).is_none());
}

#[tokio::test]
async fn i7441b_fixed_manifest_projection_does_not_reread_past_source() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let f = super::super::super::o_delivery_evidence::tests::Fixture::new(
        744106,
        ProviderKind::Claude,
        &["body"],
    )
    .await;
    let (_http, _http_guard) = f.http().await;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
        f.row.channel_id,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )]);
    capture(&ProviderKind::Claude, &f.row);
    std::fs::remove_file(&f.source.path).unwrap();
    manifest::SOURCE_BYTES.with(|c| c.set(0));
    project(&shared, &ProviderKind::Claude, f.row.channel_id).await;
    assert_eq!(manifest::SOURCE_BYTES.with(|c| c.get()), 0);
    assert!(
        read_ledger(&ProviderKind::Claude, f.row.channel_id)
            .unwrap()
            .settled_ids()
            .contains(&f.row.user_msg_id)
    );
    assert!(
        read(&path(&ProviderKind::Claude, f.row.channel_id).unwrap())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn i7441b_pending_recheck_and_failed_ledger_publication_preserve_context() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let f = super::super::super::o_delivery_evidence::tests::Fixture::new(
        744182,
        ProviderKind::Claude,
        &["body"],
    )
    .await;
    let (_http, _guard) = f.http().await;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
        f.row.channel_id,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )]);
    capture(&ProviderKind::Claude, &f.row);
    let pending_path = path(&ProviderKind::Claude, f.row.channel_id).unwrap();
    let mut replacement = read(&pending_path).unwrap();
    replacement[0].nonce = "new captured context".into();
    let changed = pending_path.clone();
    *f.port.on_get.lock().unwrap() = Some(Box::new(move || {
        save(&changed, &replacement).unwrap();
    }));
    project(&shared, &ProviderKind::Claude, f.row.channel_id).await;
    assert!(read_ledger(&ProviderKind::Claude, f.row.channel_id).is_none());
    assert_eq!(
        read(&pending_path).unwrap()[0].nonce,
        "new captured context"
    );
    let ledger = ledger_path(&ProviderKind::Claude, f.row.channel_id).unwrap();
    std::fs::create_dir_all(&ledger).unwrap();
    let before = std::fs::read(&pending_path).unwrap();
    project(&shared, &ProviderKind::Claude, f.row.channel_id).await;
    assert_eq!(
        std::fs::read(&pending_path).unwrap(),
        before,
        "failed publication cannot release context"
    );
    std::fs::remove_dir(&ledger).unwrap();
    project(&shared, &ProviderKind::Claude, f.row.channel_id).await;
    let bytes = std::fs::read(&ledger).unwrap();
    assert!(read(&pending_path).unwrap().is_empty());
    project(&shared, &ProviderKind::Claude, f.row.channel_id).await;
    assert_eq!(std::fs::read(&ledger).unwrap(), bytes);
}

#[tokio::test]
async fn i7441b_capture_never_invents_old_nonce_or_non_input_completion() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let f = super::super::super::o_delivery_evidence::tests::Fixture::new(
        744189,
        ProviderKind::Claude,
        &["body"],
    )
    .await;
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
        f.row.channel_id,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )]);
    for variant in 0..4 {
        let mut row = f.row.clone();
        match variant {
            0 => row.turn_nonce = None,
            1 => row.user_msg_id = 0,
            2 => row.silent_turn = true,
            _ => {
                row.tui_terminal_kind =
                    Some(crate::services::agent_protocol::NativeTerminalKind::Aborted)
            }
        }
        capture(&ProviderKind::Claude, &row);
        assert!(
            !path(&ProviderKind::Claude, f.row.channel_id)
                .unwrap()
                .exists(),
            "invalid input {variant}"
        );
    }
    let file = path(&ProviderKind::Claude, f.row.channel_id).unwrap();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let old = pending(1, "old");
    save(&file, std::slice::from_ref(&old)).unwrap();
    let before = std::fs::read(&file).unwrap();
    let mut oversized = old;
    oversized.body = "x".repeat(8 * 1024 * 1024);
    assert!(save(&file, &[oversized]).is_none());
    assert_eq!(std::fs::read(&file).unwrap(), before);
}
