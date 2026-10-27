use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

#[tokio::test]
async fn channel_home_commit_reset_and_retry_without_owner_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = OwnerIdentity::new(" Claude ", " 7001 ");
    assert_eq!(read(&pool, &key).await.unwrap(), None);
    let operation = Uuid::new_v4();
    let committed = write(&pool, &key, 0, operation, Some("desk-A"))
        .await
        .unwrap();
    let WriteOutcome::Applied(first) = committed else {
        panic!("first write did not commit")
    };
    assert_eq!(
        (
            first.mode.as_str(),
            first.home_instance_id.as_deref(),
            first.revision
        ),
        ("override", Some("desk-A"), 1)
    );
    assert_eq!(
        write(&pool, &key, 0, operation, Some("desk-A"))
            .await
            .unwrap(),
        WriteOutcome::Reused(first.clone())
    );
    assert!(matches!(
        write(&pool, &key, 0, operation, Some("desk-B")).await,
        Err(WriteError::OperationMismatch)
    ));
    let second_reader = db.connect_and_migrate().await;
    assert_eq!(read(&second_reader, &key).await.unwrap(), Some(first));
    let reset_op = Uuid::new_v4();
    let WriteOutcome::Applied(reset) = write(&pool, &key, 1, reset_op, None).await.unwrap() else {
        panic!("reset did not commit")
    };
    assert_eq!(
        (
            reset.mode.as_str(),
            reset.home_instance_id.as_deref(),
            reset.revision
        ),
        ("inherit", None, 2)
    );
    assert_eq!(
        write(&pool, &key, 1, reset_op, None).await.unwrap(),
        WriteOutcome::Reused(reset.clone())
    );
    assert_eq!(
        write(&pool, &key, 0, operation, Some("desk-A"))
            .await
            .unwrap(),
        WriteOutcome::Conflict(Some(reset.clone()))
    );
    assert_eq!(read(&second_reader, &key).await.unwrap(), Some(reset));
    assert_eq!(
        read(&pool, &OwnerIdentity::new("codex", "7001"))
            .await
            .unwrap(),
        None
    );
    let owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM intake_session_owners WHERE raw_channel_id='7001'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(owners, 0);
    second_reader.close().await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn channel_home_concurrent_revision_cas_has_one_winner_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = OwnerIdentity::new("codex", "7002");
    write(&pool, &key, 0, Uuid::new_v4(), Some("desk-A"))
        .await
        .unwrap();
    let (left, right) = tokio::join!(
        write(&pool, &key, 1, Uuid::new_v4(), Some("desk-B")),
        write(&pool, &key, 1, Uuid::new_v4(), None),
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|v| matches!(v, WriteOutcome::Applied(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|v| matches!(v, WriteOutcome::Conflict(_)))
            .count(),
        1
    );
    assert_eq!(read(&pool, &key).await.unwrap().unwrap().revision, 2);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn channel_home_rejects_invalid_settings_and_surfaces_pg_failure_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = OwnerIdentity::new("claude", "7003");
    for (revision, home) in [
        (-1, None),
        (i64::MAX, None),
        (0, Some(" ")),
        (0, Some("a/b")),
    ] {
        assert!(matches!(
            write(&pool, &key, revision, Uuid::new_v4(), home).await,
            Err(WriteError::Invalid(_))
        ));
    }
    for (mode, home, revision) in [
        ("inherit", Some("A"), 1),
        ("override", None, 1),
        ("migration_pending", None, 1),
        ("inherit", None, 0),
    ] {
        assert!(sqlx::query("INSERT INTO channel_home_preferences (provider,raw_channel_id,mode,home_instance_id,revision,operation_id) VALUES ('claude','7003',$1,$2,$3,$4)")
            .bind(mode).bind(home).bind(revision).bind(Uuid::new_v4()).execute(&pool).await.is_err());
    }
    assert_eq!(read(&pool, &key).await.unwrap(), None);
    pool.close().await;
    assert!(read(&pool, &key).await.is_err());
    assert!(matches!(
        write(&pool, &key, 0, Uuid::new_v4(), None).await,
        Err(WriteError::Storage(_))
    ));
    db.drop().await;
}
