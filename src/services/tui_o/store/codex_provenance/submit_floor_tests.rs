use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource};
use crate::services::tui_o::store::codex_provenance::{CODEX_PROVENANCE_FILE, SIDECAR_IO, Sidecar};
use crate::services::tui_o::store::tests::{enabled, initialized};
use crate::services::tui_o::store::{ChannelStore, STORE_DIR_NAME};
use crate::services::tui_o::writer::owed::{Reevaluation, reevaluate};
use std::path::{Path, PathBuf};

pub(crate) fn native_file(root: &Path) -> SourceId {
    let path = root.join("native.jsonl");
    let line = serde_json::json!({"type":"session_meta","payload":{
        "id":"native-session", "cwd":"/project", "source":"cli"}});
    std::fs::write(&path, format!("{line}\n")).unwrap();
    source_id_for("native-session", &path).unwrap()
}

pub(crate) fn launch(channel: u64, source: &SourceId) -> PreparedLaunch {
    PreparedLaunch {
        context: BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: "execution-future".into(),
            tmux_session: "test-pane".into(),
            channel_id: Some(channel),
            owner_runtime_root: "/runtime/owner".into(),
            host: None,
            expected_native_session_id: Some(source.session_id.clone()),
            launch_mode: "resume".into(),
            provider_root: source.path.parent().map(Path::to_path_buf),
            first_prompt_digest: Some(format!("sha256:{}", "a".repeat(64))),
            source_policy: Some("verified".into()),
        },
        episode: TurnEpisodeRef {
            channel_id: channel,
            user_message_id: 30,
            request_owner_id: 8,
            turn_nonce: "turn-future".into(),
            native_turn_id: String::new(),
        },
    }
}

pub(crate) fn native_floor(launch: PreparedLaunch, cursor: &Cursor) -> NativeFloor {
    let execution = ExecutionProofRef {
        owner_runtime_root: launch.context.owner_runtime_root.clone(),
        tmux_session: launch.context.tmux_session.clone(),
        execution_nonce: launch.context.execution_nonce.clone(),
        proof_seq: 4,
        source: cursor.source.clone(),
    };
    NativeFloor::capture(launch, execution, cursor, 1).unwrap()
}

pub(crate) fn terminal(floor: &NativeFloor) -> CodexEpisodeSpan {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&floor.canonical_source.path)
        .unwrap();
    for kind in ["task_started", "task_complete"] {
        let record = serde_json::json!({"type":"event_msg","payload":{
            "type":kind, "turn_id":"native-future"}});
        writeln!(file, "{record}").unwrap();
    }
    let mut episode = floor.launch.episode.clone();
    episode.native_turn_id = "native-future".into();
    CodexEpisodeSpan {
        execution: floor.execution.clone(),
        episode,
        delivery_channel_id: floor.launch.episode.channel_id,
        offset_authority_channel_id: floor.launch.episode.channel_id,
        generation_mtime_ns: 1,
        start: floor.through,
        end: Some(file.metadata().unwrap().len()),
    }
}

struct Fixture {
    root: tempfile::TempDir,
    store: ChannelStore,
    floor: NativeFloor,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = native_file(root.path());
        let owner = enabled(root.path());
        let era = owner
            .begin_era(&[7], chrono::Utc::now(), |c| Ok(initialized(c, Vec::new())))
            .unwrap();
        let mut store = owner.open_channel(&era, 7).unwrap().unwrap();
        store.attach_source(&source).unwrap();
        let mut capture = SourceCapture::open(source.clone(), 0).unwrap();
        let CaptureOutcome::Batch(batch) = capture.poll(1 << 20) else {
            panic!("capture failed")
        };
        store.append_spool(&batch, &capture.prefix_hash()).unwrap();
        let floor = native_floor(launch(7, &source), store.cursor(&source).unwrap());
        Self { root, store, floor }
    }

    fn path(&self) -> PathBuf {
        self.root
            .path()
            .join(STORE_DIR_NAME)
            .join("7")
            .join(CODEX_PROVENANCE_FILE)
    }

    fn document(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.path()).unwrap()).unwrap()
    }
}

fn old_evidence(floor: &NativeFloor) -> CodexProvenance {
    let mut a = CodexEpisodeSpan {
        execution: floor.execution.clone(),
        episode: floor.launch.episode.clone(),
        delivery_channel_id: 7,
        offset_authority_channel_id: 7,
        generation_mtime_ns: 1,
        start: 0,
        end: Some(floor.through),
    };
    a.episode.user_message_id = 10;
    a.episode.turn_nonce = "old-closed".into();
    a.episode.native_turn_id = "old-native".into();
    let mut b = a.clone();
    b.start = floor.through;
    b.end = None;
    b.episode.user_message_id = 20;
    b.episode.turn_nonce = "old-open".into();
    b.episode.native_turn_id = "open-native".into();
    let deny = super::super::CodexEpisodeDeny {
        execution: a.execution.clone(),
        episode: a.episode.clone(),
    };
    CodexProvenance {
        schema: 1,
        channel: 7,
        spans: vec![a, b],
        denies: vec![deny],
    }
}

fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push((
                    path.strip_prefix(root).unwrap().to_owned(),
                    std::fs::read(path).unwrap(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

#[test]
fn s2a_first_explicit_prepare_preserves_schema1_order_identity_ranges_and_denies() {
    let mut fixture = Fixture::new();
    let old = old_evidence(&fixture.floor);
    std::fs::write(fixture.path(), serde_json::to_vec(&old).unwrap()).unwrap();
    let before = tree(fixture.root.path());
    assert_eq!(
        reevaluate(&mut fixture.store, &mut []).unwrap(),
        Reevaluation::Present { attributed: 0 }
    );
    assert_eq!(
        tree(fixture.root.path()),
        before,
        "read-side migration is forbidden"
    );
    fixture
        .store
        .prepare_codex_floor(SubmitFloor::NativeFloor(Box::new(fixture.floor.clone())))
        .unwrap();
    let next = fixture.document();
    assert_eq!(next["schema"], 2);
    assert_eq!(next["spans"], serde_json::to_value(&old.spans).unwrap());
    assert_eq!(next["denies"], serde_json::to_value(&old.denies).unwrap());
    assert_eq!(next["floors"].as_array().unwrap().len(), 1);
    assert!(
        serde_json::from_value::<CodexProvenance>(next.clone()).is_err(),
        "fixed old decoder rejects schema2"
    );
    assert!(matches!(
        fixture.store.codex_provenance(),
        Sidecar::Present(_)
    ));
    let bytes = std::fs::read(fixture.path()).unwrap();
    fixture
        .store
        .prepare_codex_floor(SubmitFloor::NativeFloor(Box::new(fixture.floor.clone())))
        .unwrap();
    assert_eq!(
        std::fs::read(fixture.path()).unwrap(),
        bytes,
        "retry preserves the exact evidence"
    );
}

#[test]
fn s2a_existing_read_caller_really_reads_missing_schema1_corrupt_and_future_without_writes() {
    let mut fixture = Fixture::new();
    let old = old_evidence(&fixture.floor);
    let mut future = serde_json::to_value(&old).unwrap();
    future["schema"] = 99.into();
    let mut unknown = serde_json::to_value(&old).unwrap();
    unknown["floors"] = serde_json::json!([]);
    for bytes in [
        None,
        Some(serde_json::to_vec(&old).unwrap()),
        Some(b"{torn".to_vec()),
        Some(serde_json::to_vec(&future).unwrap()),
        Some(serde_json::to_vec(&unknown).unwrap()),
    ] {
        if fixture.path().exists() {
            std::fs::remove_file(fixture.path()).unwrap();
        }
        if let Some(bytes) = &bytes {
            std::fs::write(fixture.path(), bytes).unwrap();
        }
        let before = tree(fixture.root.path());
        SIDECAR_IO.with(|io| io.set((0, 0)));
        let observed = reevaluate(&mut fixture.store, &mut []).unwrap();
        assert_eq!(
            SIDECAR_IO.with(std::cell::Cell::get),
            (1, 0),
            "the read must execute"
        );
        assert_eq!(
            matches!(observed, Reevaluation::Present { .. }),
            bytes
                .as_ref()
                .is_some_and(|v| *v == serde_json::to_vec(&old).unwrap())
        );
        assert_eq!(tree(fixture.root.path()), before);
    }
}

#[test]
fn s2a_prepare_rejects_corrupt_foreign_unknown_and_conflicting_floors_without_repair() {
    let mut fixture = Fixture::new();
    let old = old_evidence(&fixture.floor);
    let mut foreign = serde_json::to_value(&old).unwrap();
    foreign["channel"] = 8.into();
    let mut unknown = serde_json::to_value(&old).unwrap();
    unknown["future"] = true.into();
    let mut nested = serde_json::to_value(&old).unwrap();
    nested["spans"][0]["execution"]["future"] = true.into();
    for bytes in [
        b"{torn".to_vec(),
        serde_json::to_vec(&foreign).unwrap(),
        serde_json::to_vec(&unknown).unwrap(),
        serde_json::to_vec(&nested).unwrap(),
    ] {
        std::fs::write(fixture.path(), &bytes).unwrap();
        assert!(
            fixture
                .store
                .prepare_codex_floor(SubmitFloor::NativeFloor(Box::new(fixture.floor.clone())))
                .is_err()
        );
        assert_eq!(std::fs::read(fixture.path()).unwrap(), bytes);
        assert_eq!(fixture.store.io_failure(), None);
    }
    std::fs::remove_file(fixture.path()).unwrap();
    let prepared = SubmitFloor::PreparedLaunch(Box::new(fixture.floor.launch.clone()));
    fixture.store.prepare_codex_floor(prepared).unwrap();
    let span = terminal(&fixture.floor);
    assert!(
        fixture.store.close_codex_episode(span.clone()).is_err(),
        "unbound prepared launch has no native floor"
    );
    fixture
        .store
        .prepare_codex_floor(SubmitFloor::NativeFloor(Box::new(fixture.floor.clone())))
        .unwrap();
    let before = std::fs::read(fixture.path()).unwrap();
    let mut wrong = fixture.floor.clone();
    wrong.launch.episode.request_owner_id += 1;
    assert!(
        fixture
            .store
            .prepare_codex_floor(SubmitFloor::NativeFloor(Box::new(wrong)))
            .is_err()
    );
    assert_eq!(std::fs::read(fixture.path()).unwrap(), before);
    let closed = fixture.store.close_codex_episode(span.clone()).unwrap();
    assert_eq!(closed.span(), &span);
    assert_eq!(
        fixture.store.close_codex_episode(span).unwrap().span(),
        closed.span()
    );
}

#[test]
fn s2a_prepare_and_close_never_ack_before_parent_sync_and_validated_readback() {
    use super::super::mutation::{COMMIT_HOOK, CommitStage};
    use std::sync::{Arc, Mutex};
    struct Hook;
    impl Drop for Hook {
        fn drop(&mut self) {
            COMMIT_HOOK.with(|hook| hook.borrow_mut().take());
        }
    }
    for close in [false, true] {
        for failure in [
            Some(CommitStage::Renamed),
            Some(CommitStage::ReadBack),
            None,
        ] {
            let mut fixture = Fixture::new();
            if close {
                fixture
                    .store
                    .prepare_codex_floor(SubmitFloor::NativeFloor(Box::new(fixture.floor.clone())))
                    .unwrap();
            } else {
                let old = old_evidence(&fixture.floor);
                std::fs::write(fixture.path(), serde_json::to_vec(&old).unwrap()).unwrap();
            }
            let span = terminal(&fixture.floor);
            let stages = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&stages);
            COMMIT_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |stage| {
                    seen.lock().unwrap().push(stage);
                    if Some(stage) == failure {
                        Err(std::io::Error::other("injected publication failure"))
                    } else {
                        Ok(())
                    }
                }))
            });
            let _hook = Hook;
            let acknowledged = if close {
                fixture
                    .store
                    .close_codex_episode(span.clone())
                    .map(|closed| assert_eq!(closed.span(), &span))
            } else {
                fixture
                    .store
                    .prepare_codex_floor(SubmitFloor::NativeFloor(Box::new(fixture.floor.clone())))
            };
            assert_eq!(
                acknowledged.is_ok(),
                failure.is_none(),
                "close={close}, failure={failure:?}"
            );
            if failure.is_some() {
                assert!(fixture.store.io_failure().is_some());
                assert!(
                    matches!(fixture.store.codex_provenance(), Sidecar::Unavailable(_)),
                    "visible renamed bytes are not an acknowledged close"
                );
            } else {
                assert_eq!(
                    *stages.lock().unwrap(),
                    [
                        CommitStage::Renamed,
                        CommitStage::ParentSynced,
                        CommitStage::ReadBack
                    ]
                );
            }
            if !close {
                let old = old_evidence(&fixture.floor);
                assert_eq!(
                    fixture.document()["spans"],
                    serde_json::to_value(old.spans).unwrap()
                );
                assert_eq!(
                    fixture.document()["denies"],
                    serde_json::to_value(old.denies).unwrap()
                );
            }
        }
    }
}

#[test]
fn s2a_readback_revalidates_bytes_and_closed_native_identity() {
    use super::super::mutation::{COMMIT_HOOK, CommitStage};
    let mut fixture = Fixture::new();
    fixture
        .store
        .prepare_codex_floor(SubmitFloor::NativeFloor(Box::new(fixture.floor.clone())))
        .unwrap();
    let span = terminal(&fixture.floor);
    let before = std::fs::read(fixture.path()).unwrap();
    let mut wrong = span.clone();
    wrong.generation_mtime_ns += 1;
    assert!(fixture.store.close_codex_episode(wrong).is_err());
    let mut open = span.clone();
    open.end = None;
    assert!(fixture.store.close_codex_episode(open).is_err());
    let mut foreign = span.clone();
    foreign.episode.native_turn_id = "foreign".into();
    assert!(fixture.store.close_codex_episode(foreign).is_err());
    assert_eq!(std::fs::read(fixture.path()).unwrap(), before);
    let path = fixture.path();
    COMMIT_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |stage| {
            if stage == CommitStage::ReadBack {
                std::fs::write(&path, b"{torn")?;
            }
            Ok(())
        }))
    });
    let result = fixture.store.close_codex_episode(span);
    COMMIT_HOOK.with(|hook| hook.borrow_mut().take());
    assert!(
        result.is_err(),
        "a serializer copy cannot replace validated read-back"
    );
    assert!(fixture.store.io_failure().is_some());
}

#[test]
fn s2a_schema2_rejects_unknown_floor_context_fields_and_identity_loss() {
    let mut fixture = Fixture::new();
    fixture
        .store
        .prepare_codex_floor(SubmitFloor::NativeFloor(Box::new(fixture.floor.clone())))
        .unwrap();
    let good = fixture.document();
    for variant in 0..9 {
        let mut bad = good.clone();
        match variant {
            0 => bad["floors"][0]["launch"]["context"]["future"] = true.into(),
            1 => bad["floors"][0]["future"] = true.into(),
            2 => bad["floors"][0]["launch"]["episode"]["request_owner_id"] = 0.into(),
            3 => bad["floors"][0]["execution"]["proof_seq"] = 0.into(),
            4 => bad["floors"][0]["prefix_hash"] = "invalid".into(),
            6 => bad["floors"][0]["launch"]["episode"]["future"] = true.into(),
            7 => bad["floors"][0]["execution"]["future"] = true.into(),
            8 => bad["floors"][0]["canonical_source"]["future"] = true.into(),
            _ => bad["schema"] = 3.into(),
        }
        let bytes = serde_json::to_vec(&bad).unwrap();
        std::fs::write(fixture.path(), &bytes).unwrap();
        assert!(
            matches!(fixture.store.codex_provenance(), Sidecar::Unavailable(_)),
            "accepted variant {variant}"
        );
        assert!(
            fixture
                .store
                .prepare_codex_floor(SubmitFloor::NativeFloor(Box::new(fixture.floor.clone())))
                .is_err()
        );
        assert_eq!(std::fs::read(fixture.path()).unwrap(), bytes);
    }
}
