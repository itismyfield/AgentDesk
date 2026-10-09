use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::{
    CaptureOutcome, CaptureSource, CapturedRecord, ShadowProvider, SourceId,
};
use crate::services::tui_o::store::durable::fault::{self, Step};
use crate::services::tui_o::store::tests::{enabled, initialized};
use crate::services::tui_o::store::{ChannelStore, OEra, OStore};
use crate::services::tui_o::writer::pieces::{Derived, UnitDeriver};
use crate::services::tui_o::writer::rotation::provenance::{origin, replay};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::{
    CodexEpisodeDeny, ExecutionProofRef, TurnEpisodeRef,
};

fn fixture(
    text: &str,
) -> (
    tempfile::TempDir,
    OStore,
    OEra,
    ChannelStore,
    SourceId,
    Vec<CapturedRecord>,
) {
    let runtime = tempfile::tempdir().unwrap();
    let path = runtime.path().join("rollout.jsonl");
    std::fs::write(&path, text).unwrap();
    let source = source_id_for("native-session", &path).unwrap();
    let store = enabled(runtime.path());
    let era = store
        .begin_era(&[7], chrono::Utc::now(), |channel| {
            Ok(initialized(channel, Vec::new()))
        })
        .unwrap();
    let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
    channel.attach_source(&source).unwrap();
    let mut capture = SourceCapture::open(source.clone(), 0).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(1 << 20) else {
        panic!("capture failed")
    };
    channel
        .append_spool(&batch, &capture.prefix_hash())
        .unwrap();
    (runtime, store, era, channel, source, batch.records)
}

fn span(source: &SourceId, id: &str, start: u64, end: Option<u64>) -> CodexEpisodeSpan {
    CodexEpisodeSpan {
        execution: ExecutionProofRef {
            owner_runtime_root: "/runtime/owner".into(),
            tmux_session: "pane".into(),
            execution_nonce: format!("execution-{id}"),
            proof_seq: 4,
            source: source.clone(),
        },
        episode: TurnEpisodeRef {
            channel_id: 7,
            user_message_id: if id == "old" { 10 } else { 20 },
            request_owner_id: 8,
            turn_nonce: format!("turn-{id}"),
            native_turn_id: id.into(),
        },
        delivery_channel_id: 7,
        offset_authority_channel_id: 7,
        generation_mtime_ns: 1,
        start,
        end,
    }
}

#[test]
fn episode_storage_closes_once_and_rejects_identity_boundary_or_deny_loss() {
    let (_runtime, store, era, mut channel, source, records) = fixture("{}\n{}\n");
    let open = span(&source, "old", 0, None);
    channel.persist_codex_span(open.clone()).unwrap();
    let mut closed = open.clone();
    closed.end = Some(records[0].end);
    channel.persist_codex_span(closed.clone()).unwrap();
    channel.persist_codex_span(closed.clone()).unwrap();
    let mut rotation = channel.rotation().unwrap();
    rotation.codex_denies.push(CodexEpisodeDeny {
        execution: closed.execution.clone(),
        episode: closed.episode.clone(),
    });
    channel.write_rotation(&rotation).unwrap();
    let durable = channel.rotation().unwrap();
    let mut mutations = vec![
        open,
        closed.clone(),
        closed.clone(),
        closed.clone(),
        closed.clone(),
        closed.clone(),
    ];
    mutations[1].end = Some(records[1].end);
    mutations[2].start = 1;
    mutations[3].generation_mtime_ns = 2;
    mutations[4].episode.request_owner_id = 9;
    mutations[5].execution.execution_nonce = "current-nonce".into();
    for changed in mutations {
        let mut next = durable.clone();
        next.codex_spans = vec![changed];
        assert!(
            channel.write_rotation(&next).is_err(),
            "immutable span changed"
        );
    }
    for drop_spans in [false, true] {
        let mut next = durable.clone();
        if drop_spans {
            next.codex_spans.clear();
        } else {
            next.codex_denies.clear();
        }
        assert!(channel.write_rotation(&next).is_err(), "provenance dropped");
    }
    assert_eq!(
        store
            .open_channel(&era, 7)
            .unwrap()
            .unwrap()
            .rotation()
            .unwrap(),
        durable
    );
}

#[test]
fn overlapping_renumbered_sources_and_conflicting_episode_owners_are_rejected() {
    let (_runtime, _store, _era, mut channel, source, records) = fixture("{}\n{}\n");
    let old = span(&source, "old", 0, Some(records[0].end));
    channel.persist_codex_span(old.clone()).unwrap();
    let mut adjacent = span(&source, "new", records[0].end, Some(records[1].end));
    adjacent.execution.source.dev ^= 1;
    channel.persist_codex_span(adjacent.clone()).unwrap();
    let rotation = channel.rotation().unwrap();
    for variant in 0..5 {
        let mut next = rotation.clone();
        let mut conflict = span(&source, "third", records[1].end, Some(records[1].end + 3));
        match variant {
            0 => {
                conflict.start = 0;
                conflict.execution.source.dev ^= 2;
            }
            1 => {
                conflict.episode = old.episode.clone();
                conflict.episode.request_owner_id += 1;
            }
            2 => conflict.offset_authority_channel_id = 8,
            3 => conflict.episode.native_turn_id = old.episode.native_turn_id.clone(),
            _ => {
                conflict.execution = old.execution.clone();
                conflict.execution.source.path = "/other".into();
            }
        }
        next.codex_spans.push(conflict);
        assert!(
            channel.write_rotation(&next).is_err(),
            "ambiguous provenance accepted: {variant}"
        );
    }
}

#[test]
fn a_failed_span_replace_never_publishes_a_closed_range() {
    let (runtime, store, era, mut channel, source, records) = fixture("{}\n");
    let open = span(&source, "old", 0, None);
    channel.persist_codex_span(open.clone()).unwrap();
    let mut closed = open.clone();
    closed.end = Some(records[0].end);
    let failure = fault::plant(
        runtime.path(),
        Step::Write,
        std::io::ErrorKind::StorageFull,
        Some(1),
    );
    let published = channel.persist_codex_span(closed);
    assert!(
        published.is_err(),
        "closed range escaped a failed durable write"
    );
    assert_eq!(
        channel.rotation().unwrap().codex_spans,
        std::slice::from_ref(&open)
    );
    assert_eq!(
        origin(&channel.rotation().unwrap(), &source, &records[0]),
        None
    );
    drop(failure);
    assert_eq!(
        store
            .open_channel(&era, 7)
            .unwrap()
            .unwrap()
            .rotation()
            .unwrap()
            .codex_spans,
        [open]
    );
}

#[test]
fn origin_requires_one_complete_closed_native_range_without_union_or_current_identity() {
    let (_runtime, _store, _era, _channel, source, records) = fixture("{}\n{}\n");
    let closed = span(&source, "old", 0, Some(records[0].end));
    let mut rotation = Rotation::default();
    rotation.codex_spans.push(closed.clone());
    let owed = origin(&rotation, &source, &records[0]).unwrap();
    assert_eq!(owed.span.execution.execution_nonce, "execution-old");
    assert_eq!(owed.range, records[0].start..records[0].end);
    assert_eq!(origin(&rotation, &source, &records[1]), None);
    let mut renumbered = source.clone();
    renumbered.dev ^= 1;
    assert_eq!(origin(&rotation, &renumbered, &records[0]), Some(owed));
    rotation.codex_spans.push(closed);
    assert_eq!(
        origin(&rotation, &source, &records[0]),
        None,
        "duplicate is Unknown"
    );
    rotation.codex_spans[1].start = records[0].end;
    rotation.codex_spans[1].end = Some(records[1].end);
    let mut crossing = records[0].clone();
    crossing.end = records[1].end;
    crossing.line = b"{}\n{}".to_vec();
    assert_eq!(
        origin(&rotation, &source, &crossing),
        None,
        "span union is forbidden"
    );
    rotation.codex_spans[0].end = None;
    assert_eq!(
        origin(&rotation, &source, &records[0]),
        None,
        "EOF cannot close an open span"
    );
    rotation.codex_spans.clear();
    assert_eq!(
        origin(&rotation, &source, &records[0]),
        None,
        "legacy has no verified attribution"
    );
}

#[test]
fn durable_spool_replay_preserves_old_and_new_unit_keys_pieces_and_payloads() {
    let body = |id, text| {
        format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"{id}\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{text}\"}}]}}}}\n"
        )
    };
    let text = body("old-body", "old output") + &body("new-body", "new output");
    let (_runtime, store, era, mut channel, source, records) = fixture(&text);
    channel
        .persist_codex_span(span(&source, "old", 0, Some(records[0].end)))
        .unwrap();
    channel
        .persist_codex_span(span(&source, "new", records[0].end, Some(records[1].end)))
        .unwrap();
    drop(channel);
    let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
    let work = replay(
        &mut channel,
        &source,
        &mut UnitDeriver::new(7, ShadowProvider::Codex),
    )
    .unwrap();
    let mut plain = UnitDeriver::new(7, ShadowProvider::Codex);
    let expected: Vec<_> = records
        .iter()
        .flat_map(|record| plain.derive(record))
        .collect();
    assert_eq!(
        work.iter()
            .map(|work| work.derived.clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(work.len(), 2);
    for (work, id, payload) in [
        (&work[0], "old", "old output"),
        (&work[1], "new", "new output"),
    ] {
        let Derived::Piece(piece) = &work.derived else {
            panic!("piece absent")
        };
        assert_eq!(piece.unit_key.native_key, format!("{id}-body"));
        assert_eq!((piece.index, piece.payload.as_str()), (0, payload));
        assert_eq!(
            work.origin.as_ref().unwrap().span.episode.native_turn_id,
            id
        );
    }
}

#[test]
fn additive_schema_reads_legacy_and_old_parser_rollback_loses_attribution() {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct OldRotation {
        links: std::collections::BTreeMap<String, serde_json::Value>,
        successors: std::collections::BTreeMap<String, serde_json::Value>,
    }
    let (_runtime, _store, _era, _channel, source, records) = fixture("{}\n");
    let legacy: Rotation = serde_json::from_str("{\"links\":{},\"successors\":{}}").unwrap();
    assert_eq!(
        serde_json::to_string(&legacy).unwrap(),
        "{\"links\":{},\"successors\":{}}"
    );
    let mut rotation = legacy;
    rotation
        .codex_spans
        .push(span(&source, "old", 0, Some(records[0].end)));
    rotation.codex_denies.push(CodexEpisodeDeny {
        execution: rotation.codex_spans[0].execution.clone(),
        episode: rotation.codex_spans[0].episode.clone(),
    });
    let bytes = serde_json::to_vec(&rotation).unwrap();
    assert_eq!(
        serde_json::from_slice::<Rotation>(&bytes).unwrap(),
        rotation
    );
    let old: OldRotation = serde_json::from_slice(&bytes).unwrap();
    let rolled_back: Rotation = serde_json::from_slice(&serde_json::to_vec(&old).unwrap()).unwrap();
    assert!(rolled_back.codex_spans.is_empty() && rolled_back.codex_denies.is_empty());
    assert_eq!(
        origin(&rolled_back, &source, &records[0]),
        None,
        "rollback is Unknown"
    );
}

#[test]
fn invalid_loaded_owner_channel_or_overlap_metadata_keeps_work_unknown() {
    let text = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"body\",\"content\":[{\"type\":\"output_text\",\"text\":\"kept output\"}]}}\n";
    let (runtime, _store, _era, mut channel, source, records) = fixture(text);
    let stored = span(&source, "old", 0, Some(records[0].end));
    channel.persist_codex_span(stored).unwrap();
    let rotation = channel.rotation().unwrap();
    for variant in 0..4 {
        let mut corrupt = rotation.clone();
        match variant {
            0 => corrupt.codex_spans[0].offset_authority_channel_id = 8,
            1 => corrupt.codex_spans[0].episode.request_owner_id = 0,
            2 => {
                let mut other = corrupt.codex_spans[0].clone();
                other.episode.request_owner_id += 1;
                corrupt.codex_spans.push(other);
            }
            _ => {
                corrupt.codex_spans[0].episode.channel_id = 8;
                corrupt.codex_spans[0].delivery_channel_id = 8;
                corrupt.codex_spans[0].offset_authority_channel_id = 8;
            }
        }
        let path = runtime
            .path()
            .join(crate::services::tui_o::store::STORE_DIR_NAME)
            .join("7")
            .join(super::super::BOUNDARY_FILE);
        std::fs::write(path, serde_json::to_vec(&corrupt).unwrap()).unwrap();
        let work = replay(
            &mut channel,
            &source,
            &mut UnitDeriver::new(7, ShadowProvider::Codex),
        )
        .unwrap();
        assert_eq!(work.len(), 1, "unknown work remains owed");
        assert_eq!(
            work[0].origin, None,
            "invalid metadata gained provenance: {variant}"
        );
        let Derived::Piece(piece) = &work[0].derived else {
            panic!("piece absent")
        };
        assert_eq!(piece.payload, "kept output");
    }
}
