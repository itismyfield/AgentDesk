//! Terminal rows through the writer poll: a stored interrupt barrier and a raw EOF closer.
use super::*;
use crate::services::tui_o::shadow::capture::Fingerprint;
use crate::services::tui_o::writer::input_facts::native::terminal::{RawWitness, Tail};

async fn stored_interrupt_barrier_replays(boot: bool) {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::tui_o::writer::input_facts::native::{Barrier, registrations_fingerprint};
    let mark = "[Request interrupted by user]";
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let root = runtime.path().join("runtime");
    let f = Fixture::at(root.join("injected_frames.json"), 81480481, true);
    let rows = [
        serde_json::json!({"type":"user","uuid":"own","sessionId":"native","message":{"content":"한국어 원본 입력"}}),
        serde_json::json!({"type":"user","uuid":"stop","sessionId":"native","message":{"content":[{"type":"text","text":mark}]}}),
    ]
    .map(|row| format!("{row}\n"));
    std::fs::write(&f.ticket.source.as_ref().unwrap().transcript, rows.concat()).unwrap();
    let mut shared = b2b_shared(Some(pool.clone()));
    let (key, actor) = b2b_active(&f, &shared).await;
    b2b_row(&f);
    supply_register(&f, &shared, key, Some(actor)).await;
    barrier_receipt(&f);
    let registrations = f.history.native_classification_registrations().unwrap();
    let (own, end) = (rows[0].len() as u64, rows.concat().len() as u64);
    let hashes = crate::services::tui_input::human_frame::history::v2::native::frame_hashes(mark);
    let stamp = registrations_fingerprint(&registrations).unwrap();
    let barrier: Barrier = serde_json::from_value(serde_json::json!({"namespace":"native-fixture","ticket":f.ticket,"membership_range":[0,own],"membership_row":"own","foreign_range":[own,end],"foreign_row":"stop","foreign_hashes":hashes,"through":end,"registrations_fingerprint":stamp})).unwrap();
    assert!(f.history.record_native_barrier(&barrier).unwrap());
    if boot {
        let inputs = super::super::native_prepare::BootInputs::prepare(&root, 2).unwrap();
        shared = b2b_shared(Some(pool.clone()));
        Arc::get_mut(&mut shared)
            .unwrap()
            .restart
            .current_generation = 2;
        inputs.restore(&shared).await.unwrap();
    }
    let mut capture = SupplyCapture::new(&f, &runtime.path().join("terminal-store"), true);
    capture.poll(&shared).await;
    let saved = f
        .history
        .native_settlement("native-fixture", &f.ticket)
        .unwrap();
    let outcome = saved
        .outcome
        .as_ref()
        .expect("the stored barrier is superseded");
    assert_eq!(outcome.disposition, Disposition::Cancelled);
    let proof = &outcome.extra["native_proof"];
    assert_eq!(proof["supersedes"]["interrupt_row"], "stop");
    assert!(saved.cleanup_complete);
    assert_eq!(saved.barrier("native-fixture").unwrap(), Some(barrier));
    let row = crate::services::discord::inflight::load_inflight_state_read_only(
        &ProviderKind::Claude,
        key.channel_id.get(),
    );
    assert!(row.is_none(), "the result cleans the original row once");
    let mut record = f
        .history
        .native_boot_record("native-fixture", &f.ticket)
        .unwrap();
    let validate = super::super::native_prepare::validate_saved_record;
    validate(&record).unwrap();
    record.settlement.extra["foreign_prompt_barrier"]["foreign_row"] = "other".into();
    let refused = validate(&record).unwrap_err();
    assert_eq!(refused, "native execution crosses foreign prompt barrier");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn native_8148_terminal_stored_interrupt_barrier_replays_live_and_after_boot_pg() {
    stored_interrupt_barrier_replays(false).await;
    stored_interrupt_barrier_replays(true).await;
}

#[tokio::test]
async fn native_8148_terminal_raw_eof_waits_for_the_execution_end_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use std::io::Write;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let f = Fixture::at(
        runtime.path().join("runtime/injected_frames.json"),
        81480482,
        true,
    );
    let source = f.ticket.source.as_ref().unwrap();
    let own = serde_json::json!({"type":"user","uuid":"own","sessionId":"native","message":{"content":"한국어 원본 입력"}});
    let closer =
        serde_json::json!({"type":"system","subtype":"turn_duration","sessionId":"native"});
    std::fs::write(&source.transcript, format!("{own}\n{closer}")).unwrap();
    let shared = b2b_shared(Some(pool.clone()));
    let (key, actor) = b2b_active(&f, &shared).await;
    b2b_row(&f);
    supply_register(&f, &shared, key, Some(actor)).await;
    barrier_receipt(&f);
    let mut capture = SupplyCapture::new(&f, &runtime.path().join("terminal-store"), true);
    let settled = || {
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
    };
    let row = || {
        let channel = key.channel_id.get();
        crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            channel,
        )
    };
    capture.poll(&shared).await;
    assert_eq!(
        settled().outcome,
        None,
        "no execution end, no raw closer result"
    );
    assert!(row().is_some());
    let (through, len) = (
        own.to_string().len() as u64 + 1,
        std::fs::metadata(&source.transcript).unwrap().len(),
    );
    let nonce = source.target["execution_nonce"].as_str().unwrap();
    let (observed, current) = raw_observation(&f, &shared, (through, len), nonce);
    let offer = super::super::native_supply::source_evidence;
    let superseded = offer(&observed, &current, 0, &source.target).unwrap_err();
    assert_eq!(
        superseded, "native observation superseded",
        "a changed tail drops its proof"
    );
    assert!(
        offer(&observed, &observed, 0, &source.target)
            .unwrap()
            .raw
            .is_some()
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source.transcript)
        .unwrap();
    file.write_all(b"\n").unwrap();
    capture.poll(&shared).await;
    let saved = settled();
    let outcome = saved
        .outcome
        .as_ref()
        .expect("the completed record settles");
    assert_eq!(outcome.disposition, Disposition::Executed);
    assert!(outcome.extra["native_proof"]["raw"].is_null());
    assert!(saved.cleanup_complete && row().is_none());
    let validate = super::super::native_prepare::validate_saved_record;
    for (execution, accepted) in [(nonce, true), ("another-execution", false)] {
        let mut record = f
            .history
            .native_boot_record("native-fixture", &f.ticket)
            .unwrap();
        let proof = &mut record.settlement.outcome.as_mut().unwrap().extra["native_proof"];
        let range = serde_json::from_value::<[u64; 2]>(proof["closer_range"].clone()).unwrap();
        let raw = witness(&f, range.into(), execution, String::new());
        proof["raw"] = serde_json::to_value(raw).unwrap();
        let checked = validate(&record).map_err(|e| e == "native boot result proof mismatch");
        assert_eq!(
            checked,
            if accepted { Ok(()) } else { Err(true) },
            "{execution}"
        );
    }
    pool.close().await;
    db.drop().await;
}

/// A raw closer witness for `[through, len)` of the fixture transcript.
fn witness(f: &Fixture, (through, len): (u64, u64), nonce: &str, stamp: String) -> RawWitness {
    let source = f.ticket.source.as_ref().unwrap();
    let (dev, ino, modified) = (
        source.file[0],
        source.file[1],
        std::time::SystemTime::UNIX_EPOCH,
    );
    let fingerprint = Fingerprint {
        len,
        dev,
        ino,
        modified,
        change: (0, 0),
    };
    let tail = Tail {
        through,
        prefix_hash: String::new(),
        fingerprint,
        digest: "0".repeat(64),
    };
    let (execution_nonce, registrations_fingerprint) = (nonce.into(), stamp);
    RawWitness {
        version: 1,
        tail,
        aborted: false,
        execution_nonce,
        registrations_fingerprint,
    }
}

/// The O observation of a proven raw closer `[through, len)`, and the same observation once the
/// capture saw the tail change and dropped the witness.
fn raw_observation(
    f: &Fixture,
    shared: &Arc<SharedData>,
    (through, len): (u64, u64),
    nonce: &str,
) -> (Observation, Observation) {
    use crate::services::tui_o::writer::input_facts::native::{
        Projection, registrations_fingerprint,
    };
    use sha2::Digest;
    let source = f.ticket.source.as_ref().unwrap();
    let bytes = std::fs::read(&source.transcript).unwrap();
    let hash = hex::encode(sha2::Sha256::digest(&bytes[..through as usize]));
    let (session_id, path) = (source.session_id.clone(), source.transcript.clone());
    let id = SourceId {
        session_id,
        path,
        dev: source.file[0],
        ino: source.file[1],
    };
    let (channel_id, provider) = (f.ticket.channel_id, ShadowProvider::Claude);
    let binding = SourceBinding {
        channel_id,
        provider,
        source: id,
    };
    let seed = Projection::reopen(binding.clone(), through, &hash)
        .unwrap()
        .1
        .unwrap();
    let registrations = f.history.native_classification_registrations().unwrap();
    let stamp = registrations_fingerprint(&registrations).unwrap();
    let raw = witness(f, (through, len), nonce, stamp.clone());
    let analysis = seed.analyze_closed(&registrations, Some(&raw)).unwrap();
    let observed = Observation {
        binding,
        generation: shared.restart.current_generation,
        binding_seq: 1,
        revision: 1,
        through,
        registrations_fingerprint: stamp,
        terminal: Some(raw),
        evidence: analysis.evidence,
        receipts: Vec::new(),
    };
    let current = Observation {
        terminal: None,
        ..observed.clone()
    };
    (observed, current)
}
