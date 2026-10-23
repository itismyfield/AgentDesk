use std::io::ErrorKind;

use super::*;
use crate::services::tui_o::store::fault::{self, Step};

fn context(channel: u64) -> BindingContext {
    BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: Utc::now(),
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
    }
}

fn record(channel: u64, seq: u64) -> CodexEnd {
    let context = context(channel);
    let captured_at = context.created_at;
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
        context: ContextSnapshot::from(&context),
        successor: None,
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

#[test]
fn context_snapshot_preserves_raw_alias_root_beside_the_canonical_root() {
    let root = tempfile::tempdir().unwrap();
    let canonical = root.path().join("codex");
    fs::create_dir(&canonical).unwrap();
    let canonical = canonical.canonicalize().unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&canonical, &alias).unwrap();
    let mut raw = context(7);
    raw.provider_root = Some(alias.clone());
    let mut end = record(7, 1);
    end.provider_root = canonical.clone();
    end.source.path = canonical.join("s1.jsonl");
    end.context = ContextSnapshot::from(&raw);
    assert_eq!(append(root.path(), &end), Ok(Appended::Durable));
    let restored = read(root.path(), 7).unwrap().remove(0);
    assert_eq!(restored.context, ContextSnapshot::from(&raw));
    assert_eq!(restored.context.provider_root, Some(alias));
    assert_eq!(restored.provider_root, canonical);
    assert_ne!(
        restored.context.provider_root.as_ref(),
        Some(&restored.provider_root)
    );
    let before = fs::read(path(root.path(), 7)).unwrap();
    end.context.provider_root = None;
    assert!(append(root.path(), &end).is_err());
    assert_eq!(fs::read(path(root.path(), 7)).unwrap(), before);
}

#[test]
fn snapshot_equality_covers_each_current_identity_and_fresh_evidence_field() {
    let original = context(7);
    let snapshot = ContextSnapshot::from(&original);
    assert_eq!(snapshot, ContextSnapshot::from(&original.clone()));
    for field in [
        "schema",
        "provider",
        "created_at",
        "execution_nonce",
        "tmux_session",
        "channel_id",
        "owner_runtime_root",
        "host",
        "expected_native_session_id",
        "launch_mode",
        "provider_root",
        "first_prompt_digest",
        "source_policy",
    ] {
        let mut changed = original.clone();
        match field {
            "schema" => changed.schema += 1,
            "provider" => changed.provider = "claude".into(),
            "created_at" => changed.created_at += chrono::Duration::seconds(1),
            "execution_nonce" => changed.execution_nonce = "b".repeat(32),
            "tmux_session" => changed.tmux_session = "other-pane".into(),
            "channel_id" => changed.channel_id = Some(8),
            "owner_runtime_root" => changed.owner_runtime_root = "/other-owner".into(),
            "host" => changed.host = Some("other-host".into()),
            "expected_native_session_id" => changed.expected_native_session_id = Some("s1".into()),
            "launch_mode" => changed.launch_mode = "resume".into(),
            "provider_root" => changed.provider_root = Some("/raw/alias".into()),
            "first_prompt_digest" => changed.first_prompt_digest = Some("sha256:prompt".into()),
            "source_policy" => changed.source_policy = Some("legacy".into()),
            _ => unreachable!(),
        }
        assert_ne!(
            snapshot,
            ContextSnapshot::from(&changed),
            "{field} must remain part of equality"
        );
    }
}

#[test]
fn binding_context_extensions_do_not_change_the_snapshot_storage_format() {
    let root = tempfile::tempdir().unwrap();
    let original = context(7);
    let snapshot = ContextSnapshot::from(&original);
    let mut extended = serde_json::to_value(&original).unwrap();
    extended["future_optional_evidence"] = serde_json::json!({"unrelated":"launch metadata"});
    let expanded: BindingContext = serde_json::from_value(extended.clone()).unwrap();
    assert!(serde_json::from_value::<ContextSnapshot>(extended).is_err());
    assert_eq!(ContextSnapshot::from(&expanded), snapshot);
    let mut end = record(7, 1);
    end.context = snapshot;
    append(root.path(), &end).unwrap();
    let before = fs::read(path(root.path(), 7)).unwrap();
    end.context = ContextSnapshot::from(&expanded);
    assert_eq!(append(root.path(), &end), Ok(Appended::Durable));
    assert_eq!(fs::read(path(root.path(), 7)).unwrap(), before);
    assert_eq!(read(root.path(), 7).unwrap(), [end]);
}

fn successor(end: &CodexEnd) -> SuccessorSeal {
    let mut context = end.context.clone();
    context.execution_nonce = "b".repeat(32);
    context.launch_mode = "fresh".into();
    context.expected_native_session_id = None;
    context.source_policy = Some("shadow".into());
    SuccessorSeal {
        execution_nonce: context.execution_nonce.clone(),
        binding_seq: end.binding_seq + 1,
        source: SourceId {
            session_id: "s2".into(),
            path: end.provider_root.join("s2.jsonl"),
            dev: 1,
            ino: 3,
        },
        context,
    }
}

#[test]
fn legacy_end_reopens_unsealed_and_direct_replacement_round_trips() {
    let root = tempfile::tempdir().unwrap();
    let mut end = record(7, 1);
    end.basis = EndBasis::DirectReplacement;
    append(root.path(), &end).unwrap();
    let raw: serde_json::Value =
        serde_json::from_slice(&fs::read(path(root.path(), 7)).unwrap()).unwrap();
    assert!(raw["records"][0].get("successor").is_none());
    assert_eq!(read(root.path(), 7).unwrap(), [end.clone()]);
    let seal_value = successor(&end);
    assert_eq!(
        seal(root.path(), 7, &end.end_id, &seal_value),
        Ok(Appended::Durable)
    );
    end.successor = Some(seal_value);
    assert_eq!(read(root.path(), 7).unwrap(), [end]);
}

#[test]
fn seal_cas_first_writer_wins_and_end_retry_preserves_the_seal() {
    let root = tempfile::tempdir().unwrap();
    let end = record(7, 1);
    append(root.path(), &end).unwrap();
    let first = successor(&end);
    let mut other = first.clone();
    other.execution_nonce = "c".repeat(32);
    other
        .context
        .execution_nonce
        .clone_from(&other.execution_nonce);
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for candidate in [&first, &other] {
            let (root, end, barrier) = (root.path(), &end, &barrier);
            handles.push(scope.spawn(move || {
                barrier.wait();
                (candidate.clone(), seal(root, 7, &end.end_id, candidate))
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    let (winner, _) = results.iter().find(|(_, result)| result.is_ok()).unwrap();
    let (loser, _) = results.iter().find(|(_, result)| result.is_err()).unwrap();
    assert_eq!(
        read(root.path(), 7).unwrap()[0].successor.as_ref(),
        Some(winner)
    );
    let before = fs::read(path(root.path(), 7)).unwrap();
    assert!(seal(root.path(), 7, &end.end_id, loser).is_err());
    assert_eq!(append(root.path(), &end), Ok(Appended::Durable));
    assert_eq!(fs::read(path(root.path(), 7)).unwrap(), before);
}

#[test]
fn seal_write_failure_reopens_unsealed_and_unsynced_retry_reestablishes_durability() {
    let root = tempfile::tempdir().unwrap();
    let end = record(7, 1);
    let candidate = successor(&end);
    append(root.path(), &end).unwrap();
    let dir = root.path().join(DIR_NAME);
    let before = fs::read(path(root.path(), 7)).unwrap();
    let write = fault::plant(&dir, Step::Write, ErrorKind::StorageFull, Some(1));
    assert!(seal(root.path(), 7, &end.end_id, &candidate).is_err());
    drop(write);
    assert_eq!(fs::read(path(root.path(), 7)).unwrap(), before);
    assert_eq!(read(root.path(), 7).unwrap()[0].successor, None);
    let sync = fault::plant(&dir, Step::ReplaceDirSync, ErrorKind::Other, Some(1));
    assert!(matches!(
        seal(root.path(), 7, &end.end_id, &candidate),
        Ok(Appended::PublishedUnsynced(_))
    ));
    drop(sync);
    assert_eq!(
        read(root.path(), 7).unwrap()[0].successor.as_ref(),
        Some(&candidate)
    );
    let write = fault::plant(&dir, Step::Write, ErrorKind::StorageFull, Some(1));
    assert!(matches!(
        seal(root.path(), 7, &end.end_id, &candidate),
        Ok(Appended::PublishedUnsynced(_))
    ));
    drop(write);
    assert_eq!(
        seal(root.path(), 7, &end.end_id, &candidate),
        Ok(Appended::Durable)
    );
}

#[test]
fn stale_seal_never_recreates_missing_or_evicted_end() {
    let root = tempfile::tempdir().unwrap();
    let end = record(7, 1);
    let candidate = successor(&end);
    assert!(seal(root.path(), 7, &end.end_id, &candidate).is_err());
    assert!(!root.path().join(DIR_NAME).exists());
    for seq in 1..=17 {
        append(root.path(), &record(7, seq)).unwrap();
    }
    let before = fs::read(path(root.path(), 7)).unwrap();
    assert!(seal(root.path(), 7, &end.end_id, &candidate).is_err());
    assert_eq!(fs::read(path(root.path(), 7)).unwrap(), before);
    assert!(
        read(root.path(), 7)
            .unwrap()
            .iter()
            .all(|end| end.successor.is_none())
    );
}

#[test]
fn seal_rejects_foreign_resume_old_watermark_and_oversized_evidence_without_writing() {
    let root = tempfile::tempdir().unwrap();
    let end = record(7, 1);
    append(root.path(), &end).unwrap();
    let before = fs::read(path(root.path(), 7)).unwrap();
    for field in [
        "nonce",
        "watermark",
        "channel",
        "pane",
        "host",
        "owner",
        "context_nonce",
        "resume",
        "expected_native_id",
        "source_policy",
        "provider_root",
        "source_root",
        "oversized",
    ] {
        let mut candidate = successor(&end);
        match field {
            "nonce" => {
                candidate.execution_nonce.clone_from(&end.execution_nonce);
                candidate
                    .context
                    .execution_nonce
                    .clone_from(&end.execution_nonce);
            }
            "watermark" => candidate.binding_seq = end.binding_seq,
            "channel" => candidate.context.channel_id = Some(8),
            "pane" => candidate.context.tmux_session = "other".into(),
            "host" => candidate.context.host = Some("other".into()),
            "owner" => candidate.context.owner_runtime_root = "/other".into(),
            "context_nonce" => candidate.context.execution_nonce = "c".repeat(32),
            "resume" => candidate.context.launch_mode = "resume".into(),
            "expected_native_id" => {
                candidate.context.expected_native_session_id = Some("s2".into())
            }
            "source_policy" => candidate.context.source_policy = Some("verified".into()),
            "provider_root" => candidate.context.provider_root = None,
            "source_root" => candidate.source.path = "/foreign/s2.jsonl".into(),
            "oversized" => {
                candidate.context.first_prompt_digest = Some("x".repeat(MAX_BYTES as usize))
            }
            _ => unreachable!(),
        }
        assert!(
            seal(root.path(), 7, &end.end_id, &candidate).is_err(),
            "{field}"
        );
        assert_eq!(fs::read(path(root.path(), 7)).unwrap(), before, "{field}");
    }
}

#[test]
fn malformed_seal_is_unavailable_and_preserved_instead_of_falling_back_to_unsealed() {
    let root = tempfile::tempdir().unwrap();
    let end = record(7, 1);
    let candidate = successor(&end);
    append(root.path(), &end).unwrap();
    seal(root.path(), 7, &end.end_id, &candidate).unwrap();
    let raw: serde_json::Value =
        serde_json::from_slice(&fs::read(path(root.path(), 7)).unwrap()).unwrap();
    for field in [
        "unknown",
        "invalid_nonce",
        "foreign_context",
        "unknown_source",
    ] {
        let mut value = raw.clone();
        match field {
            "unknown" => value["records"][0]["successor"]["future_field"] = true.into(),
            "invalid_nonce" => value["records"][0]["successor"]["execution_nonce"] = "bad".into(),
            "foreign_context" => {
                value["records"][0]["successor"]["context"]["channel_id"] = 8.into()
            }
            "unknown_source" => {
                value["records"][0]["successor"]["source"]["future_field"] = true.into()
            }
            _ => unreachable!(),
        }
        let bytes = serde_json::to_vec(&value).unwrap();
        fs::write(path(root.path(), 7), &bytes).unwrap();
        assert!(read(root.path(), 7).is_err(), "{field}");
        assert!(
            seal(root.path(), 7, &end.end_id, &candidate).is_err(),
            "{field}"
        );
        assert_eq!(fs::read(path(root.path(), 7)).unwrap(), bytes);
    }
}

#[test]
fn concurrent_panes_keep_both_append_and_seal_updates_in_the_shared_channel() {
    let root = tempfile::tempdir().unwrap();
    std::thread::scope(|scope| {
        for seq in 1..=4 {
            let root = root.path();
            scope.spawn(move || {
                let mut end = record(7, seq * 2);
                end.tmux_session = format!("pane-{seq}");
                end.context.tmux_session.clone_from(&end.tmux_session);
                let candidate = successor(&end);
                append(root, &end).unwrap();
                assert_eq!(
                    seal(root, 7, &end.end_id, &candidate),
                    Ok(Appended::Durable)
                );
            });
        }
    });
    let records = read(root.path(), 7).unwrap();
    assert_eq!(records.len(), 4);
    assert!(records.iter().all(|end| end.successor.is_some()));
}

#[test]
fn seal_byte_cap_evicts_only_older_records_and_never_reports_its_target_evicted() {
    let root = tempfile::tempdir().unwrap();
    let mut ends = (1..=3).map(|seq| record(7, seq)).collect::<Vec<_>>();
    ends[0].context.first_prompt_digest = Some(String::new());
    let padding = MAX_BYTES as usize - encode(&ends).unwrap().len() - 1;
    ends[0].context.first_prompt_digest = Some("x".repeat(padding));
    for end in &ends {
        append(root.path(), end).unwrap();
    }
    let before = fs::read(path(root.path(), 7)).unwrap();
    assert_eq!(before.len(), MAX_BYTES as usize - 1);
    let mut oldest_successor = successor(&ends[0]);
    oldest_successor.context.first_prompt_digest = None;
    let mut single = ends[0].clone();
    single.successor = Some(oldest_successor.clone());
    assert!(encode(&[single]).unwrap().len() < MAX_BYTES as usize);
    assert!(seal(root.path(), 7, &ends[0].end_id, &oldest_successor).is_err());
    assert_eq!(fs::read(path(root.path(), 7)).unwrap(), before);
    let newest_successor = successor(&ends[2]);
    assert_eq!(
        seal(root.path(), 7, &ends[2].end_id, &newest_successor),
        Ok(Appended::Durable)
    );
    let records = read(root.path(), 7).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].end_id, ends[1].end_id);
    assert_eq!(records[1].successor.as_ref(), Some(&newest_successor));
    assert!(fs::metadata(path(root.path(), 7)).unwrap().len() <= MAX_BYTES);
}
