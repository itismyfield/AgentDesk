use super::super::NoticeOrigin;
use super::*;
use crate::services::tmux_common as tc;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId, SourceRange, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::LedgerEntry;
use crate::services::tui_o::store::ledger::input_notices::{NoticeEntry, NoticeEvidence};
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig, clear_end};
use crate::services::tui_prompt_dedupe::binding_events as p5;
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    path::{Path, PathBuf},
};

const CHANNEL: u64 = 7332011;
const LOGICAL: &str = "notice-admission-pane";
const NONCE: &str = "73320000000000000000000000000001";

pub(crate) struct BindingRoot(Option<PathBuf>);

impl Drop for BindingRoot {
    fn drop(&mut self) {
        p5::set_test_root(self.0.as_deref());
    }
}

pub(crate) fn bind(
    runtime: &Path,
    channel: u64,
    source: &SourceId,
    logical: &str,
    nonce: &str,
) -> BindingRoot {
    bind_as(
        runtime,
        channel,
        source,
        logical,
        nonce,
        p5::BindingCause::Startup,
    )
}

fn bind_as(
    runtime: &Path,
    channel: u64,
    source: &SourceId,
    logical: &str,
    nonce: &str,
    cause: p5::BindingCause,
) -> BindingRoot {
    let previous = p5::test_root();
    p5::set_test_root(Some(runtime));
    let marker = PathBuf::from(tc::session_temp_path(logical, "spawn_nonce"));
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(marker, nonce).unwrap();
    let proposal = p5::Proposal {
        channel_id: channel,
        provider: "claude",
        tmux_session: logical,
        session_id: Some(&source.session_id),
        path: source.path.to_str().unwrap(),
        replaced: None,
        cause: p5::CauseSource::Hook(cause),
        hook: None,
    };
    assert_eq!(
        p5::record_verified(&proposal, source).unwrap(),
        p5::Committed::Appended
    );
    let events = p5::records_strict(channel).unwrap().unwrap();
    assert_eq!(
        events.last().unwrap().execution_nonce.as_deref(),
        Some(nonce)
    );
    BindingRoot(previous)
}

struct Fixture {
    _guard: crate::config::TestRuntimeRootGuard,
    _bindings: BindingRoot,
    runtime: tempfile::TempDir,
    store: ChannelStore,
    piece: PieceWork,
}

impl Fixture {
    fn new() -> Self {
        let guard = crate::config::TestRuntimeRootGuard::new();
        let runtime = tempfile::tempdir().unwrap();
        let prompt = "terminal input";
        let raw = serde_json::to_vec(&serde_json::json!({"type":"user","uuid":"p",
            "message":{"role":"user","content":prompt}}))
        .unwrap();
        let path = runtime.path().join("transcript.jsonl");
        let mut bytes = raw.clone();
        bytes.push(b'\n');
        std::fs::write(&path, bytes).unwrap();
        let (dev, ino) = crate::services::tui_o::shadow::capture::file_identity(
            &std::fs::metadata(&path).unwrap(),
        );
        let source = SourceId {
            session_id: "native".into(),
            path,
            dev,
            ino,
        };
        let bindings = bind(runtime.path(), CHANNEL, &source, LOGICAL, NONCE);
        let o = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
            .unwrap()
            .unwrap();
        o.begin_era(&[CHANNEL], Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: Vec::new(),
                initial_anchor: 100,
                build_digest: "b".into(),
                at: Utc::now(),
            })
        })
        .unwrap();
        let mut store = o
            .open_channel(&o.read_era().unwrap().unwrap(), CHANNEL)
            .unwrap()
            .unwrap();
        let origin = NoticeOrigin {
            record: SourceRange {
                source,
                start: 0,
                end: raw.len() as u64 + 1,
            },
            logical_session: LOGICAL.into(),
            binding_seq: 1,
            execution_nonce: NONCE.into(),
        };
        let piece = PieceWork {
            output_origin: None,
            unit_key: UnitKey {
                channel_id: CHANNEL,
                provider: ShadowProvider::Claude,
                native_key: "p".into(),
                kind: UnitKind::InputNotice,
            },
            index: 0,
            payload: crate::services::discord::input_notice_content(prompt).unwrap(),
            notice_origin: Some(Box::new(origin.clone())),
        };
        store
            .append_ledger(LedgerEntry::InputNotice {
                entry: Box::new(NoticeEntry::Materialized {
                    evidence: Box::new(NoticeEvidence {
                        record: origin.record.clone(),
                        raw_record: raw,
                        prompt: Some(prompt.into()),
                        prompt_sha256: Some(hex::encode(Sha256::digest(prompt.as_bytes()))),
                        unit_key: Some(piece.unit_key.clone()),
                        payload: Some(piece.payload.clone()),
                        origin: Some(Box::new(origin)),
                    }),
                }),
            })
            .unwrap();
        assert!(store.ledger().violation().is_none());
        Self {
            _guard: guard,
            _bindings: bindings,
            runtime,
            store,
            piece,
        }
    }

    fn clear(&self) {
        let origin = self.piece.notice_origin.as_ref().unwrap();
        assert_eq!(
            clear_end::append(
                self.runtime.path(),
                clear_end::ClearEnd {
                    channel: CHANNEL,
                    provider: "claude".into(),
                    tmux_session: LOGICAL.into(),
                    execution_nonce: NONCE.into(),
                    source: origin.record.source.clone(),
                    binding_seq: 1,
                    host: None,
                    ended_at: Utc::now(),
                    basis: "confirmed_clear".into(),
                }
            )
            .unwrap(),
            clear_end::Appended::Durable
        );
    }
}

#[test]
fn body_bypasses_notice_authority_and_unreadable_binding_history() {
    let mut f = Fixture::new();
    std::fs::write(
        f.runtime
            .path()
            .join("binding_events")
            .join(format!("{CHANNEL}.log")),
        b"broken\n",
    )
    .unwrap();
    f.piece.unit_key.kind = UnitKind::Body;
    f.piece.notice_origin = None;
    let called = Cell::new(0);
    let result = tc::with_tmux_source_authority(LOGICAL, |_| {
        admit_notice(&f.piece, &mut f.store, |_| called.set(called.get() + 1))
    });
    assert!(result.is_ok());
    assert_eq!(called.get(), 1);
}

#[test]
fn clear_first_excludes_before_start_and_survives_reopening() {
    let mut f = Fixture::new();
    f.clear();
    let called = Cell::new(0);
    let result = admit_notice(&f.piece, &mut f.store, |_| called.set(called.get() + 1));
    assert!(matches!(result, Err(NoticeRefusal::Withheld(_))));
    assert_eq!(called.get(), 0);
    assert_eq!(
        f.store.ledger().excluded(&f.piece.unit_key),
        Some("input_notice_clear")
    );
    assert_eq!(f.store.ledger().next_serial(), 0);
    let o = OStore::existing(f.runtime.path()).unwrap();
    let reopened = o
        .open_channel(&o.read_era().unwrap().unwrap(), CHANNEL)
        .unwrap()
        .unwrap();
    assert_eq!(
        reopened.ledger().excluded(&f.piece.unit_key),
        Some("input_notice_clear")
    );
}

#[test]
fn notice_start_holds_source_authority_and_later_clear_prevents_a_second_start() {
    let mut f = Fixture::new();
    std::fs::write(
        tc::session_temp_path(LOGICAL, "spawn_nonce"),
        "new-incarnation",
    )
    .unwrap();
    let called = Cell::new(0);
    let nested = Cell::new(false);
    let result = admit_notice(&f.piece, &mut f.store, |_| {
        called.set(called.get() + 1);
        nested.set(tc::try_with_tmux_source_authority(LOGICAL, |_| ()).is_some());
    });
    assert!(
        result.is_ok(),
        "historical binding remains evidence across a normal incarnation change"
    );
    assert_eq!(called.get(), 1);
    assert!(
        !nested.get(),
        "authority must cover prepare and the first poll"
    );
    f.clear();
    let result = admit_notice(&f.piece, &mut f.store, |_| called.set(called.get() + 1));
    assert!(matches!(result, Err(NoticeRefusal::Withheld(_))));
    assert_eq!(called.get(), 1);
}

#[test]
fn missing_provenance_and_unreadable_or_foreign_binding_never_start() {
    let mut f = Fixture::new();
    let called = Cell::new(0);
    let mut missing = f.piece.clone();
    missing.notice_origin = None;
    assert!(matches!(
        admit_notice(&missing, &mut f.store, |_| called.set(called.get() + 1)),
        Err(NoticeRefusal::Withheld(_))
    ));
    let mut foreign = f.piece.clone();
    foreign.notice_origin.as_mut().unwrap().binding_seq = 999;
    assert!(matches!(
        admit_notice(&foreign, &mut f.store, |_| called.set(called.get() + 1)),
        Err(NoticeRefusal::Withheld(_))
    ));
    std::fs::write(
        f.runtime
            .path()
            .join("binding_events")
            .join(format!("{CHANNEL}.log")),
        b"broken\n",
    )
    .unwrap();
    assert!(matches!(
        admit_notice(&f.piece, &mut f.store, |_| called.set(called.get() + 1)),
        Err(NoticeRefusal::Withheld(_))
    ));
    assert_eq!(called.get(), 0);
    assert_eq!(f.store.ledger().next_serial(), 0);
}

#[test]
fn source_contention_yields_before_start_without_a_blocking_lock() {
    let mut f = Fixture::new();
    let called = Cell::new(0);
    let result = tc::with_tmux_source_authority(LOGICAL, |_| {
        admit_notice(&f.piece, &mut f.store, |_| called.set(called.get() + 1))
    });
    assert_eq!(result, Err(NoticeRefusal::Busy));
    assert_eq!(called.get(), 0);
}

#[test]
fn descendant_clear_excludes_original_input_across_normal_resume() {
    for native in [false, true] {
        let mut f = Fixture::new();
        let mut descendant = f
            .piece
            .notice_origin
            .as_ref()
            .unwrap()
            .record
            .source
            .clone();
        descendant.session_id = "resumed".into();
        descendant.path = f.runtime.path().join("resumed.jsonl");
        std::fs::write(&descendant.path, b"{}\n").unwrap();
        (descendant.dev, descendant.ino) = crate::services::tui_o::shadow::capture::file_identity(
            &std::fs::metadata(&descendant.path).unwrap(),
        );
        let nonce = "73320000000000000000000000000002";
        let _resume = bind_as(
            f.runtime.path(),
            CHANNEL,
            &descendant,
            LOGICAL,
            nonce,
            p5::BindingCause::Resume,
        );
        let called = Cell::new(0);
        assert!(admit_notice(&f.piece, &mut f.store, |_| called.set(called.get() + 1)).is_ok());
        assert_eq!(
            called.get(),
            1,
            "normal resume must preserve the original input"
        );
        assert!(f.store.ledger().excluded(&f.piece.unit_key).is_none());
        if native {
            let mut next = descendant.clone();
            next.session_id = "cleared".into();
            next.path = f.runtime.path().join("cleared.jsonl");
            std::fs::write(&next.path, b"{}\n").unwrap();
            (next.dev, next.ino) = crate::services::tui_o::shadow::capture::file_identity(
                &std::fs::metadata(&next.path).unwrap(),
            );
            let _clear = bind_as(
                f.runtime.path(),
                CHANNEL,
                &next,
                LOGICAL,
                nonce,
                p5::BindingCause::Clear,
            );
        } else {
            assert_eq!(
                clear_end::append(
                    f.runtime.path(),
                    clear_end::ClearEnd {
                        channel: CHANNEL,
                        provider: "claude".into(),
                        tmux_session: LOGICAL.into(),
                        execution_nonce: nonce.into(),
                        source: descendant,
                        binding_seq: 2,
                        host: None,
                        ended_at: Utc::now(),
                        basis: "confirmed_clear".into(),
                    }
                )
                .unwrap(),
                clear_end::Appended::Durable
            );
        }
        assert!(matches!(
            admit_notice(&f.piece, &mut f.store, |_| called.set(called.get() + 1)),
            Err(NoticeRefusal::Withheld(_))
        ));
        assert_eq!(called.get(), 1);
        assert_eq!(
            f.store.ledger().excluded(&f.piece.unit_key),
            Some("input_notice_clear")
        );
    }
}

#[test]
fn native_clear_pending_excludes_before_new_transcript_resolves() {
    for cause in [p5::BindingCause::Clear, p5::BindingCause::Resume] {
        let mut f = Fixture::new();
        let path = f.runtime.path().join("not-yet-written.jsonl");
        let proposal = p5::Proposal {
            channel_id: CHANNEL,
            provider: "claude",
            tmux_session: LOGICAL,
            session_id: Some("pending-native"),
            path: path.to_str().unwrap(),
            replaced: None,
            cause: p5::CauseSource::Hook(cause),
            hook: None,
        };
        assert_eq!(
            p5::record_pending(&proposal).unwrap(),
            p5::PendingRecord::Recorded
        );
        let events = p5::records_strict(CHANNEL).unwrap().unwrap();
        let pending = events.last().unwrap();
        assert!(matches!(pending.new, p5::BindingTarget::Pending { .. }));
        assert_eq!(pending.cause, cause);
        assert_eq!(
            pending.old.as_ref(),
            Some(&f.piece.notice_origin.as_ref().unwrap().record.source)
        );
        assert!(!path.exists());
        let called = Cell::new(0);
        assert!(matches!(
            admit_notice(&f.piece, &mut f.store, |_| called.set(called.get() + 1)),
            Err(NoticeRefusal::Withheld(_))
        ));
        assert_eq!(called.get(), 0);
        assert_eq!(f.store.ledger().next_serial(), 0);
        assert_eq!(
            f.store.ledger().excluded(&f.piece.unit_key),
            (cause == p5::BindingCause::Clear).then_some("input_notice_clear")
        );
    }
}
