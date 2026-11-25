//! A Herdr create proven unsent retires an execution that never had a pane: presence reads that
//! row as no session, while every other unplaced or busy row still holds.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::tests::pending;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedRecord, HostedState,
};
use crate::services::herdr_launch::{HerdrLaunchError, retire_unsent_for_test as retire_unsent};

/// The host scope of the configured Herdr channel whose exact row reads `found`.
fn read(
    owner: &HostedOwner,
    key: &str,
    found: &HostedLookup,
    inflight: &Result<Option<InflightTurnState>, String>,
) -> Scope {
    let config = config(CHANNEL, true);
    let evidence = Ok(evidence(found, key, &owner.logical_key));
    let marker = HostKindMarker::Absent;
    scope(
        &config,
        owner,
        key,
        (found, found),
        evidence,
        marker,
        inflight,
    )
}

fn retired(owner: &HostedOwner) -> HostedExecution {
    let mut execution = pending(owner, "n1");
    execution.state = HostedState::Retired;
    execution
}

#[tokio::test]
async fn unsent_create_retire_reads_idle_no_session_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = test_owner(CHANNEL);
    let key =
        adk_session::build_namespaced_session_key(TOKEN, &ProviderKind::Claude, &owner.logical_key);
    let raw = Some(wire(&pending(&owner, "n1")));
    inflight::seed_session_row_keyed(&pool, &key, CHANNEL, raw).await;
    let unsent = HerdrLaunchError::NotSent("connect refused".into());
    assert_eq!(
        retire_unsent(&pool, &owner, "n1", unsent.clone()).await,
        unsent
    );
    let found = lookup(&pool, &key).await;
    let HostedLookup::Found(row) = &found else {
        panic!("the unsent create left {found:?}");
    };
    assert_eq!(row.record, HostedRecord::Known(retired(&owner)));
    let result = read(&owner, &key, &found, &Ok(None));
    assert_eq!(result, Scope::HerdrRetired);
    let host = HostScope {
        channel: CHANNEL,
        provider: ProviderKind::Claude,
        name: owner.logical_key.clone(),
        result,
    };
    let names = candidates(Some("s0u-host"), &[], vec![], None);
    let inventory = Inventory::ProbeFailed("tmux unsupported".into());
    assert_eq!(
        decide_unbound(names, host, &inventory, &BTreeMap::new()),
        Verdict::Absent(Absence::HerdrRetired)
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn unplaced_pending_or_foreign_owner_or_inflight_still_holds_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = test_owner(CHANNEL);
    let key =
        adk_session::build_namespaced_session_key(TOKEN, &ProviderKind::Claude, &owner.logical_key);
    let mut foreign = owner.clone();
    foreign.runtime_root = "/other/runtime".into();
    let mut evidenced = retired(&owner);
    evidenced.expected = record(&owner, "n1", HostedState::Retired).expected;
    let busy = Ok(Some(row(CHANNEL, &owner.logical_key)));
    let cases = [
        ("pending", pending(&owner, "n1"), Ok(None)),
        ("foreign owner", retired(&foreign), Ok(None)),
        ("evidence without a pane", evidenced, Ok(None)),
        ("inflight", retired(&owner), busy),
    ];
    for (label, execution, turn) in cases {
        let raw = Some(wire(&execution));
        inflight::seed_session_row_keyed(&pool, &key, CHANNEL, raw).await;
        let found = lookup(&pool, &key).await;
        let result = read(&owner, &key, &found, &turn);
        assert!(matches!(result, Scope::Hold(_)), "{label}: {result:?}");
    }
    pool.close().await;
    db.drop().await;
}
