//! A barrier stored for a strict interrupt row settles through the writer poll, live or restored.
use super::*;

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
