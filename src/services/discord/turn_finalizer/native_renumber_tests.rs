//! A transcript whose volume renumbered after the writer stored it: native settlement follows the
//! live identity a prefix-checked reopen proved, and refuses a dev no proof names.
use super::*;
use crate::services::tui_o::shadow::capture::renumber;

/// The dev the writer stored before the renumber; the live stat names the fixture's own.
const STORED: u64 = 1 << 40;
/// A later renumber that moves the live file off the dev the input registered.
const LATER: u64 = 1 << 41;

#[tokio::test]
async fn native_8448_renumbered_source_settles_only_under_its_proven_live_identity_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use sha2::Digest;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (case, (later, settles)) in [(0, true), (LATER, false)].into_iter().enumerate() {
        let channel = 84480001 + case as u64;
        let path = runtime.path().join(format!("runtime/renumber-{case}.json"));
        let f = Fixture::at(path, channel, true);
        let own_end = supply_rows(&f, false, true);
        let source = f.ticket.source.as_ref().unwrap();
        let bytes = std::fs::read(&source.transcript).unwrap();
        let prefix = hex::encode(sha2::Sha256::digest(&bytes[..own_end as usize]));
        let _moved = (later != 0).then(|| renumber::shift(&source.transcript, later));
        let shared = b2b_shared(Some(pool.clone()));
        let (key, actor) = b2b_active(&f, &shared).await;
        b2b_row(&f);
        let root = runtime.path().join(format!("renumber-store-{case}"));
        let stored = source.file[0] ^ STORED;
        let mut capture = SupplyCapture::resume_at_dev(&f, &root, true, (own_end, prefix), stored);
        supply_register(&f, &shared, key, Some(actor)).await;
        let port = super::super::native_supply::port(channel, shared, capture.bindings.clone());
        capture.sources.offer_native(&port).await;
        let settled = f.history.native_settlement("native-fixture", &f.ticket);
        let outcome = settled.unwrap().outcome.map(|o| o.disposition);
        assert_eq!(outcome, settles.then_some(Disposition::Executed), "{case}");
        let row = crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            channel,
        );
        assert_eq!(row.is_none(), settles, "{case}");
    }
    pool.close().await;
    db.drop().await;
}
