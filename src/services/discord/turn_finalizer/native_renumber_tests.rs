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
            cause: BindingCause::SameSessionVerified,
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

#[tokio::test]
async fn hook_refresh_settles_the_original_keyed_ticket_after_boot_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::tui_o::shadow::{binding_reader::source_id_for, capture::renumber};
    use sha2::Digest;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let root = runtime.path().join("runtime");
    let f = Fixture::at(root.join("injected_frames.json"), 723_842, true);
    let source = f.ticket.source.as_ref().unwrap();
    supply_rows(&f, false, true);
    f.history
        .note_native_any_key("native-fixture", &f.ticket)
        .unwrap();
    b2b_row(&f);
    let peer = Fixture::at(root.join("peer.json"), 723_843, true);
    let peer_row = b2b_row(&peer);
    let bytes = std::fs::read(&source.transcript).unwrap();
    let prefix = hex::encode(sha2::Sha256::digest(&bytes));
    let store = runtime.path().join("old-ticket-store");
    let mut previous =
        SupplyCapture::resume_at(&f, &store, true, bytes.len() as u64, prefix.clone());
    let old = source_id_for(&source.session_id, &source.transcript).unwrap();
    assert_eq!([old.dev, old.ino], source.file);
    assert_eq!(
        previous.writer.store().cursor(&old).unwrap().prefix_hash,
        prefix
    );
    drop(previous);

    let shifted = renumber::shift(&source.transcript, STORED);
    let current = source_id_for(&source.session_id, &source.transcript).unwrap();
    assert_ne!(old.dev, current.dev);
    let mut shared = b2b_shared(Some(pool.clone()));
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 2;
    super::super::native_prepare::BootInputs::prepare(&root, 2)
        .unwrap()
        .restore(&shared)
        .await
        .unwrap();
    let mut capture =
        SupplyCapture::resume_at(&f, &store, true, bytes.len() as u64, prefix.clone());
    let event = repin(f.ticket.channel_id, &old, &current);
    capture.bindings.events.lock().unwrap().push(event);
    capture.bindings.notice.send_replace(2);
    let owed = capture.owed.len();
    capture.poll(&shared).await;
    let saved = || {
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
    };
    let first = saved();
    assert_eq!(first.ticket, f.ticket);
    assert_eq!(
        first.outcome.as_ref().map(|outcome| &outcome.disposition),
        Some(&Disposition::Executed)
    );
    assert!(first.cleanup_complete);
    capture.poll(&shared).await;
    assert_eq!(saved().outcome, first.outcome);
    assert_eq!(capture.owed.len(), owed);
    assert_eq!(
        capture.writer.store().cursor(&current).unwrap().prefix_hash,
        prefix
    );
    assert!(capture.writer.store().cursor(&old).is_none());
    let load = |channel| {
        crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            channel,
        )
    };
    assert!(load(f.ticket.channel_id).is_none());
    assert_eq!(
        load(peer.ticket.channel_id).unwrap().turn_nonce,
        peer_row.turn_nonce
    );
    drop(shifted);
    pool.close().await;
    db.drop().await;
}
