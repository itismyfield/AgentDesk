//! Which forums the intake guard knows, and what it answers while PG is away.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::decision_forums::{ForumState, store};

/// Makes the guard's last read old enough to be read again.
fn age(forums: &Forums) {
    let mut read = forums.0.lock().unwrap();
    let old = Instant::now().checked_sub(REFRESH * 2).unwrap();
    read.as_mut().unwrap().0 = old;
}

/// Only a ready or configured forum is guarded; one recorded later, as by another process, is
/// known at once inside the reuse window, and a failed read keeps what was known.
#[tokio::test(flavor = "current_thread")]
async fn decision_forum_guard_knows_ready_and_configured_forums_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let ready = |id: &str| ForumState::Ready(id.to_string());
    store(&pool, 1, 10, "decision", &ready("8211001"))
        .await
        .unwrap();
    store(&pool, 1, 11, "decision", &ForumState::CreateMayHaveSent)
        .await
        .unwrap();
    store(&pool, 1, 12, "decision", &ForumState::Failed("403".into()))
        .await
        .unwrap();
    let mut config = DecisionsConfig::default();
    config
        .forums_by_category
        .insert("13".into(), "8211002".into());
    let forums = Forums::default();
    let guarded = [
        (8_211_001, true),
        (8_211_002, true),
        (10, false),
        (11, false),
        (8_211_999, false),
    ];
    for (parent, expected) in guarded {
        let answer = forums.contains(Some(&pool), &config, parent).await;
        assert_eq!(answer, expected, "{parent}");
    }
    store(&pool, 1, 14, "decision", &ready("8211003"))
        .await
        .unwrap();
    assert!(forums.contains(Some(&pool), &config, 8_211_003).await);
    age(&forums);
    assert!(forums.contains(Some(&pool), &config, 8_211_003).await);
    // PG goes away: a guard that read before keeps its set, one that never read knows the
    // configured forums only.
    pool.close().await;
    age(&forums);
    assert!(forums.contains(Some(&pool), &config, 8_211_001).await);
    assert!(!forums.contains(Some(&pool), &config, 8_211_999).await);
    let unread = Forums::default();
    assert!(unread.contains(Some(&pool), &config, 8_211_002).await);
    assert!(!unread.contains(Some(&pool), &config, 8_211_001).await);
    db.drop().await;
}

/// A forum added to the configuration is guarded at once, inside the refresh window and while
/// PG is away, and the forums already read stay guarded.
#[tokio::test(flavor = "current_thread")]
async fn decision_forum_guard_takes_a_newly_configured_forum_at_once_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let ready = ForumState::Ready("8211001".into());
    store(&pool, 1, 10, "decision", &ready).await.unwrap();
    let forums = Forums::default();
    let before = DecisionsConfig::default();
    assert!(!forums.contains(Some(&pool), &before, 8_211_004).await);
    let mut after = before.clone();
    after
        .forums_by_category
        .insert("15".into(), "8211004".into());
    assert!(forums.contains(Some(&pool), &after, 8_211_004).await);
    pool.close().await;
    age(&forums);
    assert!(forums.contains(Some(&pool), &after, 8_211_004).await);
    assert!(forums.contains(Some(&pool), &after, 8_211_001).await);
    db.drop().await;
}
