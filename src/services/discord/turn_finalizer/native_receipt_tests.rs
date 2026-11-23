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

/// A native completion whose receipt could not commit keeps its row marked through a restart:
/// boot proceeds while PG is still down, and the next boot settles the receipt and the row.
#[tokio::test]
async fn native_receipt_debt_survives_restart_without_holding_boot_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::discord::inflight;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let root = runtime.path().join("runtime");
    let f = Fixture::at(root.join("injected_frames.json"), 8289002, true);
    let live = b2b_shared(Some(pool.clone()));
    let (key, actor) = b2b_active(&f, &live).await;
    let mut row = b2b_row(&f);
    row.input_origin_ids = vec!["dlg:8289:restart".into()];
    inflight::save_inflight_state(&row).unwrap();
    let outage = "CREATE OR REPLACE FUNCTION i8289_down() RETURNS trigger LANGUAGE plpgsql AS
        $$ BEGIN RAISE EXCEPTION 'pg down'; END $$;
        CREATE TRIGGER i8289_down BEFORE INSERT OR UPDATE ON turns
        FOR EACH ROW EXECUTE FUNCTION i8289_down();";
    sqlx::raw_sql(outage).execute(&pool).await.unwrap();
    let prepared = NativePrepared::capture(
        &f.history,
        "native-fixture",
        f.ticket.clone(),
        key,
        Some(actor),
        &live,
    )
    .await
    .unwrap();
    prepared.any_key(&f.history).unwrap();
    let (batch, target) = (b2b_batch(&f), &f.ticket.source.as_ref().unwrap().target);
    let cleanup = prepared
        .admit(&batch, &batch, 0, target, &f.history, &live)
        .await
        .unwrap();
    assert!(!b2b_submit(&live, cleanup, &f.history).await.unwrap());
    inflight::mark_restart_mode_blocking(
        ProviderKind::Claude,
        crate::services::discord::InflightRestartMode::DrainRestart,
    )
    .await;
    let on_disk = || inflight::load_inflight_state_read_only(&ProviderKind::Claude, row.channel_id);
    assert!(on_disk().is_some_and(|row| row.terminal_receipt.is_some()));
    for (generation, down) in [(2, true), (3, false)] {
        if !down {
            sqlx::raw_sql("DROP TRIGGER i8289_down ON turns")
                .execute(&pool)
                .await
                .unwrap();
        }
        let mut shared = b2b_shared(Some(pool.clone()));
        Arc::get_mut(&mut shared)
            .unwrap()
            .restart
            .current_generation = generation;
        let plan =
            super::super::native_prepare::BootInputs::prepare_off_worker(root.clone(), generation)
                .await
                .unwrap();
        let reaped = inflight::reap_inflight_rows_at_boot_blocking(
            &ProviderKind::Claude,
            Some(pool.clone()),
        )
        .await;
        assert!(reaped.native_preparation_complete());
        plan.restore(&shared).await.unwrap();
        assert_eq!(on_disk().is_some(), down, "boot {generation}");
    }
    let receipt: (Vec<String>, Option<String>) = sqlx::query_as(
        "SELECT consumed_origin_ids, terminal_outcome FROM turns WHERE turn_id = $1",
    )
    .bind(format!("discord:{}:{}", key.channel_id, key.user_msg_id))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        receipt,
        (vec!["dlg:8289:restart".into()], Some("completed".into()))
    );
    assert!(
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
            .cleanup_complete
    );
    pool.close().await;
    db.drop().await;
}

/// A native result that lands after shutdown marked its row DrainRestart: once the receipt
/// commits (live, or at boot after PG recovers), the next generation boots and removes the row.
#[tokio::test]
async fn native_result_after_restart_mark_settles_at_next_boot_pg() {
    use crate::services::discord::{inflight, runtime_store::set_process_generation_for_tests};
    for down in [false, true] {
        let runtime = crate::config::TestRuntimeRootGuard::new();
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        set_process_generation_for_tests(Some(1));
        let root = runtime.path().join("runtime");
        let f = Fixture::at(root.join("injected_frames.json"), 8289003, true);
        let live = b2b_shared(Some(pool.clone()));
        let (key, actor) = b2b_active(&f, &live).await;
        let mut row = b2b_row(&f);
        row.input_origin_ids = vec!["dlg:8289:marked".into()];
        inflight::save_inflight_state(&row).unwrap();
        let prepared = NativePrepared::capture(
            &f.history,
            "native-fixture",
            f.ticket.clone(),
            key,
            Some(actor),
            &live,
        )
        .await
        .unwrap();
        prepared.any_key(&f.history).unwrap();
        let (batch, target) = (b2b_batch(&f), &f.ticket.source.as_ref().unwrap().target);
        let cleanup = prepared
            .admit(&batch, &batch, 0, target, &f.history, &live)
            .await
            .unwrap();
        inflight::mark_restart_mode_blocking(
            ProviderKind::Claude,
            crate::services::discord::InflightRestartMode::DrainRestart,
        )
        .await;
        let on_disk =
            || inflight::load_inflight_state_read_only(&ProviderKind::Claude, row.channel_id);
        assert!(on_disk().is_some_and(|row| row.restart_mode.is_some()));
        let outage = "CREATE FUNCTION i8289_down() RETURNS trigger LANGUAGE plpgsql AS
            $$ BEGIN RAISE EXCEPTION 'pg down'; END $$;
            CREATE TRIGGER i8289_down BEFORE INSERT OR UPDATE ON turns
            FOR EACH ROW EXECUTE FUNCTION i8289_down();";
        if down {
            sqlx::raw_sql(outage).execute(&pool).await.unwrap();
        }
        assert!(!b2b_submit(&live, cleanup, &f.history).await.unwrap());
        assert!(on_disk().is_some_and(|row| row.terminal_receipt.is_some() == down));
        if down {
            sqlx::raw_sql("DROP TRIGGER i8289_down ON turns")
                .execute(&pool)
                .await
                .unwrap();
        }
        set_process_generation_for_tests(Some(2));
        let mut shared = b2b_shared(Some(pool.clone()));
        Arc::get_mut(&mut shared)
            .unwrap()
            .restart
            .current_generation = 2;
        let plan = super::super::native_prepare::BootInputs::prepare_off_worker(root.clone(), 2)
            .await
            .unwrap();
        let reaped = inflight::reap_inflight_rows_at_boot_blocking(
            &ProviderKind::Claude,
            Some(pool.clone()),
        )
        .await;
        assert!(reaped.native_preparation_complete());
        let booted = plan.restore(&shared).await;
        set_process_generation_for_tests(None);
        booted.unwrap();
        assert!(on_disk().is_none(), "down={down}");
        let receipts: Vec<(Vec<String>, Option<String>)> = sqlx::query_as(
            "SELECT consumed_origin_ids, terminal_outcome FROM turns WHERE turn_id = $1",
        )
        .bind(format!("discord:{}:{}", key.channel_id, key.user_msg_id))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            receipts,
            vec![(vec!["dlg:8289:marked".into()], Some("completed".into()))]
        );
        assert!(
            f.history
                .native_settlement("native-fixture", &f.ticket)
                .unwrap()
                .cleanup_complete
        );
        pool.close().await;
        db.drop().await;
    }
}

/// A prior generation's DrainRestart marker still holds a row whose terminal is not committed.
#[test]
fn restart_marked_row_without_committed_receipt_keeps_its_marker() {
    use crate::services::discord::{inflight, runtime_store::set_process_generation_for_tests};
    let runtime = crate::config::TestRuntimeRootGuard::new();
    set_process_generation_for_tests(Some(1));
    let f = Fixture::at(
        runtime.path().join("runtime/injected_frames.json"),
        8289004,
        true,
    );
    let mut row = b2b_row(&f);
    row.set_restart_mode(crate::services::discord::InflightRestartMode::DrainRestart);
    inflight::save_inflight_state(&row).unwrap();
    set_process_generation_for_tests(Some(2));
    let row =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, row.channel_id).unwrap();
    let (outcome, _) = inflight::GuardedClearOutcome::for_native_snapshot_with_detail(
        &ProviderKind::Claude,
        &row,
        false,
    );
    set_process_generation_for_tests(None);
    assert_eq!(
        outcome,
        inflight::GuardedClearOutcome::PlannedRestartSkipped
    );
}
