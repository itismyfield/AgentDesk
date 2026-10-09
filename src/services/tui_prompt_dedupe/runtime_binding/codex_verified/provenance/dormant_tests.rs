#![cfg(unix)]

use super::*;
use crate::services::tui_o::shadow::capture::file_identity;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig, fault};
use sha2::{Digest, Sha256};
use std::io::Write;

struct Fixture {
    root: tempfile::TempDir,
    context: BindingContext,
    proof: ExecutionProofRef,
    episode: TurnEpisodeRef,
    store: ChannelStore,
    header_end: u64,
    checkpoint: SourceCheckpoint,
}

impl Fixture {
    fn new() -> Self {
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
        Self {
            root,
            context,
            proof,
            episode,
            store,
            header_end,
            checkpoint,
        }
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
        self.append(serde_json::json!({"type":"response_item", "payload":{
            "type":"message", "role":"assistant", "id":"recovered-body",
            "content":[{"type":"output_text", "text":"stored reply"}]
        }}));
    }

    fn spool(&mut self) {
        self.store.attach_source(&self.proof.source).unwrap();
        let mut capture = SourceCapture::open(self.proof.source.clone(), 0).unwrap();
        let CaptureOutcome::Batch(batch) = capture.poll(1 << 20) else {
            panic!("capture failed");
        };
        self.store
            .append_spool(&batch, &capture.prefix_hash())
            .unwrap();
    }

    fn row(&self) -> BootOwedRecord {
        BootOwedRecord::from_scanned_row(
            self.proof.clone(),
            self.episode.clone(),
            self.header_end,
            self.len(),
            42,
        )
        .unwrap()
    }

    fn complete(
        &mut self,
        row: Option<&BootOwedRecord>,
    ) -> Result<Option<CodexEpisodeSpan>, &'static str> {
        complete_boot_span(&mut self.store, row, &self.context, &self.checkpoint)
    }
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
    assert!(fixture.store.rotation().unwrap().codex_spans.is_empty());
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
        assert_eq!(
            fixture.store.rotation().unwrap().codex_spans,
            [span.clone()]
        );
        use crate::services::tui_o::writer::{
            pieces::{Derived, UnitDeriver},
            rotation::provenance::replay,
        };
        let work = replay(
            &mut fixture.store,
            &fixture.proof.source,
            &mut UnitDeriver::new(7, ShadowProvider::Codex),
        )
        .unwrap();
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
    assert!(fixture.store.rotation().unwrap().codex_spans.is_empty());
    let mut fixture = Fixture::new();
    fixture.event("task_started", "n1");
    let mut row = fixture.row();
    row.episode.native_turn_id = "other".into();
    assert_eq!(
        fixture.complete(Some(&row)),
        Err("boot_native_turn_mismatch")
    );
    assert!(fixture.store.rotation().unwrap().codex_spans.is_empty());
}

#[test]
fn boot_eof_is_pinned_and_missing_record_never_completes_span() {
    let mut fixture = Fixture::new();
    fixture.event("task_started", "n1");
    fixture.event("task_complete", "n1");
    let row = fixture.row();
    fixture.event("task_started", "n2");
    assert_eq!(fixture.complete(None), Err("boot_record_unavailable"));
    assert!(fixture.store.rotation().unwrap().codex_spans.is_empty());
    let span = fixture.complete(Some(&row)).unwrap().unwrap();
    assert!(span.end.unwrap() < fixture.len());
    assert_eq!(span.episode.native_turn_id, "n1");
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
    assert!(fixture.store.rotation().unwrap().codex_spans.is_empty());
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
        &mut fixture.store,
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
    assert!(fixture.store.rotation().unwrap().codex_spans.is_empty());
}
