#![cfg(unix)]

use super::*;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::{file_identity, renumber};
use crate::services::tui_o::store::spool::{Cursor, source_key};
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig, fault};
use crate::services::tui_o::writer::owed::tests::{Bindings, Collector, Writer, kept, owed_writer};
use crate::services::tui_o::writer::owed::{OwedLog, reevaluate};
use crate::services::tui_o::writer::pieces::Derived;
use sha2::{Digest, Sha256};
use std::io::Write;

struct Fixture {
    root: tempfile::TempDir,
    context: BindingContext,
    proof: ExecutionProofRef,
    episode: TurnEpisodeRef,
    writer: Writer,
    header_end: u64,
    checkpoint: SourceCheckpoint,
    /// The header-only cursor the store first keeps for the proven file.
    cursor: Cursor,
    floor: NativeSubmitFloor,
}

impl Fixture {
    fn new() -> Self {
        let mut fixture = Self::bare();
        fixture.store().init_codex_provenance().unwrap();
        fixture
    }

    /// No sidecar: boot completion must neither use nor create one.
    fn bare() -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source.jsonl");
        let header = serde_json::json!({"type":"session_meta","payload":{"id":"native","cwd":root.path(),"source":"cli","originator":"codex-tui"}}).to_string() + "\n";
        std::fs::write(&path, &header).unwrap();
        let (dev, ino) = file_identity(&std::fs::metadata(&path).unwrap());
        let source = SourceId {
            session_id: "native".into(),
            path,
            dev,
            ino,
        };
        let proof = ExecutionProofRef {
            owner_runtime_root: root.path().display().to_string(),
            tmux_session: "test-pane".into(),
            execution_nonce: "a".repeat(32),
            proof_seq: 3,
            source,
        };
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: proof.execution_nonce.clone(),
            tmux_session: proof.tmux_session.clone(),
            channel_id: Some(7),
            owner_runtime_root: proof.owner_runtime_root.clone(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(root.path().into()),
            first_prompt_digest: None,
            source_policy: Some("verified".into()),
        };
        let episode = TurnEpisodeRef {
            channel_id: 7,
            user_message_id: 9,
            request_owner_id: 11,
            turn_nonce: "original-turn".into(),
            native_turn_id: String::new(),
        };
        let o = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
            .unwrap()
            .unwrap();
        let era = o
            .begin_era(&[7], chrono::Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: vec![],
                    initial_anchor: 1,
                    build_digest: "test".into(),
                    at: chrono::Utc::now(),
                })
            })
            .unwrap();
        let store = o.open_channel(&era, 7).unwrap().unwrap();
        let header_end = header.len() as u64;
        let checkpoint = SourceCheckpoint {
            through: header_end,
            prefix_hash: hex::encode(Sha256::digest(header.as_bytes())),
        };
        let cursor = Cursor {
            source: proof.source.clone(),
            captured_through: header_end,
            prefix_hash: checkpoint.prefix_hash.clone(),
            retired: false,
        };
        let floor = NativeSubmitFloor::capture_before_submit(&proof, &cursor).unwrap();
        let (writer, _) = owed_writer(store);
        Self {
            root,
            context,
            proof,
            episode,
            writer,
            header_end,
            checkpoint,
            cursor,
            floor,
        }
    }

    fn store(&mut self) -> &mut ChannelStore {
        self.writer.store()
    }

    fn spans(&mut self) -> Vec<CodexEpisodeSpan> {
        match self.store().codex_provenance() {
            Sidecar::Present(read) => read.spans,
            other => panic!("sidecar not present: {other:?}"),
        }
    }

    /// Owed work of a fresh resume over the retained spool, each origin read from the sidecar.
    fn collect(&mut self) -> OwedLog {
        let resumed = Collector::resumed(&mut self.writer, &Bindings::new());
        let mut log = kept(resumed, "resume").log;
        reevaluate(self.store(), &mut log.work).unwrap();
        log
    }

    fn len(&self) -> u64 {
        std::fs::metadata(&self.proof.source.path).unwrap().len()
    }

    fn append(&self, value: serde_json::Value) -> (u64, u64) {
        let start = self.len();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.proof.source.path)
            .unwrap();
        writeln!(file, "{value}").unwrap();
        (start, self.len())
    }

    fn event(&self, kind: &str, id: &str) -> (u64, u64) {
        self.append(serde_json::json!({"type":"event_msg","payload":{"type":kind,"turn_id":id}}))
    }

    fn body(&self) {
        self.reply("recovered-body", "stored reply");
    }

    fn reply(&self, id: &str, text: &str) {
        self.append(serde_json::json!({"type":"response_item", "payload":{
            "type":"message", "role":"assistant", "id":id,
            "content":[{"type":"output_text", "text":text}]
        }}));
    }

    /// Spools the whole file under the cursor's first-stored identity.
    fn spool(&mut self) {
        let source = self.cursor.source.clone();
        self.store().attach_source(&source).unwrap();
        let empty = hex::encode(Sha256::digest(b""));
        let mut capture = SourceCapture::reopen(source, 0, &empty).unwrap();
        let CaptureOutcome::Batch(batch) = capture.poll(1 << 20) else {
            panic!("capture failed");
        };
        let prefix_hash = capture.prefix_hash();
        self.store().append_spool(&batch, &prefix_hash).unwrap();
    }

    fn row(&self) -> BootOwedRecord {
        BootOwedRecord::from_scanned_row(
            self.proof.clone(),
            self.episode.clone(),
            &self.floor,
            self.len(),
            42,
        )
        .unwrap()
    }

    fn complete(
        &mut self,
        row: Option<&BootOwedRecord>,
    ) -> Result<Option<CodexEpisodeSpan>, &'static str> {
        complete_boot_span(self.writer.store(), row, &self.context, &self.checkpoint)
    }
}

#[test]
fn confirmed_0162_metadata_persists_span_and_child_review_has_no_span() {
    let mut fixture = Fixture::new();
    for line in
        include_str!("../../../../../../tests/fixtures/codex_provenance/0162_metadata_turn.jsonl")
            .lines()
    {
        fixture.append(serde_json::from_str(line).unwrap());
    }
    let row = fixture.row();
    let span = fixture.complete(Some(&row)).unwrap().unwrap();
    assert_eq!(span.episode.native_turn_id, "old");
    assert_eq!(
        (span.start, span.end),
        (fixture.header_end, Some(fixture.len()))
    );
    assert_eq!(fixture.spans(), std::slice::from_ref(&span));
    let mut fixture = Fixture::new();
    for line in include_str!("../../../../../../tests/fixtures/codex_busy_inject/review_turn.jsonl")
        .lines()
        .take(10)
    {
        fixture.append(serde_json::from_str(line).unwrap());
    }
    let row = fixture.row();
    assert_eq!(fixture.complete(Some(&row)), Err("child_native_turn"));
    assert!(fixture.spans().is_empty());
}

#[test]
fn actual_submit_success_is_required_for_exact_token_span() {
    let fixture = Fixture::new();
    assert!(
        EpisodeEvidenceSnapshot::capture(
            Arc::new(CancelToken::from_persisted_turn_nonce(None)),
            fixture.episode.clone(),
        )
        .is_err()
    );
    let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        fixture.episode.turn_nonce.clone(),
    )));
    let evidence =
        EpisodeEvidenceSnapshot::capture(token.clone(), fixture.episode.clone()).unwrap();
    assert!(actual_submit(&evidence, &token, &fixture.context, || Err("input_refused")).is_err());
    let receipt = actual_submit(&evidence, &token, &fixture.context, || Ok(())).unwrap();
    let submitted = submitted_episode(&evidence, &token, &fixture.context, &receipt).unwrap();
    let span = bind_episode_span(
        &submitted,
        &fixture.proof,
        NativeTurnAnchor {
            native_turn_id: "n1".into(),
            start: fixture.header_end,
        },
        42,
    )
    .unwrap();
    assert_eq!(span.episode.turn_nonce, "original-turn");
    assert_eq!(span.end, None);
    let successor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        fixture.episode.turn_nonce.clone(),
    )));
    assert!(submitted_episode(&evidence, &successor, &fixture.context, &receipt).is_err());
    let mut changed = fixture.context.clone();
    changed.execution_nonce = "b".repeat(32);
    assert!(submitted_episode(&evidence, &token, &changed, &receipt).is_err());
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(submitted_episode(&evidence, &token, &fixture.context, &receipt).is_err());
}

#[test]
fn t0a_boot_row_before_submit_has_no_span_or_origin() {
    let mut fixture = Fixture::new();
    let row = fixture.row();
    assert_eq!(fixture.complete(Some(&row)), Ok(None));
    assert!(fixture.spans().is_empty());
}

#[test]
fn boot_completion_neither_uses_nor_creates_a_missing_sidecar() {
    let mut fixture = Fixture::bare();
    fixture.event("task_started", "n1");
    fixture.event("task_complete", "n1");
    let row = fixture.row();
    assert_eq!(fixture.complete(Some(&row)), Err("span_store_unavailable"));
    assert_eq!(fixture.store().codex_provenance(), Sidecar::Missing);
}

#[test]
fn t0bc_boot_completes_missing_span_from_unique_opener_and_named_terminal() {
    for observed_before_crash in [false, true] {
        let mut fixture = Fixture::new();
        let (start, _) = fixture.event("task_started", "n1");
        if observed_before_crash {
            assert!(
                scan_anchor(&fixture.proof.source, fixture.header_end, fixture.len())
                    .unwrap()
                    .is_some()
            );
        }
        fixture.body();
        let (_, end) = fixture.event("task_complete", "n1");
        let row = fixture.row();
        fixture.spool();
        let span = fixture.complete(Some(&row)).unwrap().unwrap();
        assert_eq!(
            (span.start, span.end),
            (start, Some(end)),
            "T0 b/c must complete S"
        );
        assert_eq!(span.execution, fixture.proof);
        assert_eq!(fixture.spans(), std::slice::from_ref(&span));
        let work = fixture.collect().work;
        let [work] = work.as_slice() else {
            panic!("T0 b/c requires one original body");
        };
        let Derived::Piece(piece) = &work.derived else {
            panic!("body missing");
        };
        assert_eq!(piece.payload, "stored reply");
        assert_eq!(work.origin.as_ref().unwrap().span, span);
    }
}

#[test]
fn t0d_two_openers_and_row_native_conflict_are_unknown() {
    let mut fixture = Fixture::new();
    fixture.event("task_started", "n1");
    fixture.event("task_complete", "n1");
    fixture.event("task_started", "n2");
    let row = fixture.row();
    assert_eq!(fixture.complete(Some(&row)), Err("multiple_native_openers"));
    assert!(fixture.spans().is_empty());
    let mut fixture = Fixture::new();
    fixture.event("task_started", "n1");
    let mut row = fixture.row();
    row.episode.native_turn_id = "other".into();
    assert_eq!(
        fixture.complete(Some(&row)),
        Err("boot_native_turn_mismatch")
    );
    assert!(fixture.spans().is_empty());
}

#[test]
fn boot_eof_is_pinned_and_missing_record_never_completes_span() {
    let mut fixture = Fixture::new();
    fixture.event("task_started", "n1");
    fixture.event("task_complete", "n1");
    let row = fixture.row();
    fixture.event("task_started", "n2");
    assert_eq!(fixture.complete(None), Err("boot_record_unavailable"));
    assert!(fixture.spans().is_empty());
    let span = fixture.complete(Some(&row));
    let pinned = span
        .as_ref()
        .map(|s| s.as_ref().map(|s| s.episode.native_turn_id.as_str()));
    assert_eq!(
        pinned,
        Ok(Some("n1")),
        "a later opener past the pinned EOF was scanned"
    );
    assert!(span.unwrap().unwrap().end.unwrap() < fixture.len());
}

#[test]
fn t4_boot_closes_saved_open_span_at_own_terminal_and_t5_keeps_tail_unknown() {
    let mut fixture = Fixture::new();
    let (start, _) = fixture.event("task_started", "n1");
    let row = fixture.row();
    let open = fixture.complete(Some(&row)).unwrap().unwrap();
    assert_eq!(open.end, None, "T5 EOF cannot confirm tail");
    let (_, end) = fixture.event("task_complete", "n1");
    let closed = fixture.complete(Some(&row)).unwrap().unwrap();
    assert_eq!(
        (closed.start, closed.end),
        (start, Some(end)),
        "T4 matching terminal closes saved S"
    );
    assert_eq!(fixture.complete(Some(&row)).unwrap(), Some(closed));
}

/// The proof's dev differs from the cursor's first-stored dev and the file is renumbered again:
/// only the stored span closes, under its original execution, and the cursor key is unchanged.
#[test]
fn renumbered_proof_closes_only_its_own_span_while_the_cursor_keeps_its_first_identity() {
    let mut fixture = Fixture::new();
    let key = source_key(&fixture.cursor.source);
    let path = fixture.proof.source.path.clone();
    let _boot = renumber::shift(&path, 0x4000);
    fixture.proof.source.dev = file_identity(&std::fs::metadata(&path).unwrap()).0;
    fixture.floor =
        NativeSubmitFloor::capture_before_submit(&fixture.proof, &fixture.cursor).unwrap();
    let mut other = fixture.proof.clone();
    other.execution_nonce = "b".repeat(32);
    let other_floor = NativeSubmitFloor::capture_before_submit(&other, &fixture.cursor).unwrap();
    fixture.event("task_started", "n1");
    fixture.body();
    fixture.spool();
    let row = fixture.row();
    let open = fixture.complete(Some(&row)).unwrap().unwrap();
    let _reboot = renumber::shift(&path, 0x8000);
    let (_, end) = fixture.event("task_complete", "n1");
    let closed = fixture.complete(Some(&row));
    let shape = closed
        .as_ref()
        .map(|s| s.as_ref().map(|s| (&s.execution, &s.episode, s.end)));
    assert_eq!(shape, Ok(Some((&fixture.proof, &open.episode, Some(end)))));
    assert_ne!(fixture.proof.source.dev, fixture.cursor.source.dev);
    let closed = closed.unwrap().unwrap();
    assert_eq!(fixture.spans(), std::slice::from_ref(&closed));
    let len = fixture.len();
    let foreign =
        BootOwedRecord::from_scanned_row(other, fixture.episode.clone(), &other_floor, len, 42);
    fixture.context.execution_nonce = "b".repeat(32);
    let joined = fixture.complete(Some(&foreign.unwrap()));
    assert_eq!(
        joined,
        Err("span_store_unavailable"),
        "another execution joined the span"
    );
    assert_eq!(fixture.spans(), std::slice::from_ref(&closed));
    let work = fixture.collect().work;
    let origins: Vec<_> = work
        .iter()
        .map(|w| (source_key(&w.source), w.origin.clone()))
        .collect();
    assert_eq!(origins.len(), 1);
    assert_eq!(origins[0].0, key, "the spool key follows the stored cursor");
    assert_eq!(origins[0].1.as_ref().map(|o| &o.span), Some(&closed));
    let mut second = fixture.cursor.source.clone();
    second.dev = file_identity(&std::fs::metadata(&path).unwrap()).0;
    fixture.store().attach_source(&second).unwrap();
    let resumed = Collector::resumed(&mut fixture.writer, &Bindings::new());
    assert!(resumed.is_err(), "two stored cursors of one file resumed");
}

/// Work collected while its span is open has no origin; once the own terminal closes the span
/// durably, re-evaluating the same work finds it, and no work is added or changed.
#[test]
fn late_origin_reaches_the_same_spooled_work_only_after_the_durable_close() {
    let mut fixture = Fixture::new();
    fixture.event("task_started", "n1");
    fixture.body();
    fixture.spool();
    let row = fixture.row();
    let open = fixture.complete(Some(&row));
    assert_eq!(open.map(|s| s.map(|s| s.end)), Ok(Some(None)));
    let mut log = fixture.collect();
    let [before] = log
        .work
        .clone()
        .try_into()
        .unwrap_or_else(|w| panic!("one body: {w:?}"));
    assert_eq!(before.origin, None, "an open span grants no origin");
    let (_, end) = fixture.event("task_complete", "n1");
    let closed = fixture.complete(Some(&row));
    assert_eq!(
        closed.as_ref().map(|s| s.as_ref().map(|s| s.end)),
        Ok(Some(Some(end)))
    );
    reevaluate(fixture.store(), &mut log.work).unwrap();
    let [after] = log.work.as_slice() else {
        panic!("re-evaluation changed the work: {:?}", log.work)
    };
    let Derived::Piece(piece) = &after.derived else {
        panic!("body missing")
    };
    assert_eq!((piece.index, piece.payload.as_str()), (0, "stored reply"));
    assert_eq!(
        (&after.derived, &after.source, &after.range),
        (&before.derived, &before.source, &before.range)
    );
    assert_eq!(
        after.origin.as_ref().map(|o| &o.span),
        closed.unwrap().as_ref()
    );
    assert_eq!(fixture.spans().len(), 1);
}

/// P2-R1: the submit floor lies past the cursor with a whole earlier turn between them; only the
/// opener after the floor is attributed, and a changed byte before the floor is refused.
#[test]
fn r1_a_floor_past_the_cursor_excludes_the_earlier_turn_and_is_itself_verified() {
    let mut fixture = Fixture::new();
    fixture.event("task_started", "n0");
    fixture.reply("old-body", "old reply");
    fixture.event("task_complete", "n0");
    fixture.floor =
        NativeSubmitFloor::capture_before_submit(&fixture.proof, &fixture.cursor).unwrap();
    let (start, _) = fixture.event("task_started", "n1");
    fixture.reply("new-body", "new reply");
    let (_, end) = fixture.event("task_complete", "n1");
    assert!(fixture.cursor.captured_through < start);
    let row = fixture.row();
    let span = fixture.complete(Some(&row));
    let shape = span.as_ref().map(|s| {
        s.as_ref()
            .map(|s| (s.episode.native_turn_id.as_str(), s.start, s.end))
    });
    assert_eq!(shape, Ok(Some(("n1", start, Some(end)))));
    let span = span.unwrap().unwrap();
    assert_eq!(fixture.spans(), std::slice::from_ref(&span));
    fixture.spool();
    let work = fixture.collect().work;
    let origins: Vec<_> = work
        .iter()
        .map(|w| w.origin.as_ref().map(|o| &o.span))
        .collect();
    assert_eq!(
        origins,
        [None, Some(&span)],
        "only the turn after the floor is attributed"
    );
    let mut bytes = std::fs::read(&fixture.proof.source.path).unwrap();
    let at = bytes.windows(9).position(|w| w == b"old reply").unwrap();
    bytes[at] = b'O';
    std::fs::write(&fixture.proof.source.path, bytes).unwrap();
    assert_eq!(
        fixture.complete(Some(&row)),
        Err("source_checkpoint_changed")
    );
}

/// P2-R2/B/C: a well-formed turn reaches a span first; each malformed or foreign record leaves
/// the sidecar without a span and the spooled work without an origin, for its own reason.
#[test]
fn r2_metadata_opener_and_terminal_faults_leave_no_span_or_origin_for_their_reason() {
    use serde_json::json;
    let started =
        |id: &str| json!({"type":"event_msg","payload":{"type":"task_started","turn_id":id}});
    let done =
        |id: &str| json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":id}});
    let token = |payload| json!({"type":"token_usage_record","payload":payload});
    let body = json!({"type":"response_item","payload":{"type":"message","role":"assistant",
        "id":"recovered-body","content":[{"type":"output_text","text":"stored reply"}]}});
    let trigger =
        json!({"type":"inter_agent_communication_metadata","payload":{"trigger_turn":true}});
    let synthetic = json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"n1","synthetic":true}});
    let anonymous = json!({"type":"event_msg","payload":{"type":"task_started"}});
    let unknown = json!({"type":"event_msg","payload":{"type":"mystery","turn_id":"n1"}});
    let cases: Vec<(&str, Vec<serde_json::Value>, Option<&str>)> = vec![
        (
            "well-formed",
            vec![
                started("n1"),
                token(json!({"turn_id":"n1"})),
                body.clone(),
                done("n1"),
            ],
            None,
        ),
        (
            "malformed token",
            vec![
                started("n1"),
                token(json!({"response_id":"r"})),
                body.clone(),
                done("n1"),
            ],
            Some("unknown_native_schema"),
        ),
        (
            "foreign token",
            vec![
                started("n1"),
                token(json!({"turn_id":"n2"})),
                body.clone(),
                done("n1"),
            ],
            Some("foreign_native_record"),
        ),
        (
            "agent trigger",
            vec![started("n1"), trigger, body.clone(), done("n1")],
            Some("agent_triggered_native_turn"),
        ),
        (
            "anonymous opener",
            vec![anonymous, body.clone()],
            Some("anonymous_native_opener"),
        ),
        (
            "two openers",
            vec![started("n1"), body.clone(), done("n1"), started("n2")],
            Some("multiple_native_openers"),
        ),
        (
            "synthetic terminal",
            vec![started("n1"), body.clone(), synthetic],
            Some("unknown_native_schema"),
        ),
        (
            "foreign terminal",
            vec![started("n1"), body.clone(), done("n2")],
            Some("unnamed_or_foreign_terminal"),
        ),
        (
            "unknown record",
            vec![started("n1"), unknown, body.clone()],
            Some("unknown_native_schema"),
        ),
        ("open at EOF", vec![started("n1"), body.clone()], None),
        (
            "truncated",
            vec![started("n1"), body],
            Some("incomplete_native_record"),
        ),
    ];
    for (case, lines, refused) in cases {
        let mut fixture = Fixture::new();
        for line in lines {
            fixture.append(line);
        }
        if case == "truncated" {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&fixture.proof.source.path)
                .unwrap();
            file.write_all(b"{\"type\":\"event_msg\"").unwrap();
        }
        let row = fixture.row();
        let result = fixture.complete(Some(&row));
        let closed = match (refused, &result) {
            (Some(reason), _) => {
                assert_eq!(result, Err(reason), "{case}");
                assert!(fixture.spans().is_empty(), "{case} stored a span");
                false
            }
            (None, Ok(Some(span))) => span.end.is_some(),
            (None, other) => panic!("{case} reached no span: {other:?}"),
        };
        assert_eq!(
            closed,
            case == "well-formed",
            "{case} closed at the wrong place"
        );
        fixture.spool();
        let origins: Vec<_> = fixture
            .collect()
            .work
            .iter()
            .map(|w| w.origin.is_some())
            .collect();
        assert_eq!(origins, [closed], "{case} origin");
    }
}

#[test]
fn span_storage_failure_exposes_no_origin_and_prefix_rewrite_is_unknown() {
    let mut fixture = Fixture::new();
    fixture.event("task_started", "n1");
    fixture.event("task_complete", "n1");
    let row = fixture.row();
    let failure = fault::plant(
        fixture.root.path(),
        fault::Step::Write,
        std::io::ErrorKind::StorageFull,
        None,
    );
    assert_eq!(
        fixture.complete(Some(&row)),
        Err("span_store_unavailable"),
        "durable failure must not publish an origin"
    );
    assert!(fixture.spans().is_empty());
    drop(failure);
    let mut bytes = std::fs::read(&fixture.proof.source.path).unwrap();
    let at = bytes.windows(6).position(|s| s == b"native").unwrap();
    bytes[at] = b'x';
    std::fs::write(&fixture.proof.source.path, bytes).unwrap();
    assert_eq!(
        fixture.complete(Some(&row)),
        Err("source_checkpoint_changed")
    );
}

#[test]
fn prefix_rewrite_after_terminal_scan_exposes_no_span_or_origin() {
    let mut fixture = Fixture::new();
    fixture.event("task_started", "n1");
    fixture.body();
    fixture.event("task_complete", "n1");
    let row = fixture.row();
    let path = fixture.proof.source.path.clone();
    let before = std::fs::metadata(&path).unwrap();
    let result = complete_boot_span_with_hook(
        fixture.writer.store(),
        Some(&row),
        &fixture.context,
        &fixture.checkpoint,
        || {
            let mut bytes = std::fs::read(&path).unwrap();
            let needle = b"\"turn_id\":\"n1\"";
            let at = bytes
                .windows(needle.len())
                .rposition(|s| s == needle)
                .unwrap();
            bytes[at + b"\"turn_id\":\"n".len()] = b'2';
            std::fs::write(&path, bytes).unwrap();
            let after = std::fs::metadata(&path).unwrap();
            assert_eq!(file_identity(&after), file_identity(&before));
            assert_eq!(after.len(), before.len());
        },
    );
    assert_eq!(result, Err("source_checkpoint_changed"));
    assert!(fixture.spans().is_empty());
}
