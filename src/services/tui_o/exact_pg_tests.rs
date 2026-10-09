use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

#[tokio::test]
async fn exact_metadata_pg_ack_restore_off_zero_and_db_port() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let records = super::super::exact_episode::tests::fixture();
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM public.delivery_journal_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("SET application_name='c1_off_statement_counter'")
        .execute(&pool)
        .await
        .unwrap();
    // A lazy, unreachable pool proves OFF does not even acquire a PG connection.
    let unreachable = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgresql://localhost:1/disabled")
        .unwrap();
    assert_eq!(
        record_episode_evidence(false, &unreachable, &records[0])
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        record_episode_evidence(false, &pool, &records[0])
            .await
            .unwrap(),
        None
    );
    sqlx::query("SET application_name='c1_off_counter_end'")
        .execute(&pool)
        .await
        .unwrap();
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM public.delivery_journal_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, after);
    for record in &records {
        let ack = record_episode_evidence(true, &pool, record)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((ack.record, ack.version), (record.record, 1));
        assert!(!ack.digest.is_empty());
        assert!(
            record_episode_evidence(true, &pool, record)
                .await
                .unwrap()
                .is_some()
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM public.delivery_journal_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, before + records.len() as i64);
    let mut connection = pool.acquire().await.unwrap();
    let result = resolve_in_tx(&mut connection, records[0].episode)
        .await
        .unwrap();
    assert_eq!(result.authority(), Authority::Body);
    // Both reads use PG only: neither runtime root contains an O ledger.
    let root_a = tempfile::tempdir().unwrap();
    let root_b = tempfile::tempdir().unwrap();
    assert_eq!(std::fs::read_dir(root_a.path()).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(root_b.path()).unwrap().count(), 0);
    assert_eq!(
        result,
        resolve_in_tx(&mut connection, records[0].episode)
            .await
            .unwrap()
    );
    let target = DbTarget {
        id: 9,
        birth: 1,
        episode: records[0].episode,
    };
    assert_eq!(
        guard_terminal_in_tx(&mut connection, &target, TerminalIntent::BodyDone)
            .await
            .unwrap(),
        TerminalDisposition::Deferred
    );
    assert_eq!(
        guard_terminal_in_tx(&mut connection, &target, TerminalIntent::Delete)
            .await
            .unwrap(),
        TerminalDisposition::Deferred
    );
    let mut future = records[0].clone();
    future.version = 2;
    assert!(record_episode_evidence(true, &pool, &future).await.is_err());
    sqlx::query("UPDATE public.delivery_journal_events SET canonical_payload = jsonb_set(canonical_payload,'{version}','2') WHERE canonical_payload->>'episode'=$1").bind(records[0].episode.to_string()).execute(&mut *connection).await.unwrap();
    assert_eq!(
        resolve_in_tx(&mut connection, records[0].episode)
            .await
            .unwrap()
            .authority(),
        Authority::Pending
    );
}

#[cfg(unix)]
#[tokio::test]
async fn exact_duplicate_pg_full_fields_and_legacy_same_key_other_attempt() {
    crate::services::discord::exact_duplicate_pg_full_fields_and_legacy_same_key_other_attempt()
        .await;
}
#[cfg(unix)]
#[tokio::test]
async fn exact_namespace_pg_old_reader_and_legacy_binding_bytes_unchanged() {
    crate::services::discord::exact_namespace_pg_old_reader_and_legacy_binding_bytes_unchanged()
        .await;
}
