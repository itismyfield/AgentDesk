use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource, CapturedRecord, SourceId};
use crate::services::tui_o::store::durable::fault::{self, Step};
use crate::services::tui_o::store::rotation::{BOUNDARY_FILE, Rotation};
use crate::services::tui_o::store::tests::{enabled, initialized};
use crate::services::tui_o::store::{OEra, OStore, STORE_DIR_NAME, StoreError};
use crate::services::tui_o::writer::rotation::provenance::origin;
use crate::services::tui_prompt_dedupe::codex_verified::provenance::{
    ExecutionProofRef, TurnEpisodeRef,
};

struct Fixture {
    runtime: tempfile::TempDir,
    store: OStore,
    era: OEra,
    channel: ChannelStore,
    source: SourceId,
    records: Vec<CapturedRecord>,
}

impl Fixture {
    fn new(text: &str) -> Self {
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
        (channel.append_spool(&batch, &capture.prefix_hash())).unwrap();
        let records = batch.records;
        Self {
            runtime,
            store,
            era,
            channel,
            source,
            records,
        }
    }

    fn path(&self, file: &str) -> std::path::PathBuf {
        self.runtime
            .path()
            .join(STORE_DIR_NAME)
            .join("7")
            .join(file)
    }

    fn present(&self) -> CodexProvenance {
        match self.channel.codex_provenance() {
            Sidecar::Present(read) => read.raw().clone(),
            other => panic!("sidecar not present: {other:?}"),
        }
    }

    fn reopened(&self) -> Sidecar {
        let channel = self.store.open_channel(&self.era, 7).unwrap().unwrap();
        channel.codex_provenance()
    }
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

fn deny(span: &CodexEpisodeSpan) -> CodexEpisodeDeny {
    let (execution, episode) = (span.execution.clone(), span.episode.clone());
    CodexEpisodeDeny { execution, episode }
}

fn rejected<T: std::fmt::Debug>(result: Result<T, StoreError>) -> bool {
    matches!(result, Err(StoreError::Rejected(_)))
}

#[test]
fn s2a_existing_owed_reader_decodes_schema2_without_migrating_or_writing() {
    use crate::services::tui_o::writer::owed::{Reevaluation, reevaluate};
    let mut fixture = Fixture::new("{}\n{}\n");
    fixture.channel.init_codex_provenance().unwrap();
    let closed = span(&fixture.source, "old", 0, Some(fixture.records[0].end));
    fixture.channel.persist_codex_span(closed.clone()).unwrap();
    fixture.channel.persist_codex_deny(deny(&closed)).unwrap();
    let mut document = serde_json::to_value(fixture.present()).unwrap();
    document["schema"] = 2.into();
    document["floors"] = serde_json::json!([]);
    let path = fixture.path(CODEX_PROVENANCE_FILE);
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let before = std::fs::read(&path).unwrap();
    SIDECAR_IO.with(|io| io.set((0, 0)));
    assert_eq!(
        reevaluate(&mut fixture.channel, &mut []).unwrap(),
        Reevaluation::Present { attributed: 0 },
        "the gated schema2 reader must decode the new envelope"
    );
    assert_eq!(SIDECAR_IO.with(std::cell::Cell::get), (1, 0));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(fixture.present().spans, [closed]);
}

#[test]
fn episode_storage_closes_once_and_rejects_identity_or_deny_loss_without_failing_the_store() {
    let mut fixture = Fixture::new("{}\n{}\n");
    let (source, end) = (fixture.source.clone(), fixture.records[0].end);
    fixture.channel.init_codex_provenance().unwrap();
    let open = span(&source, "old", 0, None);
    fixture.channel.persist_codex_span(open.clone()).unwrap();
    let mut closed = open.clone();
    closed.end = Some(end);
    fixture.channel.persist_codex_span(closed.clone()).unwrap();
    fixture.channel.persist_codex_span(closed.clone()).unwrap();
    fixture.channel.persist_codex_deny(deny(&closed)).unwrap();
    let durable = fixture.present();
    assert_eq!((durable.spans.len(), durable.denies.len()), (1, 1));
    let mut mutations = vec![open, closed.clone(), closed.clone(), closed.clone()];
    mutations[1].end = Some(fixture.records[1].end);
    mutations[2].start = 1;
    mutations[3].generation_mtime_ns = 2;
    let mut owner = closed.clone();
    owner.episode.request_owner_id = 9;
    let mut nonce = closed.clone();
    nonce.execution.execution_nonce = "current-nonce".into();
    for changed in mutations.into_iter().chain([owner, nonce]) {
        assert!(rejected(fixture.channel.persist_codex_span(changed)));
    }
    for drop_spans in [false, true] {
        let dropped = fixture
            .channel
            .change_codex_provenance(|next| match drop_spans {
                true => next.spans.clear(),
                false => next.denies.clear(),
            });
        assert!(rejected(dropped), "provenance dropped");
    }
    assert_eq!(fixture.channel.io_failure(), None, "Rejected is not a halt");
    fixture.channel.set_binding_checkpoint(1).unwrap();
    let reopened = fixture.reopened();
    assert!(matches!(&reopened, Sidecar::Present(read) if *read.raw() == durable));
}

#[test]
fn overlapping_renumbered_sources_and_conflicting_episode_owners_are_rejected() {
    let mut fixture = Fixture::new("{}\n{}\n");
    let (source, records) = (fixture.source.clone(), fixture.records.clone());
    fixture.channel.init_codex_provenance().unwrap();
    let old = span(&source, "old", 0, Some(records[0].end));
    fixture.channel.persist_codex_span(old.clone()).unwrap();
    let mut adjacent = span(&source, "new", records[0].end, Some(records[1].end));
    adjacent.execution.source.dev ^= 1;
    fixture.channel.persist_codex_span(adjacent).unwrap();
    let durable = fixture.present();
    for variant in 0..5 {
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
        let stored = fixture.channel.persist_codex_span(conflict);
        assert!(rejected(stored), "ambiguous provenance accepted: {variant}");
        assert_eq!(fixture.present(), durable, "conflict overwrote: {variant}");
    }
    assert_eq!(fixture.channel.io_failure(), None);
}

#[test]
fn a_failed_span_replace_publishes_no_closed_range_and_fails_the_store() {
    let mut fixture = Fixture::new("{}\n");
    let source = fixture.source.clone();
    fixture.channel.init_codex_provenance().unwrap();
    let open = span(&source, "old", 0, None);
    fixture.channel.persist_codex_span(open.clone()).unwrap();
    let mut closed = open.clone();
    closed.end = Some(fixture.records[0].end);
    let path = fixture.runtime.path().to_path_buf();
    let failure = fault::plant(&path, Step::Write, std::io::ErrorKind::StorageFull, Some(1));
    let published = fixture.channel.persist_codex_span(closed);
    assert!(matches!(published, Err(StoreError::Io(_))));
    assert_eq!(
        fixture.channel.io_failure(),
        Some(std::io::ErrorKind::StorageFull)
    );
    let stored = fixture.present();
    assert_eq!(stored.spans, std::slice::from_ref(&open));
    let stored = ValidatedCodexProvenance::checked(7, stored).unwrap();
    assert_eq!(origin(&stored, &source, &fixture.records[0]), None);
    drop(failure);
    let Sidecar::Present(reopened) = fixture.reopened() else {
        panic!("sidecar lost")
    };
    assert_eq!(reopened.spans(), [open]);
}

#[test]
fn origin_requires_one_complete_closed_native_range_without_union_or_current_identity() {
    let fixture = Fixture::new("{}\n{}\n");
    let (source, records) = (&fixture.source, &fixture.records);
    let closed = span(source, "old", 0, Some(records[0].end));
    let validated = |spans: Vec<CodexEpisodeSpan>| {
        let denies = Vec::new();
        let read = CodexProvenance {
            schema: SCHEMA,
            channel: 7,
            spans,
            denies,
        };
        ValidatedCodexProvenance::checked(7, read).unwrap()
    };
    let snapshot = validated(vec![closed.clone()]);
    let owed = origin(&snapshot, source, &records[0]).unwrap();
    assert_eq!(owed.span.execution.execution_nonce, "execution-old");
    assert_eq!(owed.range, records[0].start..records[0].end);
    assert_eq!(origin(&snapshot, source, &records[1]), None);
    let mut renumbered = source.clone();
    renumbered.dev ^= 1;
    assert_eq!(origin(&snapshot, &renumbered, &records[0]), Some(owed));
    let mut next = span(source, "new", records[0].end, Some(records[1].end));
    next.execution.execution_nonce = "execution-new".into();
    let snapshot = validated(vec![closed.clone(), next]);
    let mut crossing = records[0].clone();
    crossing.end = records[1].end;
    crossing.line = b"{}\n{}".to_vec();
    let union = origin(&snapshot, source, &crossing);
    assert_eq!(union, None, "span union is forbidden");
    let mut open = closed;
    open.end = None;
    let open = origin(&validated(vec![open]), source, &records[0]);
    assert_eq!(open, None, "EOF cannot close an open span");
}

/// Missing, damaged, foreign or unreadable sidecars are never empty provenance: each is refused
/// by mutation without creating or changing the file and without failing the store.
#[test]
fn missing_corrupt_foreign_or_unreadable_sidecar_is_not_present_and_never_created() {
    let mut fixture = Fixture::new("{}\n");
    let (source, end) = (fixture.source.clone(), fixture.records[0].end);
    let closed = span(&source, "old", 0, Some(end));
    let sidecar = fixture.path(CODEX_PROVENANCE_FILE);
    assert_eq!(fixture.channel.codex_provenance(), Sidecar::Missing);
    assert!(rejected(fixture.channel.persist_codex_span(closed.clone())));
    assert!(rejected(fixture.channel.persist_codex_deny(deny(&closed))));
    assert!(!sidecar.exists(), "a refused mutation created the sidecar");
    let valid = |schema: u32, channel: u64| {
        let spans = vec![closed.clone()];
        let read = CodexProvenance {
            schema,
            channel,
            spans,
            denies: Vec::new(),
        };
        serde_json::to_vec(&read).unwrap()
    };
    let mut floor = serde_json::from_slice::<serde_json::Value>(&valid(SCHEMA, 7)).unwrap();
    floor["floors"] = serde_json::json!([{"through": 1}]);
    let mut ownerless = floor.clone();
    ownerless.as_object_mut().unwrap().remove("floors");
    ownerless["spans"][0]["episode"]["request_owner_id"] = 0.into();
    let cases: [(&str, Vec<u8>, &str); 5] = [
        (
            "corrupt",
            b"{\"schema\":1,".to_vec(),
            "codex_provenance_unreadable",
        ),
        (
            "ownerless",
            serde_json::to_vec(&ownerless).unwrap(),
            "codex_provenance_conflict",
        ),
        ("schema", valid(SCHEMA + 1, 7), "codex_provenance_identity"),
        ("channel", valid(SCHEMA, 8), "codex_provenance_identity"),
        (
            "future",
            serde_json::to_vec(&floor).unwrap(),
            "codex_provenance_unreadable",
        ),
    ];
    for (case, bytes, reason) in cases {
        std::fs::write(&sidecar, &bytes).unwrap();
        assert_eq!(
            fixture.channel.codex_provenance(),
            Sidecar::Unavailable(reason),
            "{case}"
        );
        assert!(rejected(fixture.channel.persist_codex_span(closed.clone())));
        assert_eq!(std::fs::read(&sidecar).unwrap(), bytes, "{case} changed");
    }
    std::fs::remove_file(&sidecar).unwrap();
    std::fs::create_dir(&sidecar).unwrap();
    assert_eq!(
        fixture.channel.codex_provenance(),
        Sidecar::Unavailable("codex_provenance_unreadable")
    );
    assert!(rejected(fixture.channel.persist_codex_deny(deny(&closed))));
    assert_eq!(
        fixture.channel.io_failure(),
        None,
        "a refusal is not a halt"
    );
    fixture.channel.set_binding_checkpoint(1).unwrap();
}

/// A boundary written while it carried Codex fields still reads and rewrites, and none of its
/// spans or denies reach the sidecar.
#[test]
fn an_old_boundary_with_codex_fields_reads_rewrites_and_is_never_imported() {
    let mut fixture = Fixture::new("{}\n");
    let closed = span(&fixture.source, "old", 0, Some(fixture.records[0].end));
    let old = serde_json::json!({
        "links": {}, "successors": {},
        "codex_spans": [closed], "codex_denies": [deny(&closed)],
    });
    let boundary = fixture.path(BOUNDARY_FILE);
    std::fs::write(&boundary, serde_json::to_vec(&old).unwrap()).unwrap();
    let before_rewrite = fixture.channel.codex_provenance();
    assert_eq!(
        before_rewrite,
        Sidecar::Missing,
        "old boundary fields were imported"
    );
    let rotation = fixture.channel.rotation().unwrap();
    assert_eq!(rotation, Rotation::default());
    fixture.channel.write_rotation(&rotation).unwrap();
    let rewritten = std::fs::read_to_string(&boundary).unwrap();
    assert_eq!(rewritten, "{\"links\":{},\"successors\":{}}");
    assert_eq!(fixture.channel.codex_provenance(), Sidecar::Missing);
    fixture.channel.init_codex_provenance().unwrap();
    let imported = fixture.present();
    assert!(imported.spans.is_empty() && imported.denies.is_empty());
}

/// P2-1: a deny names a whole episode of this channel and never another execution of a stored
/// span; a refused deny changes no byte and fails nothing, and an invalid one on disk is Unavailable.
#[test]
fn a_foreign_partial_or_conflicting_deny_is_refused_and_unreadable_when_written_directly() {
    let mut fixture = Fixture::new("{}\n");
    let source = fixture.source.clone();
    fixture.channel.init_codex_provenance().unwrap();
    let stored = span(&source, "old", 0, Some(fixture.records[0].end));
    fixture.channel.persist_codex_span(stored.clone()).unwrap();
    let sidecar = fixture.path(CODEX_PROVENANCE_FILE);
    let bytes = std::fs::read(&sidecar).unwrap();
    let mut pending = deny(&span(&source, "new", 0, None));
    pending.episode.native_turn_id.clear();
    let mut cases = vec![deny(&stored); 4];
    cases[0].episode.channel_id = 8;
    cases[1].episode.request_owner_id = 0;
    cases[2].episode.turn_nonce = " ".into();
    cases[3].execution.execution_nonce = "another-execution".into();
    for (case, refused) in cases.into_iter().enumerate() {
        let persisted = fixture.channel.persist_codex_deny(refused.clone());
        assert!(rejected(persisted), "deny {case} accepted");
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            bytes,
            "deny {case} changed bytes"
        );
        assert_eq!(fixture.channel.io_failure(), None);
        let mut written: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        written["denies"] = serde_json::json!([refused]);
        std::fs::write(&sidecar, serde_json::to_vec(&written).unwrap()).unwrap();
        let read = fixture.channel.codex_provenance();
        assert_eq!(
            read,
            Sidecar::Unavailable("codex_provenance_conflict"),
            "deny {case} read"
        );
        std::fs::write(&sidecar, &bytes).unwrap();
    }
    fixture.channel.persist_codex_deny(pending).unwrap();
    fixture.channel.persist_codex_deny(deny(&stored)).unwrap();
    assert_eq!(fixture.present().denies.len(), 2);
}
