use chrono::Utc;

use super::super::tests::{enabled, initialized};
use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource};

#[test]
fn a_decided_boundary_is_never_rewritten_and_an_unowed_source_keeps_its_spool() {
    let runtime = tempfile::tempdir().unwrap();
    let path = runtime.path().join("b.jsonl");
    std::fs::write(&path, b"L1\n").unwrap();
    let source = source_id_for("s2", &path).unwrap();
    let store = enabled(runtime.path());
    let init = |channel| Ok(initialized(channel, Vec::new()));
    let era = store.begin_era(&[7], Utc::now(), init).unwrap();
    let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
    channel.set_limits_for_test(1, u64::MAX);
    channel.attach_source(&source).unwrap();
    let mut capture = SourceCapture::open(source.clone(), 0).unwrap();
    for grown in [&b"L2\n"[..], b""] {
        let CaptureOutcome::Batch(batch) = capture.poll(1 << 20) else {
            panic!("capture failed");
        };
        channel
            .append_spool(&batch, &capture.prefix_hash())
            .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, grown).unwrap();
    }
    let link = |boundary| SourceLink {
        source: source.clone(),
        seq: 2,
        parent: None,
        committed_at: Utc::now(),
        boundary,
    };
    let mut rotation = Rotation::default();
    let key = source_key(&source);
    rotation.links.insert(
        key.clone(),
        link(Boundary::Pending {
            candidates: vec![0],
        }),
    );
    channel.write_rotation(&rotation).unwrap();
    assert!(channel.gc_oldest_segment(&source).is_err());
    assert_eq!(channel.retained_segments(&source), 2);
    rotation
        .links
        .insert(key.clone(), link(Boundary::Owed { from: 0 }));
    assert!(channel.write_rotation(&rotation).is_err());
    rotation.links.clear();
    assert!(
        channel.write_rotation(&rotation).is_err(),
        "a link is never dropped"
    );
    let reopened = store.open_channel(&era, 7).unwrap().unwrap();
    let kept = reopened.rotation().unwrap().links[&key].boundary.clone();
    assert_eq!(
        kept,
        Boundary::Pending {
            candidates: vec![0]
        }
    );
}
