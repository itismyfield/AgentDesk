use super::*;
use crate::services::tui_o::writer::pieces::Derived;
use crate::services::tui_o::writer::reply_anchor::ReplyRef;

const ANCHOR: u64 = 4242;

/// A re-post keeps its original's reply in the new serial's `ReplyTo` and its one request's
/// reference, with payload, nonce and marker, and a restart posts nothing more.
#[tokio::test]
async fn repost_replies_to_the_original_anchor_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = admitted(&pool, "reply-repost", "piece").await;
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let Derived::Piece(mut original) = piece("reply-repost", "piece") else {
        unreachable!("a body piece")
    };
    original.reply = Some(ReplyRef::Fixed(ANCHOR));
    assert_eq!(writer.deliver(&Derived::Piece(original)).await, Step::Done);
    writer.set_live(true);
    let mut runner = Runner::new(pool.clone(), BOT, run_scope());
    let io = Io::over(&harness.port);
    let intent = evidence(&pool, &key).await;
    let admission = runner.try_next_dispatch(&mut writer, intent);
    let Admission::Granted { permit, lease } = admission.await.unwrap() else {
        panic!("granted")
    };
    let started = dispatch::start(&mut writer, permit, lease, &*io).await;
    started.finish(&mut writer, &pool).await.unwrap();
    let ledger = writer.store().ledger();
    let (serial, _) = ledger.latest_piece(key.unit_for_test(), 0).unwrap();
    assert_eq!(
        (serial, ledger.reply_to_for_test(serial)),
        (1, Some(ANCHOR))
    );
    let bodies = io.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1);
    let reference = &bodies[0]["message_reference"];
    assert_eq!(reference["message_id"].as_str(), Some("4242"));
    assert_eq!(bodies[0]["content"].as_str(), Some("piece"));
    assert!(bodies[0]["nonce"].is_string() && bodies[0]["embeds"][0]["footer"].is_object());
    drop(writer);
    let mut restarted = harness.writer();
    let again = restarted.deliver(&piece("reply-repost", "piece")).await;
    assert_eq!(again, Step::Done);
    assert_eq!((io.wire(), harness.port.posts().len()), (1, 1));
    pool.close().await;
    db.drop().await;
}
