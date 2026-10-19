use super::*;

fn seed(case: u64) -> Snapshot {
    let scope = TaskCardScope::new(case, "claude", "metadata-authority-fixture", "event");
    let content = "immutable body\n\n-# ⏳ 5h: 3% (4h39m)".to_string();
    let snapshot = Snapshot {
        scope: scope.clone(),
        message_id: case + 1000,
        bot_key: "notify".into(),
        revision: 3,
        hash: content_hash(&content),
        content: content.clone(),
        visible: "immutable body".into(),
    };
    MEMORY_STORE.lock().unwrap().insert(
        scope,
        MemoryRow {
            state: MemoryState::CardPosted,
            bot_key: snapshot.bot_key.clone(),
            nonce: "stable-original-nonce".into(),
            message_id: Some(snapshot.message_id),
            revision: snapshot.revision,
            update_count: 1,
            rendered_content: content,
            content_hash: snapshot.hash.clone(),
            terminal_delivery_fingerprint: None,
            lease_owner: None,
            lease_expires_at: None,
            post_started_at: None,
            last_error: None,
        },
    );
    snapshot
}

#[test]
fn cleanup_edit_claim_rejects_changed_message_bot_revision_hash_and_live_lease() {
    for case in 0..6 {
        let snapshot = seed(581_930 + case);
        {
            let mut rows = MEMORY_STORE.lock().unwrap();
            let row = rows.get_mut(&snapshot.scope).unwrap();
            match case {
                0 => row.message_id = Some(snapshot.message_id + 1),
                1 => row.bot_key = "replacement-bot".into(),
                2 => row.revision += 1,
                3 => row.content_hash = "replacement-content-hash".into(),
                4 => {
                    row.lease_owner = Some("another-worker".into());
                    row.lease_expires_at = Some(Instant::now() + Duration::from_secs(30));
                }
                _ => row.state = MemoryState::Posting,
            }
        }
        assert!(
            claim_memory_snapshot(snapshot).unwrap().is_none(),
            "metadata cleanup cannot acquire changed authority case={case}"
        );
    }
}

#[test]
fn cleanup_edit_reuses_existing_mark_edited_and_preserves_nonce_revision_and_body() {
    let snapshot = seed(581_940);
    let (claim, visible) = claim_memory_snapshot(snapshot.clone()).unwrap().unwrap();
    assert!(
        matches!(claim.action, ClaimAction::Edit { message_id } if message_id == snapshot.message_id)
    );
    mark_edited_memory(
        &claim,
        snapshot.message_id,
        &visible,
        &content_hash(&visible),
    )
    .unwrap();
    assert!(
        mark_edited_memory(
            &claim,
            snapshot.message_id,
            &visible,
            &content_hash(&visible)
        )
        .is_err(),
        "released exact lease cannot commit twice"
    );
    let rows = MEMORY_STORE.lock().unwrap();
    let row = rows.get(&snapshot.scope).unwrap();
    assert_eq!(row.message_id, Some(snapshot.message_id));
    assert_eq!(row.nonce, "stable-original-nonce");
    assert_eq!(row.revision, 3);
    assert_eq!(row.update_count, 1);
    assert_eq!(row.rendered_content, snapshot.visible);
    assert_eq!(row.content_hash, content_hash(&snapshot.visible));
    assert!(row.lease_owner.is_none());
}
