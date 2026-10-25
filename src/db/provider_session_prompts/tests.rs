use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

fn row(session: usize) -> SessionPrompt {
    SessionPrompt {
        provider: "claude".into(),
        channel_id: "42".into(),
        provider_session_id: format!("session-{session}"),
        source_turn_id: format!("turn-{session}"),
        started_at: Utc::now() + chrono::Duration::seconds(session as i64),
        system_prompt: format!("verbatim prompt {session}\n[ANCHOR MEMORY]\n- item"),
        anchor_count: 1,
    }
}

#[tokio::test]
async fn system_prompt_sixth_session_retains_five_and_original_is_immutable_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    assert!(
        fetch(&pool, "claude", "42", "missing")
            .await
            .unwrap()
            .is_none()
    );
    for index in 0..6 {
        save(&pool, &row(index)).await.unwrap();
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_session_system_prompts WHERE provider='claude' AND channel_id='42'").fetch_one(&pool).await.unwrap();
    assert_eq!(count, 5);
    assert!(
        fetch(&pool, "claude", "42", "session-0")
            .await
            .unwrap()
            .is_none()
    );
    let original = fetch(&pool, "claude", "42", "session-5")
        .await
        .unwrap()
        .unwrap();
    let mut duplicate = row(5);
    duplicate.system_prompt = "resumed changed settings".into();
    save(&pool, &duplicate).await.unwrap();
    assert_eq!(
        fetch(&pool, "claude", "42", "session-5")
            .await
            .unwrap()
            .unwrap()
            .system_prompt,
        original.system_prompt
    );
    assert!(
        fetch(&pool, "codex", "42", "session-5")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fetch(&pool, "claude", "43", "session-5")
            .await
            .unwrap()
            .is_none()
    );
    let mut backward = row(6);
    backward.started_at = Utc::now() - chrono::Duration::days(1);
    save(&pool, &backward).await.unwrap();
    assert!(
        fetch(&pool, "claude", "42", "session-6")
            .await
            .unwrap()
            .is_some()
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn system_prompt_concurrent_starts_remain_bounded_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let tasks = (0..9).map(|index| {
        let pool = pool.clone();
        tokio::spawn(async move { save(&pool, &row(index)).await.unwrap() })
    });
    for task in futures::future::join_all(tasks).await {
        task.unwrap();
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_session_system_prompts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 5);
    pool.close().await;
    db.drop().await;
}
