use std::io::ErrorKind;

use super::*;
use crate::services::tui_o::store::fault::{self, Step};

fn record(channel: u64, seq: u64) -> CodexEnd {
    let captured_at = Utc::now();
    let context = BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: captured_at,
        execution_nonce: "a".repeat(32),
        tmux_session: "pane".into(),
        channel_id: Some(channel),
        owner_runtime_root: "/owner".into(),
        host: Some("host".into()),
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some("/codex".into()),
        first_prompt_digest: None,
        source_policy: Some("shadow".into()),
    };
    CodexEnd {
        end_id: format!("end-{seq}"),
        channel,
        provider: "codex".into(),
        tmux_session: context.tmux_session.clone(),
        execution_nonce: context.execution_nonce.clone(),
        host: "host".into(),
        owner_runtime_root: context.owner_runtime_root.clone(),
        provider_root: "/codex".into(),
        source: SourceId {
            session_id: "s1".into(),
            path: "/codex/s1.jsonl".into(),
            dev: 1,
            ino: 2,
        },
        binding_seq: seq,
        captured_at,
        ended_at: captured_at,
        basis: EndBasis::ExplicitClear,
        context,
    }
}

#[test]
fn missing_old_store_has_no_end_and_appends_keep_sixteen_newest() {
    let root = tempfile::tempdir().unwrap();
    assert!(read(root.path(), 7).unwrap().is_empty());
    assert!(!root.path().join(DIR_NAME).exists());
    for seq in 1..=17 {
        assert_eq!(append(root.path(), &record(7, seq)), Ok(Appended::Durable));
    }
    let records = read(root.path(), 7).unwrap();
    assert_eq!(
        records
            .iter()
            .map(|record| record.binding_seq)
            .collect::<Vec<_>>(),
        (2..=17).collect::<Vec<_>>()
    );
    assert!(fs::metadata(path(root.path(), 7)).unwrap().len() <= MAX_BYTES);
}

#[test]
fn corrupt_foreign_and_unknown_records_are_preserved_on_append_failure() {
    let root = tempfile::tempdir().unwrap();
    append(root.path(), &record(7, 1)).unwrap();
    let original = fs::read(path(root.path(), 7)).unwrap();
    let valid: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let mut invalid = vec![b"{broken".to_vec()];
    for change in [
        "schema",
        "channel",
        "provider",
        "zero_watermark",
        "context_nonce",
        "context_host",
        "unknown_context",
        "unknown_source",
    ] {
        let mut value = valid.clone();
        match change {
            "schema" => value["schema"] = 2.into(),
            "channel" => value["records"][0]["channel"] = 8.into(),
            "provider" => value["records"][0]["provider"] = "claude".into(),
            "zero_watermark" => value["records"][0]["binding_seq"] = 0.into(),
            "context_nonce" => {
                value["records"][0]["context"]["execution_nonce"] = "b".repeat(32).into()
            }
            "context_host" => value["records"][0]["context"]["host"] = serde_json::Value::Null,
            "unknown_context" => value["records"][0]["context"]["future_field"] = true.into(),
            "unknown_source" => value["records"][0]["source"]["future_field"] = true.into(),
            _ => unreachable!(),
        }
        invalid.push(serde_json::to_vec(&value).unwrap());
    }
    invalid.push(vec![b' '; MAX_BYTES as usize + 1]);
    for bytes in invalid {
        fs::write(path(root.path(), 7), &bytes).unwrap();
        assert!(read(root.path(), 7).is_err());
        assert!(append(root.path(), &record(7, 2)).is_err());
        assert_eq!(fs::read(path(root.path(), 7)).unwrap(), bytes);
    }
}

#[test]
fn byte_limit_evicts_oldest_but_single_oversize_and_changed_id_write_nothing() {
    let root = tempfile::tempdir().unwrap();
    for seq in 1..=16 {
        let mut record = record(7, seq);
        record.context.first_prompt_digest = Some("x".repeat(900));
        append(root.path(), &record).unwrap();
    }
    let retained = read(root.path(), 7).unwrap();
    assert!(retained.len() < MAX_RECORDS);
    assert_eq!(retained.last().unwrap().binding_seq, 16);
    assert!(fs::metadata(path(root.path(), 7)).unwrap().len() <= MAX_BYTES);
    let before = fs::read(path(root.path(), 7)).unwrap();
    let last = retained.last().unwrap();
    assert_eq!(append(root.path(), last), Ok(Appended::Durable));
    assert_eq!(fs::read(path(root.path(), 7)).unwrap(), before);
    let mut changed = last.clone();
    changed.binding_seq += 1;
    assert!(append(root.path(), &changed).is_err());
    let mut oversized = record(7, 17);
    oversized.context.first_prompt_digest = Some("x".repeat(MAX_BYTES as usize));
    assert!(append(root.path(), &oversized).is_err());
    assert_eq!(fs::read(path(root.path(), 7)).unwrap(), before);
}

#[test]
fn writes_are_serialized_for_same_channel_and_isolated_by_root_and_channel() {
    let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::thread::scope(|scope| {
        for seq in 1..=16 {
            let (first, second) = (first.path(), second.path());
            scope.spawn(move || {
                append(first, &record(7, seq)).unwrap();
                append(first, &record(8, seq)).unwrap();
                append(second, &record(7, seq)).unwrap();
            });
        }
    });
    for (root, channel) in [(first.path(), 7), (first.path(), 8), (second.path(), 7)] {
        let mut seqs = read(root, channel)
            .unwrap()
            .iter()
            .map(|record| record.binding_seq)
            .collect::<Vec<_>>();
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=16).collect::<Vec<_>>());
    }
    assert!(read(second.path(), 8).unwrap().is_empty());
}

#[test]
fn failed_write_preserves_old_bytes_and_published_unsynced_is_never_durable() {
    let root = tempfile::tempdir().unwrap();
    append(root.path(), &record(7, 1)).unwrap();
    let before = fs::read(path(root.path(), 7)).unwrap();
    let dir = root.path().join(DIR_NAME);
    let write = fault::plant(&dir, Step::Write, ErrorKind::StorageFull, Some(1));
    assert!(append(root.path(), &record(7, 2)).is_err());
    drop(write);
    assert_eq!(fs::read(path(root.path(), 7)).unwrap(), before);
    let end = record(7, 3);
    let sync = fault::plant(&dir, Step::ReplaceDirSync, ErrorKind::Other, Some(1));
    assert!(matches!(
        append(root.path(), &end),
        Ok(Appended::PublishedUnsynced(_))
    ));
    drop(sync);
    assert_eq!(
        read(root.path(), 7)
            .unwrap()
            .iter()
            .map(|record| record.binding_seq)
            .collect::<Vec<_>>(),
        [1, 3]
    );
    assert_eq!(append(root.path(), &end), Ok(Appended::Durable));
    assert_eq!(read(root.path(), 7).unwrap().len(), 2);
}
