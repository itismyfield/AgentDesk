use super::*;

/// A native completion commits its row's terminal with the origin it consumed, kept through a
/// rewrite of the row, before the cleanup removes the row.
#[tokio::test]
async fn native_completion_commits_the_row_origin_before_removing_it_pg() {
    use crate::services::discord::inflight;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    std::fs::create_dir_all(runtime.path().join("runtime")).unwrap();
    let dir = tempfile::tempdir_in(runtime.path().join("runtime")).unwrap();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let f = Fixture::at(dir.path().join("frames.json"), 8289001, true);
    let shared = b2b_shared(Some(pool.clone()));
    let (key, actor) = b2b_active(&f, &shared).await;
    let mut row = b2b_row(&f);
    row.input_origin_ids = vec!["dlg:8289:native".into()];
    inflight::save_inflight_state(&row).unwrap();
    let mut row = inflight::load_inflight_state(&ProviderKind::Claude, row.channel_id).unwrap();
    row.full_response = "streamed".into();
    inflight::save_inflight_state(&row).unwrap();
    let prepared = NativePrepared::capture(
        &f.history,
        "native-fixture",
        f.ticket.clone(),
        key,
        Some(actor),
        &shared,
    )
    .await
    .unwrap();
    prepared.any_key(&f.history).unwrap();
    let (batch, target) = (b2b_batch(&f), &f.ticket.source.as_ref().unwrap().target);
    let cleanup = prepared
        .admit(&batch, &batch, 0, target, &f.history, &shared)
        .await
        .unwrap();
    assert!(b2b_submit(&shared, cleanup, &f.history).await.unwrap());
    assert!(inflight::load_inflight_state(&ProviderKind::Claude, row.channel_id).is_none());
    let receipt: (Vec<String>, Option<String>) = sqlx::query_as(
        "SELECT consumed_origin_ids, terminal_outcome FROM turns WHERE turn_id = $1",
    )
    .bind(format!("discord:{}:{}", key.channel_id, key.user_msg_id))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        receipt,
        (vec!["dlg:8289:native".into()], Some("completed".into()))
    );
    pool.close().await;
    db.drop().await;
}
