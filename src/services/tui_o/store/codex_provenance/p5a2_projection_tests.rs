use super::projection::ReadBackProjection;
use super::submit_floor::{NativeFloor, SubmitFloor, tests as fixture};
use super::*;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource};
use crate::services::tui_o::store::tests::{enabled, initialized};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::decisions::{
    CheckpointReason, PersistenceReason as P,
};

struct Fixture {
    root: tempfile::TempDir,
    store: ChannelStore,
    floor: NativeFloor,
}
impl Fixture {
    fn new(ahead: bool) -> Self {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let source = fixture::native_file(root.path());
        let owner = enabled(root.path());
        let era = owner
            .begin_era(&[7], chrono::Utc::now(), |c| Ok(initialized(c, Vec::new())))
            .unwrap();
        let mut store = owner.open_channel(&era, 7).unwrap().unwrap();
        store.attach_source(&source).unwrap();
        let mut capture = SourceCapture::open(source.clone(), 0).unwrap();
        let CaptureOutcome::Batch(batch) = capture.poll(1 << 20) else {
            panic!("native capture")
        };
        store.append_spool(&batch, &capture.prefix_hash()).unwrap();
        if ahead {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&source.path)
                .unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":"world_state","payload":{"full":true}})
            )
            .unwrap();
        }
        let floor =
            fixture::native_floor(fixture::launch(7, &source), store.cursor(&source).unwrap());
        Self { root, store, floor }
    }
    fn path(&self) -> std::path::PathBuf {
        self.root.path().join("o_store/7/codex_provenance")
    }
    fn prepare(&mut self) {
        self.store
            .prepare_codex_floor(SubmitFloor::NativeFloor(Box::new(self.floor.clone())))
            .unwrap();
    }
    fn read(&self) -> ReadBackProjection {
        self.store.codex_read_back(self.floor.execution()).unwrap()
    }
    fn deny(&self) -> CodexEpisodeDeny {
        CodexEpisodeDeny {
            execution: self.floor.execution().clone(),
            episode: fixture::launch(7, &self.floor.execution().source).episode,
        }
    }
}

#[test]
fn p5a2_native_cursor_floor_are_authority_not_marker() {
    for ahead in [false, true] {
        let mut f = Fixture::new(ahead);
        f.prepare();
        let original = f.store.cursor(&f.floor.execution().source).unwrap().clone();
        let bytes = std::fs::read(f.path()).unwrap();
        let projected = f.read();
        assert_eq!(projected.cursor(), &original);
        assert_eq!(
            projected.evidence().floors(),
            &[SubmitFloor::NativeFloor(Box::new(f.floor.clone()))]
        );
        assert_eq!(
            projected.checkpoint_reason(),
            if ahead {
                CheckpointReason::FloorAheadOfCursor
            } else {
                CheckpointReason::NoProcessedWitness
            }
        );
        assert_eq!(
            std::fs::read(f.path()).unwrap(),
            bytes,
            "read-back has no metadata writes"
        );
        let mut foreign = f.floor.execution().clone();
        foreign.proof_seq += 1;
        assert_eq!(
            f.store.codex_read_back(&foreign).unwrap_err(),
            P::IdentityMismatch
        );
        let native = &f.floor.execution().source.path;
        let mut changed = std::fs::read(native).unwrap();
        changed[1] ^= 1;
        std::fs::write(native, changed).unwrap();
        assert_eq!(
            f.store.codex_read_back(f.floor.execution()).unwrap_err(),
            P::PrefixChanged
        );
    }
}

#[test]
fn p5a2_schema1_readback_is_owned_and_read_only() {
    let mut f = Fixture::new(false);
    let span = fixture::terminal(&f.floor);
    let original = CodexProvenance {
        schema: 1,
        channel: 7,
        spans: vec![span.clone()],
        denies: vec![],
    };
    let bytes = serde_json::to_vec(&original).unwrap();
    std::fs::write(f.path(), &bytes).unwrap();
    let read = f.read();
    assert_eq!(read.evidence().spans(), std::slice::from_ref(&span));
    assert!(read.evidence().floors().is_empty());
    assert_eq!(std::fs::read(f.path()).unwrap(), bytes);
    let deny = CodexEpisodeDeny {
        execution: span.execution.clone(),
        episode: span.episode.clone(),
    };
    let mut wrong_owner = deny.clone();
    wrong_owner.episode.request_owner_id += 1;
    assert_eq!(
        f.store.record_codex_deny(wrong_owner).unwrap_err(),
        P::IdentityMismatch
    );
    assert_eq!(std::fs::read(f.path()).unwrap(), bytes);
    f.store.record_codex_deny(deny.clone()).unwrap();
    assert!(
        read.evidence().denies().is_empty(),
        "owned read-back cannot change beneath a consumer"
    );
    let next = f.read();
    assert_eq!(next.evidence().denies(), &[deny]);
    assert_eq!(next.evidence().spans(), &[span]);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(f.path()).unwrap()).unwrap()["schema"],
        2
    );
}

struct Hook;
impl Drop for Hook {
    fn drop(&mut self) {
        mutation::COMMIT_HOOK.with(|slot| slot.borrow_mut().take());
    }
}

#[test]
fn p5a2_deny_failure_keeps_original_evidence() {
    for stage in [
        mutation::CommitStage::Renamed,
        mutation::CommitStage::ParentSynced,
        mutation::CommitStage::ReadBack,
    ] {
        let mut f = Fixture::new(false);
        f.prepare();
        let original = f.read();
        let deny = f.deny();
        let token = crate::services::provider::CancelToken::new();
        token.publish_cancel("cancel before disk");
        mutation::COMMIT_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |at| {
                if at == stage {
                    Err(std::io::Error::other("deny commit failpoint"))
                } else {
                    Ok(())
                }
            }))
        });
        let hook = Hook;
        assert_eq!(
            f.store.record_codex_deny(deny.clone()).unwrap_err(),
            P::StorageFailed
        );
        drop(hook);
        assert!(token.cancelled.load(std::sync::atomic::Ordering::Acquire));
        let disk: submit_floor::Schema2 =
            serde_json::from_slice(&std::fs::read(f.path()).unwrap()).unwrap();
        assert_eq!(disk.floors, original.evidence().floors());
        assert_eq!(disk.spans, original.evidence().spans());
        assert!(disk.denies.contains(&deny));
        assert_eq!(
            f.store.codex_read_back(f.floor.execution()).unwrap_err(),
            P::StorageFailed
        );
        assert!(f.store.ledger().unresolved().is_none());
    }
    let mut f = Fixture::new(false);
    f.prepare();
    let deny = f.deny();
    let cursor = f.store.cursor(&f.floor.execution().source).unwrap().clone();
    let recorded = f.store.record_codex_deny(deny.clone()).unwrap();
    assert_eq!(recorded.deny(), &deny);
    let before = std::fs::read(f.path()).unwrap();
    f.store.record_codex_deny(deny).unwrap();
    assert_eq!(std::fs::read(f.path()).unwrap(), before);
    assert_eq!(
        f.store.cursor(&f.floor.execution().source).unwrap(),
        &cursor
    );
}

#[test]
fn p5a2_deny_readback_mismatch_and_foreign_identity_are_typed() {
    let mut f = Fixture::new(false);
    f.prepare();
    let mut deny = f.deny();
    deny.execution.proof_seq += 1;
    let before = std::fs::read(f.path()).unwrap();
    assert_eq!(
        f.store.record_codex_deny(deny).unwrap_err(),
        P::IdentityMismatch
    );
    assert_eq!(std::fs::read(f.path()).unwrap(), before);
    let mut wrong_owner = f.deny();
    wrong_owner.episode.request_owner_id += 1;
    assert_eq!(
        f.store.record_codex_deny(wrong_owner).unwrap_err(),
        P::IdentityMismatch
    );
    assert_eq!(std::fs::read(f.path()).unwrap(), before);
    let path = f.path();
    mutation::COMMIT_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |stage| {
            if stage == mutation::CommitStage::ReadBack {
                std::fs::write(&path, b"{corrupt")?;
            }
            Ok(())
        }))
    });
    let _hook = Hook;
    assert_eq!(
        f.store.record_codex_deny(f.deny()).unwrap_err(),
        P::ReadBackMismatch
    );
}
