//! A transcript whose volume renumbered after the writer stored it: the hook's re-pin of the same
//! file moves the writer onto the live identity, and only then does the native turn settle.
use super::*;
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvidence, BindingRecord, BindingTarget,
};

/// The dev the writer stored before the renumber; the live stat names the fixture's own.
const STORED: u64 = 1 << 40;

/// The re-pin a same-session hook logs once its stat finds the pinned file under a new dev.
fn repin(channel: u64, old: &SourceId, new: &SourceId) -> BindingEvent {
    BindingEvent {
        seq: 2,
        channel_id: channel,
        provider: ShadowProvider::Claude,
        tmux_session: "fixture-pane".into(),
        execution_nonce: "incarnation-1".into(),
        committed_at: chrono::Utc::now(),
        record: BindingRecord::Bound {
            old: Some(old.clone()),
            new: BindingTarget::Source(new.clone()),
            cause: BindingCause::Resume,
            parent_hint: None,
            evidence: BindingEvidence {
                hook_event: "UserPromptSubmit".into(),
                received_at: chrono::Utc::now(),
                reclaims: false,
            },
        },
    }
}

#[tokio::test]
async fn native_8448_hook_repin_of_a_renumbered_source_settles_the_native_turn_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use sha2::Digest;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (case, repinned) in [true, false].into_iter().enumerate() {
        let channel = 84480001 + case as u64;
        let path = runtime.path().join(format!("runtime/renumber-{case}.json"));
        let f = Fixture::at(path, channel, true);
        let own_end = supply_rows(&f, false, true);
        let s = f.ticket.source.as_ref().unwrap();
        let bytes = std::fs::read(&s.transcript).unwrap();
        let prefix = hex::encode(sha2::Sha256::digest(&bytes[..own_end as usize]));
        let shared = b2b_shared(Some(pool.clone()));
        let (key, actor) = b2b_active(&f, &shared).await;
        b2b_row(&f);
        let root = runtime.path().join(format!("renumber-store-{case}"));
        let stored = s.file[0] ^ STORED;
        let mut capture = SupplyCapture::resume_at_dev(&f, &root, true, (own_end, prefix), stored);
        supply_register(&f, &shared, key, Some(actor)).await;
        if repinned {
            let id = |dev| SourceId {
                session_id: s.session_id.clone(),
                path: s.transcript.clone(),
                dev,
                ino: s.file[1],
            };
            let event = repin(channel, &id(stored), &id(s.file[0]));
            capture.bindings.events.lock().unwrap().push(event);
        }
        capture.poll(&shared).await;
        let settled = f.history.native_settlement("native-fixture", &f.ticket);
        let outcome = settled.unwrap().outcome.map(|o| o.disposition);
        assert_eq!(outcome, repinned.then_some(Disposition::Executed), "{case}");
        let row = crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            channel,
        );
        assert_eq!(row.is_none(), repinned, "{case}");
    }
    pool.close().await;
    db.drop().await;
}
