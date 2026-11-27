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
                hook_event: "user_prompt_submit".into(),
                received_at: chrono::Utc::now(),
                reclaims: false,
            },
        },
    }
}

#[tokio::test]
async fn hook_refresh_applies_a_renumber_and_settles_the_native_turn_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use sha2::Digest;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 723_841;
    {
        let path = runtime.path().join("runtime/renumber.json");
        let f = Fixture::at(path, channel, true);
        let own_end = supply_rows(&f, false, true);
        let s = f.ticket.source.as_ref().unwrap();
        let bytes = std::fs::read(&s.transcript).unwrap();
        let prefix = hex::encode(sha2::Sha256::digest(&bytes[..own_end as usize]));
        let shared = b2b_shared(Some(pool.clone()));
        let (key, actor) = b2b_active(&f, &shared).await;
        b2b_row(&f);
        let root = runtime.path().join("renumber-store");
        let stored = s.file[0] ^ STORED;
        let mut capture = SupplyCapture::resume_at_dev(&f, &root, true, (own_end, prefix), stored);
        supply_register(&f, &shared, key, Some(actor)).await;
        capture.poll(&shared).await;
        assert!(
            f.history
                .native_settlement("native-fixture", &f.ticket)
                .unwrap()
                .outcome
                .is_none()
        );
        let id = |dev| SourceId {
            session_id: s.session_id.clone(),
            path: s.transcript.clone(),
            dev,
            ino: s.file[1],
        };
        let old_cursor = capture.writer.store().cursor(&id(stored)).unwrap().clone();
        let owed = capture.owed.len();
        let event = repin(channel, &id(stored), &id(s.file[0]));
        capture.bindings.events.lock().unwrap().push(event);
        capture.bindings.notice.send_replace(2);
        capture.poll(&shared).await;
        assert_eq!(
            capture.writer.store().binding_checkpoint().unwrap(),
            Some(2)
        );
        let settled = f.history.native_settlement("native-fixture", &f.ticket);
        let settled = settled.unwrap();
        assert_eq!(
            settled.outcome.map(|o| o.disposition),
            Some(Disposition::Executed)
        );
        assert!(settled.cleanup_complete);
        let row = crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            channel,
        );
        assert!(row.is_none());
        let mut cursors = capture.writer.store().cursors();
        let cursor = cursors.next().unwrap();
        assert!(cursors.next().is_none());
        assert_eq!(cursor.source, id(s.file[0]));
        assert_eq!(cursor.captured_through, old_cursor.captured_through);
        assert_eq!(cursor.prefix_hash, old_cursor.prefix_hash);
        assert_eq!(capture.owed.len(), owed);
    }
    pool.close().await;
    db.drop().await;
}
