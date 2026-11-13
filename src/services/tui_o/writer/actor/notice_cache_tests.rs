use super::*;
use history::notice_owner::{Batch, Decision};

#[cfg(unix)]
fn notice_debt(actor: &mut Host) -> usize {
    let queued = actor
        .owed
        .iter()
        .filter_map(|item| match item {
            Derived::Piece(piece) if piece.unit_key.kind == UnitKind::InputNotice => {
                Some(piece.unit_key.clone())
            }
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>()
        .len();
    queued + actor.notice_debt_extra()
}

#[cfg(unix)]
fn scans() -> usize {
    history::notice_owner::CENSUS_READS.with(|count| count.get())
}

#[cfg(unix)]
fn reset_scans() {
    history::notice_owner::CENSUS_READS.with(|count| count.set(0));
}

#[cfg(unix)]
async fn drain(actor: &mut Host) {
    actor
        .deliver_owed(&crate::services::tui_o::writer::progress::Publisher::new(
            CHANNEL,
        ))
        .await;
}

#[cfg(unix)]
fn invalid_bytes(original: &[u8], variant: &str) -> Vec<u8> {
    let mut bytes = original.to_vec();
    match variant {
        "prefix" => {
            let at = bytes
                .windows(5)
                .position(|bytes| bytes == b"stale")
                .unwrap();
            bytes[at] = b'x';
        }
        "json" => bytes[0] = b'!',
        "uuid" => {
            let end = bytes.iter().position(|byte| *byte == b'\n').unwrap();
            let mut value: Value = serde_json::from_slice(&bytes[..end]).unwrap();
            value["uuid"] = Value::Null;
            let changed = serde_json::to_vec(&value).unwrap();
            assert!(changed.len() <= end);
            bytes[..end].fill(b' ');
            bytes[..changed.len()].copy_from_slice(&changed);
        }
        "newline" => *bytes.last_mut().unwrap() = b' ',
        _ => panic!("unknown invalid fixture"),
    }
    assert_eq!(bytes.len(), original.len());
    bytes
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn repeated_valid_notice_poll_reuses_census_and_posts_each_direct_once() {
    let c = Case::new("repeat-valid");
    let frame = c.frame();
    c.register(&frame);
    for id in ["owned", "direct-1", "direct-2"] {
        c.input(id, &frame);
    }
    let mut actor = c.open();
    let fresh = actor.read_sources();
    reset_scans();
    actor.prepare_notices(&fresh).await;
    assert_eq!(scans(), 1, "the first poll really scanned the source");
    assert_eq!(notice_debt(&mut actor), 2);
    actor.prepare_notices(&[]).await;
    assert_eq!(scans(), 1, "an unchanged poll adds no native scan");
    assert_eq!(
        actor.writer.store().ledger().excluded(&input_key("owned")),
        Some("input_notice_source")
    );
    drain(&mut actor).await;
    assert_eq!(c.notices(), 2);
    assert_eq!(notice_debt(&mut actor), 0);
    c.run(&mut actor).await;
    assert_eq!(c.notices(), 2, "polling never republishes settled notices");
    assert_eq!(scans(), 1);
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn repeated_stable_invalid_notice_poll_reuses_none_then_recovers() {
    for variant in ["prefix", "json", "uuid", "newline"] {
        let c = Case::new(variant);
        let frame = c.frame();
        c.register(&frame);
        c.input("stale", &frame);
        let mut actor = c.open();
        actor.read_sources();
        let original = std::fs::read(&c.path).unwrap();
        std::fs::write(&c.path, invalid_bytes(&original, variant)).unwrap();
        reset_scans();
        actor.prepare_notices(&[]).await;
        assert_eq!(scans(), 1, "{variant}: first invalid view was inspected");
        assert_eq!(notice_debt(&mut actor), 1);
        actor.prepare_notices(&[]).await;
        assert_eq!(scans(), 1, "{variant}: stable None must also be reusable");
        assert_eq!(notice_debt(&mut actor), 1);
        assert_eq!(c.notices(), 0);
        assert!(
            actor
                .writer
                .store()
                .ledger()
                .excluded(&input_key("stale"))
                .is_none()
        );
        std::fs::write(&c.path, original).unwrap();
        actor.prepare_notices(&[]).await;
        assert_eq!(
            scans(),
            2,
            "{variant}: repaired bytes invalidate cached None"
        );
        drain(&mut actor).await;
        c.excluded(&mut actor, "stale");
    }
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn exact_receipt_settles_while_native_invalid_snapshot_stays_cached() {
    let c = Case::new("invalid-exact");
    let frame = c.frame();
    c.register(&frame);
    c.input("stale", &frame);
    let mut actor = c.open();
    actor.read_sources();
    let original = std::fs::read(&c.path).unwrap();
    std::fs::write(&c.path, invalid_bytes(&original, "json")).unwrap();
    reset_scans();
    actor.prepare_notices(&[]).await;
    assert_eq!(scans(), 1);
    assert_eq!(notice_debt(&mut actor), 1);
    let origin = actor
        .writer
        .store()
        .ledger()
        .input_notices()
        .evidence
        .values()
        .find(|evidence| evidence.unit_key.as_ref() == Some(&input_key("stale")))
        .unwrap()
        .origin
        .clone()
        .unwrap();
    assert_eq!(
        history::receipt("notice-attempt", &c.path, 0, "user", Some("stale"), &frame),
        Some(true)
    );
    actor.prepare_notices(&[]).await;
    assert_eq!(
        scans(),
        1,
        "a late receipt does not require native rescanning"
    );
    assert_eq!(
        actor
            .writer
            .notice_owners
            .decision(&origin, "stale", &frame),
        Decision::Owned,
        "the exact proof precedes unavailable native census"
    );
    c.excluded(&mut actor, "stale");
    drain(&mut actor).await;
    assert_eq!(c.notices(), 0);
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn native_rewrite_after_cached_preflight_is_rechecked_before_notice_post() {
    use crate::services::tui_o::shadow::capture::Fingerprint;
    let c = Case::new("cached-preflight");
    let frame = c.frame();
    c.register(&frame);
    assert_eq!(
        history::receipt(
            "notice-attempt",
            &c.path,
            0,
            "user",
            Some("historical"),
            &frame
        ),
        Some(true)
    );
    c.input("stale", &frame);
    let mut actor = c.open();
    actor.read_sources();
    reset_scans();
    actor.prepare_notices(&[]).await;
    assert_eq!(scans(), 1);
    actor.prepare_notices(&[]).await;
    assert_eq!(scans(), 1);
    let original = std::fs::read(&c.path).unwrap();
    let before = Fingerprint::of(&std::fs::metadata(&c.path).unwrap()).unwrap();
    std::fs::write(&c.path, invalid_bytes(&original, "prefix")).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&c.path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(before.modified))
        .unwrap();
    let after = Fingerprint::of(&std::fs::metadata(&c.path).unwrap()).unwrap();
    assert_eq!(after.modified, before.modified);
    assert_eq!(after.len, before.len);
    assert_ne!(
        after.change, before.change,
        "ctime witnesses the hidden rewrite"
    );
    drain(&mut actor).await;
    assert_eq!(c.notices(), 0);
    assert_eq!(notice_debt(&mut actor), 1);
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .excluded(&input_key("stale"))
            .is_none()
    );
    std::fs::write(&c.path, original).unwrap();
    actor.prepare_notices(&[]).await;
    assert_eq!(scans(), 2);
    drain(&mut actor).await;
    assert_eq!(c.notices(), 1);
    assert_eq!(notice_debt(&mut actor), 0);
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn cold_notice_cache_preserves_unposted_and_settled_notice_counts() {
    let c = Case::new("cold-cache");
    let frame = c.frame();
    c.register(&frame);
    for id in ["owned", "direct-1", "direct-2"] {
        c.input(id, &frame);
    }
    let mut actor = c.open();
    actor.read_sources();
    reset_scans();
    actor.prepare_notices(&[]).await;
    assert_eq!(scans(), 1);
    actor.prepare_notices(&[]).await;
    assert_eq!(scans(), 1);
    assert_eq!(c.notices(), 0);
    drop(actor);
    let mut actor = c.open();
    actor.prepare_notices(&[]).await;
    assert_eq!(scans(), 2, "a new writer has a cold computational cache");
    drain(&mut actor).await;
    assert_eq!(c.notices(), 2);
    assert_eq!(notice_debt(&mut actor), 0);
    drop(actor);
    let mut actor = c.open();
    c.run(&mut actor).await;
    assert_eq!(c.notices(), 2);
    assert_eq!(
        scans(),
        2,
        "settled native debt needs no census even after restart"
    );
}

#[tokio::test(start_paused = true)]
async fn history_parse_reuse_obeys_raw_size_gate_and_rewrite() {
    use crate::utils::parsed_bytes::{BUILDS, forget};
    const LIMIT: usize = 256 * 1024;
    for bytes in [LIMIT - 1, LIMIT, LIMIT + 1] {
        let c = Case::new("history-gate");
        let frame = c.frame();
        c.register(&frame);
        c.input("direct", &frame);
        let mut actor = c.open();
        actor.read_sources();
        let origin = actor
            .writer
            .store()
            .ledger()
            .input_notices()
            .evidence
            .values()
            .find(|evidence| evidence.unit_key.as_ref() == Some(&input_key("direct")))
            .unwrap()
            .origin
            .clone()
            .unwrap();
        let path = c._guard.path().join("runtime/injected_frames.json");
        let mut raw = br#"{"entries":[]}"#.to_vec();
        raw.resize(bytes, b' ');
        std::fs::write(&path, raw).unwrap();
        forget(&path);
        BUILDS.with(|count| count.set(0));
        let first = Batch::prepare(actor.writer.store());
        assert_eq!(first.decision(&origin, "direct", &frame), Decision::Unowned);
        let cached = usize::from(bytes <= LIMIT);
        assert_eq!(
            BUILDS.with(|count| count.get()),
            cached,
            "first history parse gate {bytes}"
        );
        let second = Batch::prepare(actor.writer.store());
        assert_eq!(
            second.decision(&origin, "direct", &frame),
            Decision::Unowned
        );
        assert_eq!(
            BUILDS.with(|count| count.get()),
            cached,
            "same raw parse gate {bytes}"
        );
        if cached == 1 {
            let mut raw = br#"{"entries":[],"changed":true}"#.to_vec();
            raw.resize(bytes, b' ');
            std::fs::write(&path, raw).unwrap();
            let changed = Batch::prepare(actor.writer.store());
            assert_eq!(
                changed.decision(&origin, "direct", &frame),
                Decision::Unowned
            );
            assert_eq!(
                BUILDS.with(|count| count.get()),
                2,
                "new raw is parsed immediately"
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn transient_notice_census_failure_recovers_with_identical_key_and_posts_once() {
    use crate::services::tui_o::shadow::capture::Fingerprint;
    use history::notice_owner::{CensusStage, fail_census_once};
    for stage in [CensusStage::Open, CensusStage::Metadata, CensusStage::Read] {
        let c = Case::new(&format!("transient-{stage:?}"));
        let frame = c.frame();
        c.register(&frame);
        for id in ["owned", "direct"] {
            c.input(id, &frame);
        }
        let mut actor = c.open();
        actor.read_sources();
        let before_bytes = std::fs::read(&c.path).unwrap();
        let before_source = c.source.clone();
        let before_cursor = actor.writer.store().cursor(&c.source).unwrap().clone();
        let before_fp = Fingerprint::of(&std::fs::metadata(&c.path).unwrap()).unwrap();
        let assert_unchanged = |actor: &mut Host| {
            assert_eq!(c.source, before_source, "{stage:?}: SourceId stayed fixed");
            let cursor = actor.writer.store().cursor(&c.source).unwrap();
            assert_eq!(cursor.source, before_cursor.source);
            assert_eq!(cursor.captured_through, before_cursor.captured_through);
            assert_eq!(cursor.prefix_hash, before_cursor.prefix_hash);
            assert_eq!(std::fs::read(&c.path).unwrap(), before_bytes);
            assert_eq!(
                Fingerprint::of(&std::fs::metadata(&c.path).unwrap()).unwrap(),
                before_fp,
                "{stage:?}: recovery must not depend on fingerprint invalidation"
            );
        };
        reset_scans();
        let injection = fail_census_once(stage);
        actor.prepare_notices(&[]).await;
        drop(injection);
        assert_eq!(scans(), 1, "{stage:?}: the first scan attempt failed");
        assert_unchanged(&mut actor);
        assert_eq!(notice_debt(&mut actor), 2);
        assert_eq!(c.notices(), 0);
        for id in ["owned", "direct"] {
            assert!(
                actor
                    .writer
                    .store()
                    .ledger()
                    .excluded(&input_key(id))
                    .is_none()
            );
            assert!(
                actor
                    .writer
                    .store()
                    .ledger()
                    .latest_piece(&input_key(id), 0)
                    .is_none()
            );
        }
        actor.prepare_notices(&[]).await;
        assert_eq!(
            scans(),
            2,
            "{stage:?}: transient errors cannot become cached None"
        );
        assert_unchanged(&mut actor);
        assert_eq!(
            actor.writer.store().ledger().excluded(&input_key("owned")),
            Some("input_notice_source")
        );
        assert_eq!(notice_debt(&mut actor), 1);
        assert_eq!(c.notices(), 0);
        actor.prepare_notices(&[]).await;
        assert_eq!(scans(), 2, "{stage:?}: the recovered result is reusable");
        assert_unchanged(&mut actor);
        drain(&mut actor).await;
        assert_eq!(c.notices(), 1, "{stage:?}: one direct twin posts once");
        assert_eq!(notice_debt(&mut actor), 0);
        assert!(matches!(
            actor.writer.store().ledger().latest_piece(&input_key("direct"), 0),
            Some((_, record)) if matches!(record.outcome,
                Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(_)))
        ));
        c.run(&mut actor).await;
        assert_eq!(c.notices(), 1);
        assert_eq!(scans(), 2);
        assert_unchanged(&mut actor);
    }
}
