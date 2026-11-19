use super::super::cancel_tombstones::tests::StorePgFixture;
use super::*;

const MIGRATION: &str =
    include_str!("../../../migrations/postgres/0148_provider_session_system_prompts.sql");

fn row(session: usize) -> SessionPrompt {
    SessionPrompt {
        provider: "claude".into(),
        channel_id: "42".into(),
        provider_session_id: format!("session-{session}"),
        source_turn_id: format!("turn-{session}"),
        started_at: DateTime::from_timestamp(1_700_000_000 + session as i64, 0).unwrap(),
        system_prompt: format!("verbatim prompt {session}\n[ANCHOR MEMORY]\n- item"),
        anchor_count: 1,
    }
}

#[tokio::test]
async fn system_prompt_sixth_session_retains_five_and_original_is_immutable_pg() {
    let fixture = StorePgFixture::connect_test_pool(MIGRATION).await;
    let pool = &fixture.pool;
    assert!(
        fetch(pool, "claude", "42", "missing")
            .await
            .unwrap()
            .is_none()
    );
    for index in 0..6 {
        save(pool, &row(index)).await.unwrap();
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_session_system_prompts WHERE provider='claude' AND channel_id='42'").fetch_one(pool).await.unwrap();
    assert_eq!(count, 5);
    assert!(
        fetch(pool, "claude", "42", "session-0")
            .await
            .unwrap()
            .is_none()
    );
    let original = fetch(pool, "claude", "42", "session-5")
        .await
        .unwrap()
        .unwrap();
    let mut duplicate = row(5);
    duplicate.system_prompt = "resumed changed settings".into();
    save(pool, &duplicate).await.unwrap();
    assert_eq!(
        fetch(pool, "claude", "42", "session-5")
            .await
            .unwrap()
            .unwrap()
            .system_prompt,
        original.system_prompt
    );
    assert!(
        fetch(pool, "codex", "42", "session-5")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fetch(pool, "claude", "43", "session-5")
            .await
            .unwrap()
            .is_none()
    );
    let mut backward = row(6);
    backward.started_at = row(0).started_at - chrono::Duration::days(1);
    save(pool, &backward).await.unwrap();
    assert!(
        fetch(pool, "claude", "42", "session-6")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        fetch_latest_session_id(pool, "claude", "42")
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
    let tasks = (0..9).map(|index| {
        let pool = pool.clone();
        tokio::spawn(async move { save(&pool, &row(index)).await.unwrap() })
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
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind("session-prompt:claude:42")
        .execute(&mut *held)
        .await
        .unwrap();

    let mut other_channel = row(1);
    other_channel.channel_id = "43".into();
    save(pool, &other_channel).await.unwrap();
    let blocked = save(pool, &row(0)).await.unwrap_err();
    assert_eq!(
        blocked
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("55P03")
    );
    assert!(
        fetch(pool, "claude", "42", "session-0")
            .await
            .unwrap()
            .is_none()
    );

    held.commit().await.unwrap();
    save(pool, &row(0)).await.unwrap();
    assert!(
        fetch(pool, "claude", "42", "session-0")
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
    for index in 0..5 {
        save(pool, &row(index)).await.unwrap();
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

    let failure = save(pool, &row(5)).await.unwrap_err();
    assert_eq!(
        failure
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("P0001")
    );
    assert!(
        fetch(pool, "claude", "42", "session-5")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fetch(pool, "claude", "42", "session-0")
            .await
            .unwrap()
            .is_some()
    );
    fixture.close().await;
}
