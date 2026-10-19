//! Requests queued before source selection must be checked at apply and reply receive.

use super::*;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource};
use crate::services::tui_o::store::codex_provenance::submit_floor::tests as native;
use crate::services::tui_o::store::{Initialized, OStore, STORE_DIR_NAME, StoreConfig};
use crate::services::tui_o::writer::owed::tests::{Alarms, Lease, Port as Discord};
use std::cell::Cell;
use std::path::{Path, PathBuf};

pub(in crate::services::tui_o::writer::actor) type Writer = ChannelWriter<Discord, Lease, Alarms>;

pub(in crate::services::tui_o::writer::actor) fn fixture()
-> (tempfile::TempDir, Writer, NativeFloor) {
    let root = tempfile::tempdir().unwrap();
    let source = native::native_file(root.path());
    let owner = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
        .unwrap()
        .unwrap();
    let era = owner
        .begin_era(&[7], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: Vec::new(),
                initial_anchor: 1,
                build_digest: "scoped-submission".into(),
                at: chrono::Utc::now(),
            })
        })
        .unwrap();
    let mut store = owner.open_channel(&era, 7).unwrap().unwrap();
    store.attach_source(&source).unwrap();
    let mut capture = SourceCapture::open(source.clone(), 0).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(1 << 20) else {
        panic!("fixture native metadata must be captured")
    };
    store.append_spool(&batch, &capture.prefix_hash()).unwrap();
    let floor = native::native_floor(native::launch(7, &source), store.cursor(&source).unwrap());
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let writer = ChannelWriter::new(
        store,
        gate,
        Arc::new(Discord::default()),
        Lease,
        Alarms::default(),
    );
    (root, writer, floor)
}

pub(in crate::services::tui_o::writer::actor) fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push((
                    path.strip_prefix(root).unwrap().into(),
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

#[tokio::test]
async fn queued_submission_is_rejected_at_apply_after_scope_freezes() {
    let _environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, mut writer, floor) = fixture();
    // The separate empty port selection isolates the actor's apply guard from its reply guard.
    let (port, mut receiver) = Port::new(41);
    let before = tree(&root.path().join(STORE_DIR_NAME));
    let source = floor.checkpoint().0.clone();
    let request = port.request(Operation::Prepare(SubmitFloor::NativeFloor(Box::new(
        floor,
    ))));
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    let queued = receiver.next().await.unwrap();
    writer.suspension.extend("successor".into(), [source]);
    receiver.apply(&mut writer, queued);
    assert_eq!(request.await.unwrap_err(), "suspended_sources");
    assert_eq!(tree(&root.path().join(STORE_DIR_NAME)), before);
}

#[tokio::test]
async fn empty_selection_reports_invalid_canonical_capture_without_store_write() {
    let _environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, mut writer, floor) = fixture();
    let (port, mut receiver) = Port::new(42);
    let launch = native::launch(7, floor.checkpoint().0);
    let mut execution = floor.execution().clone();
    execution.source.session_id.push_str("-unbound");
    let before = tree(&root.path().join(STORE_DIR_NAME));
    let request = port.request(Operation::Capture(Box::new((launch, execution))));
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    let queued = receiver.next().await.unwrap();
    receiver.apply(&mut writer, queued);
    assert_eq!(request.await.unwrap_err(), "canonical cursor unavailable");
    assert_eq!(tree(&root.path().join(STORE_DIR_NAME)), before);
}

#[tokio::test]
async fn submission_reply_received_after_scope_freezes_never_calls_consumer() {
    let _environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for freeze in [false, true] {
        let (root, mut writer, floor) = fixture();
        let (port, mut receiver) = Port::new(43);
        let port = port.with_suspension(writer.suspension.clone());
        let source = floor.checkpoint().0.clone();
        let before = tree(&root.path().join(STORE_DIR_NAME));
        let calls = Cell::new(0);
        let request = async {
            port.request(Operation::Prepare(SubmitFloor::NativeFloor(Box::new(
                floor,
            ))))
            .await
            .map(|reply| {
                calls.set(calls.get() + 1);
                reply
            })
        };
        tokio::pin!(request);
        assert!(futures::poll!(&mut request).is_pending());
        let queued = receiver.next().await.unwrap();
        receiver.apply(&mut writer, queued);
        assert_ne!(
            tree(&root.path().join(STORE_DIR_NAME)),
            before,
            "actual Receiver.apply must publish the valid native floor before reply is polled"
        );
        if freeze {
            writer.suspension.extend("successor".into(), [source]);
        }
        let after_apply = tree(&root.path().join(STORE_DIR_NAME));
        let result = request.await;
        if freeze {
            assert_eq!(result.unwrap_err(), "suspended_sources");
        } else {
            assert!(result.is_ok());
        }
        assert_eq!(calls.get(), usize::from(!freeze));
        assert_eq!(tree(&root.path().join(STORE_DIR_NAME)), after_apply);
    }
}
