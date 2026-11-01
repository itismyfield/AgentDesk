use super::retired_durability_tests::{SyncFault, hold_path};
use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn retired_launch_requires_absent_n1_hold_durability_before_n2_create_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let n1 = record(&owner(CHANNEL), "retired-n1", HostedState::Retired);
    let raw = Some(wire(&n1));
    seed_row(&pool, raw.clone()).await;
    let path = hold_path(&n1.execution_nonce);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let fault = SyncFault::new(&path);
    fault.fail(true);
    let host = Arc::new(FakeHost::created("fresh-pane"));
    let prepared = AtomicUsize::new(0);
    let result = launch_herdr_session(
        &pool,
        launch(Some(endpoint())),
        |incarnation| {
            prepared.fetch_add(1, Ordering::SeqCst);
            command(incarnation)
        },
        host.clone(),
    )
    .await;
    assert_eq!(
        host.creates.load(Ordering::SeqCst),
        0,
        "N2 create must wait for durable N1 hold absence: {result:?}"
    );
    assert!(
        matches!(&result, Err(HerdrLaunchError::RetiredHold(_))),
        "durability failure must keep its RetiredHold classification: {result:?}"
    );
    assert_eq!(prepared.load(Ordering::SeqCst), 0);
    assert_eq!(stored(&pool).await, raw);
    assert_eq!(
        fault.calls(),
        1,
        "the production launch must reach the fsync seam"
    );
    fault.fail(false);
    let result = launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone())
        .await
        .unwrap();
    let HerdrLaunchOutcome::Launched {
        execution_nonce,
        evidence: Ok(()),
        ..
    } = result
    else {
        panic!("{result:?}")
    };
    assert_ne!(execution_nonce, n1.execution_nonce);
    assert_eq!(fault.calls(), 2);
    assert_eq!(host.creates.load(Ordering::SeqCst), 1);
    let HostedRecord::Known(n2) = decoded(stored(&pool).await) else {
        panic!("N2 missing")
    };
    assert_eq!(n2.state, HostedState::Pending);
    assert_eq!(n2.execution_nonce, execution_nonce);
    assert_eq!(n2.location.unwrap().pane_id, "fresh-pane");
    assert!(matches!(
        launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone()).await,
        Err(HerdrLaunchError::Occupied(Some(HostedState::Pending)))
    ));
    assert_eq!(host.creates.load(Ordering::SeqCst), 1);
    assert_eq!(fault.calls(), 2);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn retired_launch_normal_fresh_once_and_live_rows_untouched_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for state in [
        HostedState::Bound,
        HostedState::Pending,
        HostedState::Retired,
    ] {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let n1 = record(&owner(CHANNEL), "normal-n1", state);
        let raw = Some(wire(&n1));
        seed_row(&pool, raw.clone()).await;
        let path = hold_path(&n1.execution_nonce);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "hold unchanged for live rows").unwrap();
        let fault = SyncFault::new(&path);
        let host = Arc::new(FakeHost::created("fresh-pane"));
        let result =
            launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone()).await;
        if state == HostedState::Retired {
            assert!(
                matches!(
                    result,
                    Ok(HerdrLaunchOutcome::Launched {
                        evidence: Ok(()),
                        ..
                    })
                ),
                "{result:?}"
            );
            assert_eq!(host.creates.load(Ordering::SeqCst), 1);
            assert_eq!(fault.calls(), 1);
            assert!(!path.exists());
        } else {
            assert_eq!(result, Err(HerdrLaunchError::Occupied(Some(state))));
            assert_eq!(host.creates.load(Ordering::SeqCst), 0);
            assert_eq!(stored(&pool).await, raw);
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "hold unchanged for live rows"
            );
            assert_eq!(fault.calls(), 0);
        }
        pool.close().await;
        db.drop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn retired_launch_owner_mismatch_keeps_n1_hold_and_row_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut other = owner(CHANNEL);
    other.runtime_root = "/another-runtime".into();
    let n1 = record(&other, "other-owner-n1", HostedState::Retired);
    let raw = Some(wire(&n1));
    seed_row(&pool, raw.clone()).await;
    let path = hold_path(&n1.execution_nonce);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "other owner hold").unwrap();
    let fault = SyncFault::new(&path);
    let host = Arc::new(FakeHost::created("fresh-pane"));
    let result = launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone()).await;
    assert_eq!(
        result,
        Err(HerdrLaunchError::RetiredHold(
            "retired owner differs: runtime_root".into()
        ))
    );
    assert_eq!(host.creates.load(Ordering::SeqCst), 0);
    assert_eq!(stored(&pool).await, raw);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "other owner hold");
    assert_eq!(fault.calls(), 0);
    pool.close().await;
    db.drop().await;
}

/// A successor launched before a Claude takeover's confirmation read the retired row keeps that
/// end, so the takeover still confirms once the row names only the successor.
#[tokio::test(flavor = "multi_thread")]
async fn retired_launch_keeps_a_claude_takeovers_end_before_its_successor_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().unwrap();
    let channel: u64 = CHANNEL.parse().unwrap();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    crate::services::discord::seed_claude_takeover(&root, channel, "takeover-n1");
    let n1 = record(&owner(CHANNEL), "takeover-n1", HostedState::Retired);
    seed_row(&pool, Some(wire(&n1))).await;
    let host = Arc::new(FakeHost::created("fresh-pane"));
    let result = launch_herdr_session(&pool, launch(Some(endpoint())), command, host).await;
    assert!(
        matches!(result, Ok(HerdrLaunchOutcome::Launched { .. })),
        "{result:?}"
    );
    let HostedRecord::Known(n2) = decoded(stored(&pool).await) else {
        panic!("the successor is missing")
    };
    assert_ne!(n2.execution_nonce, n1.execution_nonce);
    assert!(
        crate::services::discord::claude_takeover_confirmed_now(channel),
        "the end kept before the overwrite confirms the takeover"
    );
    drop(crate::services::tui_o::turn_mode::TestConfirmation::confirmed(channel));
    pool.close().await;
    db.drop().await;
}
