#[cfg(any(target_os = "macos", target_os = "linux"))]
mod supported {
    use super::super::*;
    use serde_json::json;
    #[test]
    fn durable_handback_appends_and_recognizes_queue_marker_and_active_sources() {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
        std::fs::create_dir_all(&parent).unwrap();
        let root = tempfile::tempdir_in(parent).unwrap();
        let destination = Destination {
            root: root.path(),
            provider: &ProviderKind::Claude,
            token_hash: "token",
            channel: 9,
            authorized: true,
            active_sources: &[],
        };
        let item = |key| {
            json!({"author_id":7,"message_id":key,"text":format!("input {key}"),"channel_id":9,
            "channel_name":"preserved","override_channel_id":11,"reply_context":"reply",
            "source_message_ids":[key],"source_text_segments":[{"message_id":key,"text":format!("input {key}")}],
            "source_message_queued_generations":[{"message_id":key,"queued_generation":12,"enqueued_at_epoch_us":17}]})
        };
        for key in [99, 8, 2] {
            assert_eq!(
                enqueue(&destination, &item(key)).unwrap(),
                EnqueueOutcome::Persisted
            );
        }
        assert_eq!(
            enqueue(&destination, &item(8)).unwrap(),
            EnqueueOutcome::AlreadyPreserved
        );
        let path = root
            .path()
            .join("discord_pending_queue/claude/token/9.json");
        let queue = optional(&path).unwrap().unwrap();
        let keys: Vec<_> = queue
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["message_id"].as_u64().unwrap())
            .collect();
        assert_eq!(keys, vec![99, 8, 2]);
        assert_eq!(
            queue,
            json!([item(99), item(8), item(2)]),
            "wire metadata and existing queue must be unchanged"
        );
        let mut partial = item(5);
        partial["source_message_ids"] = json!([8, 5]);
        assert_eq!(
            enqueue(&destination, &partial).unwrap(),
            EnqueueOutcome::Rejected
        );
        let mut invalid = item(5);
        invalid["author_id"] = json!(0);
        assert_eq!(
            enqueue(&destination, &invalid).unwrap(),
            EnqueueOutcome::Rejected
        );
        assert_eq!(optional(&path).unwrap().unwrap(), queue);
        std::fs::write(
            path.with_extension("dispatch"),
            serde_json::to_vec(&item(3)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            enqueue(&destination, &item(3)).unwrap(),
            EnqueueOutcome::AlreadyPreserved
        );
        let destination = Destination {
            active_sources: &[4],
            ..destination
        };
        assert_eq!(
            enqueue(&destination, &item(4)).unwrap(),
            EnqueueOutcome::Rejected
        );
        let destination = Destination {
            authorized: false,
            ..destination
        };
        assert_eq!(
            enqueue(&destination, &item(5)).unwrap(),
            EnqueueOutcome::Rejected
        );
        assert_eq!(optional(&path).unwrap().unwrap(), queue);
    }
}

#[cfg(unix)]
#[test]
fn distinct_same_author_text_clean_wire_and_durable_upload_copy() {
    use super::*;
    use serde_json::json;
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    std::fs::create_dir_all(&parent).unwrap();
    let root = tempfile::tempdir_in(parent).unwrap();
    let destination = Destination {
        root: root.path(),
        provider: &ProviderKind::Claude,
        token_hash: "token",
        channel: 9,
        authorized: true,
        active_sources: &[],
    };
    let ledger = crate::services::tui_input::ledger::Ledger::open(root.path(), 9).unwrap();
    let pin = ledger
        .pin_blob("99", 0, "proof.txt", b"durable proof")
        .unwrap();
    let item = |key| json!({"author_id":7,"message_id":key,"text":"same","channel_id":9,"source_message_ids":[key],"source_text_segments":[{"message_id":key,"text":"same"}],"reply_context":"reply","merge_consecutive":true,"attempt":{"internal":true},"move_attempt":{},"rendered_prompt":"private","blob_pins":[]});
    let mut invalid = item(100);
    invalid["author_id"] = json!(0);
    invalid["blob_pins"] = json!([pin.clone()]);
    assert_eq!(
        enqueue(&destination, &invalid).unwrap(),
        EnqueueOutcome::Rejected
    );
    assert!(!root.path().join("discord_uploads").exists());
    let mut first = item(99);
    first["blob_pins"] = json!([pin.clone()]);
    assert_eq!(
        enqueue(&destination, &first).unwrap(),
        EnqueueOutcome::Persisted
    );
    assert_eq!(
        enqueue(&destination, &item(2)).unwrap(),
        EnqueueOutcome::Persisted
    );
    let path = root
        .path()
        .join("discord_pending_queue/claude/token/9.json");
    let queue = optional(&path).unwrap().unwrap();
    assert_eq!(
        queue.as_array().unwrap().len(),
        2,
        "distinct IDs may not text-dedup or author-merge"
    );
    for entry in queue.as_array().unwrap() {
        assert!(
            entry.get("attempt").is_none()
                && entry.get("blob_pins").is_none()
                && entry.get("move_attempt").is_none()
                && entry.get("rendered_prompt").is_none(),
            "internal ledger fields leaked"
        );
        assert_eq!(entry["reply_context"], "reply");
        assert_eq!(entry["merge_consecutive"], true);
    }
    let upload = queue[0]["pending_uploads"][0].as_str().unwrap();
    let copied = upload
        .split_once(" → ")
        .unwrap()
        .1
        .rsplit_once(" (")
        .unwrap()
        .0;
    assert!(Path::new(copied).starts_with(root.path().join("discord_uploads/9")));
    assert_eq!(std::fs::read(copied).unwrap(), b"durable proof");
    assert_eq!(
        ledger.read_blob(&pin).unwrap(),
        b"durable proof",
        "original pin retained"
    );
    assert_eq!(
        enqueue(&destination, &first).unwrap(),
        EnqueueOutcome::AlreadyPreserved
    );
    let mut mismatch = item(2);
    mismatch["reply_context"] = json!("changed");
    assert_eq!(
        enqueue(&destination, &mismatch).unwrap(),
        EnqueueOutcome::Rejected
    );
}
