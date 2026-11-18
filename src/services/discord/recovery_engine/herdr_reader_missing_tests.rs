//! Missing and unreadable panes through the restart reader and the durable row.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{HostedLookupKey, load_hosted_execution_pg};

fn check(missing: bool, shell: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[]);
    let mut record = record(&rig);
    record.owner.runtime_root = crate::config::runtime_root().unwrap().display().to_string();
    record.source_ref.runtime_root = record.owner.runtime_root.clone();
    let (db, pool, row) = runtime.block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)")
            .bind(&record.owner.logical_key)
            .bind(&record.owner.discord_token_hash)
            .bind(&record.owner.channel_id)
            .bind(serde_json::to_value(&record).unwrap())
            .execute(&pool).await.unwrap();
        let HostedLookup::Found(row) = load_hosted_execution_pg(&pool, HostedLookupKey::SessionKey(&record.owner.logical_key)).await else { panic!("stored row"); };
        (db, pool, row)
    });
    if missing {
        rig.show_panes(&[]);
    } else if shell {
        rig.run_provider(&rig.context(NONCE), false);
        rig.show_panes(&[PANE]);
        rig.foreground(&[SHELL]);
    }
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let result = if shell {
        use crate::services::herdr_launch::retire::{self, RetireRefusal, Scope};
        let result = runtime.block_on(retire::observed(&pool, &row, Scope::Missing));
        assert!(
            matches!(
                result,
                Err(RetireRefusal::Unproven("pane is not confirmed missing"))
            ),
            "automatic recovery accepts only Missing"
        );
        Reconnect::Unknown
    } else {
        reconnect_unless_pending(runtime.handle(), &pool, &record, &shared, &row).0
    };
    let actual = runtime.block_on(load_hosted_execution_pg(
        &pool,
        HostedLookupKey::SessionKey(&record.owner.logical_key),
    ));
    let HostedLookup::Found(actual) = actual else {
        panic!("read row");
    };
    let HostedRecord::Known(actual) = actual.record else {
        panic!("known row");
    };
    if missing {
        assert_eq!(result, Reconnect::Retired);
        assert_eq!(actual.state, HostedState::Retired);
    } else {
        assert_ne!(result, Reconnect::Retired);
        assert_eq!(actual, record, "unreadable is not Missing");
    }
    assert!(rig.sends().is_empty(), "reconnect never writes a pane");
    runtime.block_on(async {
        pool.close().await;
        db.drop().await;
    });
}

#[test]
fn a_missing_bound_pane_is_retired_by_the_restart_reader_pg() {
    check(true, false);
}

#[test]
fn an_unreadable_bound_pane_is_kept_by_the_restart_reader_pg() {
    check(false, false);
}

#[test]
fn automatic_missing_retire_does_not_retire_an_ended_shell_pg() {
    check(false, true);
}
