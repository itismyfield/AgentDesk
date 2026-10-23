use super::*;
use crate::config::TestRuntimeRootGuard;
use serde_json::{Value, json};

fn context() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, "v2-compat", None)
}

fn item(channel: u64, id: u64) -> Value {
    json!({"author_id": 7, "message_id": id, "queued_generation": 5,
        "source_message_ids": [id], "channel_id": channel, "text": "original A"})
}

fn fixture(channel: u64, waiting: usize) -> Value {
    json!({"version": 2,
        "items": (0..waiting).map(|i| item(channel, 200+i as u64)).collect::<Vec<_>>(),
        "head_injection": null,
        "retained_inputs": [{"input": {
            "identity": {"primary": 100, "queued_generation": 5,
                "sources": [{"message_id": 100, "queued_generation": 5}]},
            "original": item(channel, 100), "phase": "unknown", "attempt": null,
            "payload": null, "confirmation_deadline_ms": 30_000},
            "reason": {"kind": "unconfirmed"},
            "notice": {"kind": "unconfirmed", "enqueued": false}}],
        "terminal_identities": [{"identity": {"primary": 99, "queued_generation": 5,
            "sources": [{"message_id": 99, "queued_generation": 5}]},
            "settled_at_ms": 1_000}]})
}

fn seed(channel: u64, value: &Value) -> (std::path::PathBuf, Vec<u8>) {
    let ctx = context();
    let path =
        pending_queue_file_path(&ctx.provider, &ctx.token_hash, ChannelId::new(channel)).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = serde_json::to_vec_pretty(value).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    (path, bytes)
}

fn run(work: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(work);
}

#[test]
fn v2_reader_keeps_retained_outside_the_thirty_executable_items() {
    let _root = TestRuntimeRootGuard::new();
    let (path, before) = seed(7_272_211, &fixture(7_272_211, 30));
    let compat::SnapshotRead::ReadOnlyV2(snapshot) =
        compat::inspect(std::str::from_utf8(&before).unwrap()).unwrap()
    else {
        panic!("V2 must be explicitly read-only")
    };
    assert_eq!(
        snapshot.items.len(),
        30,
        "retained originals must not become executable items"
    );
    assert_eq!(snapshot.retained_inputs.len(), 1);
    assert_eq!(
        snapshot.retained_inputs[0].input.original.text,
        "original A"
    );
    assert_eq!(
        snapshot.retained_inputs[0].input.confirmation_deadline_ms,
        Some(30_000)
    );
    assert_eq!(snapshot.terminal_identities[0].settled_at_ms, 1_000);
    assert!(
        parse_snapshot(std::str::from_utf8(&before).unwrap()).is_err(),
        "legacy reader must not flatten V2"
    );
    assert!(compat::require_legacy_write(&path).is_err());
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn v2_clear_purge_drain_and_restore_preserve_originals_and_marker() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        for waiting in [0, 30] {
            let channel = ChannelId::new(7_272_220 + waiting as u64);
            let (path, before) = seed(channel.get(), &fixture(channel.get(), waiting));
            let marker = path.with_extension("dispatch");
            std::fs::write(&marker, b"independent marker evidence").unwrap();
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel);
            let restored = handle.hydrate_pending_queue_from_disk(context()).await;
            assert!(restored.persistence_error.is_some());
            assert!(handle.snapshot().await.intervention_queue.is_empty());
            assert!(
                handle
                    .take_next_soft(context())
                    .await
                    .intervention
                    .is_none()
            );
            let clear = handle.clear(context()).await;
            assert!(clear.persistence.count_unknown);
            assert!(clear.discarded_message_ids.is_empty());
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "A24 clear preserves bytes"
            );
            let drained = handle.restart_drain(context()).await;
            assert_eq!(drained.queued_count, 0);
            assert!(drained.persistence_error.is_some());
            let purge = handle.try_purge_queue(context(), false).await.unwrap();
            assert!(purge.queued_head_refused);
            assert_eq!(super::super::pending_queue_persistence::remove_channel_pending_queue_files_all_tokens(&ProviderKind::Claude, channel), 0);
            assert_eq!(
                std::fs::read(&marker).unwrap(),
                b"independent marker evidence"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    });
}

#[test]
fn v2_metadata_only_catalog_is_visible_but_legacy_recovery_is_quarantined() {
    let _root = TestRuntimeRootGuard::new();
    let ctx = context();
    let (path, before) = seed(7_272_260, &fixture(7_272_260, 0));
    let catalog = super::super::pending_queue_persistence::queued_recovery::read_catalog(
        &ctx.provider,
        &ctx.token_hash,
    );
    let entry = catalog
        .snapshots
        .get(&ChannelId::new(7_272_260))
        .expect("metadata-only channel must be visible");
    assert!(matches!(entry, compat::SnapshotRead::ReadOnlyV2(_)));
    assert!(catalog.quarantined.is_empty() && catalog.scan_errors.is_empty());
    let old = super::super::pending_queue_persistence::load_pending_queues_checked(
        &ctx.provider,
        &ctx.token_hash,
    );
    assert!(old.queues.is_empty());
    assert_eq!(
        old.quarantined.len(),
        1,
        "a V1 consumer must not silently skip V2"
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn v2_and_unknown_objects_refuse_legacy_mutation_before_unlink() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        for (index, content) in [
            "{\"version\":3,\"items\":[]}",
            "{\"version\":2,",
            "{\"items\":[]}",
        ]
        .into_iter()
        .enumerate()
        {
            let channel = ChannelId::new(7_272_270 + index as u64);
            let (path, _) = seed(channel.get(), &json!([]));
            std::fs::write(&path, content).unwrap();
            assert!(compat::inspect(content).is_err());
            assert!(compat::require_legacy_write(&path).is_err());
            let result = ChannelMailboxRegistry::default()
                .handle(channel)
                .clear(context())
                .await;
            assert!(result.persistence.count_unknown);
            assert_eq!(std::fs::read_to_string(path).unwrap(), content);
        }
    });
}

#[test]
fn v2_pre_source_inspection_does_not_invent_a_file_identity() {
    let mut snapshot = fixture(7_272_280, 0);
    snapshot["retained_inputs"][0]["input"]["attempt"] = json!({
        "nonce": "attempt-A", "provider": "codex",
        "target": {"host": "herdr", "owner": "owner", "root": "/tmp/provider", "execution_nonce": "execution-A"},
        "source": {"kind": "pre_source", "expected_session_id": null,
            "expected_transcript": null, "binding_watermark": "before-launch"}});
    let compat::SnapshotRead::ReadOnlyV2(read) = compat::inspect(&snapshot.to_string()).unwrap()
    else {
        panic!("V2")
    };
    let attempt = read.retained_inputs[0].input.attempt.as_ref().unwrap();
    assert!(matches!(
        attempt.source,
        v2_types::AttemptSource::PreSource {
            expected_session_id: None,
            expected_transcript: None,
            ..
        }
    ));
    assert_eq!(attempt.nonce, "attempt-A");
}
