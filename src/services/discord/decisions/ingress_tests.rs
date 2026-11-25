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

/// A forum registered before its first post stays guarded after a full read that lacks it, and
/// then while PG is away.
#[tokio::test(flavor = "current_thread")]
async fn decision_forum_registered_before_its_post_outlives_later_reads_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (forums, config) = (Forums::default(), DecisionsConfig::default());
    assert!(!forums.contains(Some(&pool), &config, 8_211_005).await);
    forums.register(8_211_005);
    age(&forums);
    // Another parent makes the guard read the whole set again, a set without the registered one.
    assert!(!forums.contains(Some(&pool), &config, 8_211_007).await);
    pool.close().await;
    age(&forums);
    assert!(forums.contains(Some(&pool), &config, 8_211_005).await);
    db.drop().await;
}

/// A request journaled to `forum`, as a publisher that has the forum configured leaves it: no
/// forum row, only the journal.
async fn journal_into(pool: &PgPool, forum: u64) {
    use crate::db::decisions::publication::{Journal, journal};
    let body = serde_json::json!({"agent_id": "a", "target": {"channel_id": "1"}, "ref": "r",
        "title": "t", "options": [{"label": "x"}, {"label": "y"}]});
    let ask: super::super::Ask = serde_json::from_value(body).unwrap();
    let (id, _) = super::super::store(pool, &ask, (1, 1), "c").await.unwrap();
    let entry = Journal {
        provider: "claude",
        bot_user_id: 1,
        forum_id: forum,
        handles: &[],
    };
    assert!(journal(pool, id, &entry).await.unwrap());
}

/// A forum only the publisher has configured is guarded by every other receiver once a request is
/// journaled to it: on a fresh read's miss, after the reuse window, and on a first read.
#[tokio::test(flavor = "current_thread")]
async fn decision_forum_journaled_under_another_nodes_override_is_guarded_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let receiver = DecisionsConfig::default();
    let (missed, stale) = (Forums::default(), Forums::default());
    for forums in [&missed, &stale] {
        assert!(!forums.contains(Some(&pool), &receiver, 8_211_006).await);
    }
    journal_into(&pool, 8_211_006).await;
    assert!(missed.contains(Some(&pool), &receiver, 8_211_006).await);
    age(&stale);
    assert!(stale.contains(Some(&pool), &receiver, 8_211_006).await);
    let first = Forums::default();
    assert!(first.contains(Some(&pool), &receiver, 8_211_006).await);
    pool.close().await;
    db.drop().await;
}
