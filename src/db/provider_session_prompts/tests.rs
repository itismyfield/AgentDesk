use super::super::cancel_tombstones::tests::StorePgFixture;
use super::*;

const MIGRATION: &str = concat!(
    include_str!("../../../migrations/postgres/0148_provider_session_system_prompts.sql"),
    include_str!("../../../migrations/postgres/0199_provider_session_prompt_anchors.sql")
);

fn test_channel() -> String {
    // Advisory keys share the database across fixture schemas.
    uuid::Uuid::new_v4().simple().to_string()
}

fn row(channel: &str, session: usize) -> SessionPrompt {
    SessionPrompt {
        provider: "claude".into(),
        channel_id: channel.into(),
        provider_session_id: format!("session-{session}"),
        source_turn_id: format!("turn-{session}"),
        started_at: DateTime::from_timestamp(1_700_000_000 + session as i64, 0).unwrap(),
        system_prompt: format!("verbatim prompt {session}\n[ANCHOR MEMORY]\n- item"),
        anchor_count: 1,
        anchors: None,
    }
}

#[tokio::test]
async fn system_prompt_sixth_session_retains_five_and_original_is_immutable_pg() {
    let fixture = StorePgFixture::connect_test_pool(MIGRATION).await;
    let pool = &fixture.pool;
    let channel = test_channel();
    assert!(
        fetch(pool, "claude", &channel, "missing")
            .await
            .unwrap()
            .is_none()
    );
    for index in 0..6 {
        save(pool, &row(&channel, index)).await.unwrap();
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM provider_session_system_prompts WHERE provider='claude' AND channel_id=$1",
    )
    .bind(&channel)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(count, 5);
    assert!(
        fetch(pool, "claude", &channel, "session-0")
            .await
            .unwrap()
            .is_none()
    );
    let original = fetch(pool, "claude", &channel, "session-5")
        .await
        .unwrap()
        .unwrap();
    let mut duplicate = row(&channel, 5);
    duplicate.system_prompt = "resumed changed settings".into();
    save(pool, &duplicate).await.unwrap();
    assert_eq!(
        fetch(pool, "claude", &channel, "session-5")
            .await
            .unwrap()
            .unwrap()
            .system_prompt,
        original.system_prompt
    );
    assert!(
        fetch(pool, "codex", &channel, "session-5")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fetch(pool, "claude", "other-channel", "session-5")
            .await
            .unwrap()
            .is_none()
    );
    let mut backward = row(&channel, 6);
    backward.started_at = row(&channel, 0).started_at - chrono::Duration::days(1);
    save(pool, &backward).await.unwrap();
    assert!(
        fetch(pool, "claude", &channel, "session-6")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        fetch_latest_session_id(pool, "claude", &channel)
            .await
            .unwrap()
            .as_deref(),
        Some("session-5")
    );
    fixture.close().await;
}

#[tokio::test]
async fn system_prompt_concurrent_starts_remain_bounded_pg() {
    let fixture = StorePgFixture::connect_test_pool(MIGRATION).await;
    let pool = &fixture.pool;
    let channel = test_channel();
    let tasks = (0..9).map(|index| {
        let pool = pool.clone();
        let prompt = row(&channel, index);
        tokio::spawn(async move { save(&pool, &prompt).await.unwrap() })
    });
    for task in futures::future::join_all(tasks).await {
        task.unwrap();
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_session_system_prompts")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(count, 5);
    fixture.close().await;
}

#[tokio::test]
async fn system_prompt_save_uses_the_channel_advisory_lock_pg() {
    let fixture = StorePgFixture::connect_test_pool(MIGRATION).await;
    let pool = &fixture.pool;
    let channel = test_channel();
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("session-prompt:claude:{channel}"))
        .execute(&mut *held)
        .await
        .unwrap();

    let other_channel = row(&test_channel(), 1);
    save(pool, &other_channel).await.unwrap();
    let blocked = save(pool, &row(&channel, 0)).await.unwrap_err();
    assert_eq!(
        blocked
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("55P03")
    );
    assert!(
        fetch(pool, "claude", &channel, "session-0")
            .await
            .unwrap()
            .is_none()
    );

    held.commit().await.unwrap();
    save(pool, &row(&channel, 0)).await.unwrap();
    assert!(
        fetch(pool, "claude", &channel, "session-0")
            .await
            .unwrap()
            .is_some()
    );
    fixture.close().await;
}

#[tokio::test]
async fn system_prompt_prune_failure_rolls_back_the_insert_pg() {
    let fixture = StorePgFixture::connect_test_pool(MIGRATION).await;
    let pool = &fixture.pool;
    let channel = test_channel();
    for index in 0..5 {
        save(pool, &row(&channel, index)).await.unwrap();
    }
    sqlx::raw_sql(
        "CREATE FUNCTION fail_prune() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'fixture refuses prune'; END;
         $$;
         CREATE TRIGGER fail_prune BEFORE DELETE ON provider_session_system_prompts
         FOR EACH ROW EXECUTE FUNCTION fail_prune();",
    )
    .execute(pool)
    .await
    .unwrap();

    let failure = save(pool, &row(&channel, 5)).await.unwrap_err();
    assert_eq!(
        failure
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("P0001")
    );
    assert!(
        fetch(pool, "claude", &channel, "session-5")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fetch(pool, "claude", &channel, "session-0")
            .await
            .unwrap()
            .is_some()
    );
    fixture.close().await;
}
