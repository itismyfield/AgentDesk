use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::session_host::herdr_termination_rig as herdr;

const CHANNEL: u64 = 8_128_303;

fn rig(name: &str, alive: bool) -> Option<(herdr::P11TerminationRig, tokio::runtime::Runtime)> {
    if !herdr::in_child(&format!("{}::{name}", module_path!()), CHANNEL) {
        return None;
    }
    let rig = herdr::P11TerminationRig::boot_with(CHANNEL, alive);
    rig.thread_installer()();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(rig.thread_installer())
        .build()
        .unwrap();
    Some((rig, runtime))
}

async fn seeded(
    rig: &herdr::P11TerminationRig,
    state: HostedState,
) -> (
    TestPostgresDb,
    PgPool,
    Arc<SharedData>,
    String,
    herdr::LiveSwitches,
) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (shared, _) =
        crate::services::discord::host_teardown_gate::test_support::runtime(&pool).await;
    let key = format!(
        "claude/{}/{}:AgentDesk-claude-restart-recovery",
        shared.token_hash,
        herdr::NODE
    );
    let switches = rig.bind(&pool, &shared.token_hash, &key, state).await;
    (db, pool, shared, key, switches)
}

async fn recover(
    pool: &PgPool,
    shared: &Arc<SharedData>,
    key: &str,
) -> Result<Option<RetiredExclusion>, TerminationResult> {
    let row = load(pool, key).await.unwrap();
    let HostedRecord::Known(record) = &row.record else {
        panic!("known record")
    };
    let expected = expected_for_restart(shared, record, None).await.unwrap();
    recover_ended(
        OperatorTerminate {
            session_key: key.into(),
            execution_nonce: record.execution_nonce.clone(),
        },
        expected,
        shared.clone(),
        pool,
    )
    .await
}

fn ended(state: HostedState, name: &str) {
    let Some((rig, runtime)) = rig(name, false) else {
        return;
    };
    runtime.block_on(async {
        let _root = TestRuntimeRootGuard::new();
        let (db, pool, shared, key, _switches) = seeded(&rig, state).await;
        rig.restore_shell();
        let exclusion = recover(&pool, &shared, &key)
            .await
            .unwrap()
            .expect("ended predecessor must recover");
        assert_eq!(
            herdr::row(&pool, &key).await,
            Some((HostedState::Retired, herdr::EXECUTION.into()))
        );
        assert!(
            exclusion
                .disconnect_session_keeping_provider(&pool)
                .await
                .unwrap()
        );
        assert!(
            rig.writes().is_empty(),
            "recovery issues no physical mutation"
        );
        drop(exclusion);
        pool.close().await;
        db.drop().await;
    });
}

#[test]
fn idle_ended_bound_restart_repairs_without_physical_close_pg() {
    ended(
        HostedState::Bound,
        "idle_ended_bound_restart_repairs_without_physical_close_pg",
    );
}

#[test]
fn idle_ended_placed_pending_restart_repairs_without_physical_close_pg() {
    ended(
        HostedState::Pending,
        "idle_ended_placed_pending_restart_repairs_without_physical_close_pg",
    );
}

#[test]
fn confirmed_live_restart_is_not_retired_by_recovery_pg() {
    let Some((rig, runtime)) = rig("confirmed_live_restart_is_not_retired_by_recovery_pg", true)
    else {
        return;
    };
    runtime.block_on(async {
        let _root = TestRuntimeRootGuard::new();
        let (db, pool, shared, key, _switches) = seeded(&rig, HostedState::Bound).await;
        assert!(recover(&pool, &shared, &key).await.unwrap().is_none());
        assert_eq!(
            herdr::row(&pool, &key).await,
            Some((HostedState::Bound, herdr::EXECUTION.into()))
        );
        assert!(rig.writes().is_empty());
        pool.close().await;
        db.drop().await;
    });
}

#[test]
fn unconfirmed_restart_recovery_keeps_row_and_sends_nothing_pg() {
    let Some((rig, runtime)) = rig(
        "unconfirmed_restart_recovery_keeps_row_and_sends_nothing_pg",
        true,
    ) else {
        return;
    };
    runtime.block_on(async {
        let _root = TestRuntimeRootGuard::new();
        let (db, pool, shared, key, _switches) = seeded(&rig, HostedState::Bound).await;
        rig.run_other_execution();
        assert!(recover(&pool, &shared, &key).await.is_err());
        assert_eq!(
            herdr::row(&pool, &key).await,
            Some((HostedState::Bound, herdr::EXECUTION.into()))
        );
        assert!(rig.writes().is_empty());
        pool.close().await;
        db.drop().await;
    });
}

#[test]
fn restart_recovery_rechecks_dispatch_under_fence_pg() {
    let Some((rig, runtime)) = rig("restart_recovery_rechecks_dispatch_under_fence_pg", false)
    else {
        return;
    };
    runtime.block_on(async {
        let _root = TestRuntimeRootGuard::new();
        let (db, pool, shared, key, _switches) = seeded(&rig, HostedState::Bound).await;
        rig.restore_shell();
        let (changed_pool, changed_key) = (pool.clone(), key.clone());
        super::super::probe_after_fence(Box::new(move || Box::pin(async move {
            let changed = sqlx::query("UPDATE sessions SET active_dispatch_id = 'moved-dispatch' WHERE session_key = $1")
                .bind(changed_key).execute(&changed_pool).await.unwrap();
            assert_eq!(changed.rows_affected(), 1);
        })));
        assert!(recover(&pool, &shared, &key).await.is_err());
        assert_eq!(herdr::row(&pool, &key).await, Some((HostedState::Bound, herdr::EXECUTION.into())));
        assert!(rig.writes().is_empty());
        pool.close().await;
        db.drop().await;
    });
}
