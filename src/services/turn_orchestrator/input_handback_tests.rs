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
            EnqueueOutcome::AlreadyPreserved
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
