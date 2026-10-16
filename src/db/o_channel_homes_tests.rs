use std::time::Duration;

use sqlx::PgPool;

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

const C: &str = "1490141479707086938";
const F: ForceWindow = ForceWindow::MIN;

async fn home(pool: &PgPool) -> Option<ChannelHome> {
    read_home(pool, C).await.expect("read home")
}

fn applied<T>(write: Result<HomeWrite<T>, HomeError>) -> T {
    match write.expect("home write") {
        HomeWrite::Applied(value) => value,
        HomeWrite::Stale => panic!("expected the write to apply"),
    }
}

/// Each refused write is Stale and leaves the row exactly as it was before the write ran.
async fn refused<T: std::fmt::Debug>(
    pool: &PgPool,
    label: &str,
    write: impl std::future::Future<Output = Result<HomeWrite<T>, HomeError>>,
) {
    let before = home(pool).await;
    match write.await.expect("home write") {
        HomeWrite::Stale => {}
        HomeWrite::Applied(value) => panic!("{label}: applied {value:?}"),
    }
    assert_eq!(home(pool).await, before, "{label}: row changed");
}

async fn age_lease(pool: &PgPool, seconds: i64) {
    sqlx::query(
        "UPDATE o_channel_homes SET renewed_at = NOW() - make_interval(secs => $2)
          WHERE channel_id = $1",
    )
    .bind(C)
    .bind(seconds as f64)
    .execute(pool)
    .await
    .expect("age lease");
}

#[tokio::test]
async fn every_home_transition_applies_only_under_its_exact_condition_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    assert_eq!(home(&pool).await, None);

    let row = applied(delegate(&pool, C, "claude", "gw", "mini").await);
    assert_eq!(
        (
            row.state,
            row.holder.as_deref(),
            row.target.as_deref(),
            row.epoch
        ),
        (HomeState::Releasing, Some("gw"), Some("mini"), 1)
    );
    refused(
        &pool,
        "second delegate",
        delegate(&pool, C, "claude", "gw", "x"),
    )
    .await;
    refused(&pool, "renew other holder", renew(&pool, C, "mini", 1)).await;
    refused(&pool, "renew other epoch", renew(&pool, C, "gw", 2)).await;
    let held = applied(renew(&pool, C, "gw", 1).await);
    assert_eq!((held.epoch(), held.state()), (1, HomeState::Releasing));
    refused(&pool, "adopt before release", adopt(&pool, C, "mini", 1)).await;
    refused(
        &pool,
        "release by target",
        finish_release(&pool, C, "mini", 1),
    )
    .await;
    refused(
        &pool,
        "release old epoch",
        finish_release(&pool, C, "gw", 0),
    )
    .await;
    refused(
        &pool,
        "release as reclaim",
        finish_reclaim(&pool, C, "gw", 1),
    )
    .await;

    let row = applied(finish_release(&pool, C, "gw", 1).await);
    assert_eq!(
        (
            row.state,
            row.holder.as_deref(),
            row.target.as_deref(),
            row.epoch
        ),
        (HomeState::Released, None, Some("mini"), 2)
    );
    refused(&pool, "repeat release", finish_release(&pool, C, "gw", 1)).await;
    refused(&pool, "renew after leaving", renew(&pool, C, "gw", 1)).await;
    refused(&pool, "adopt by non-target", adopt(&pool, C, "gw", 2)).await;
    refused(&pool, "adopt old epoch", adopt(&pool, C, "mini", 1)).await;

    let row = applied(adopt(&pool, C, "mini", 2).await);
    assert_eq!(
        (
            row.state,
            row.holder.as_deref(),
            row.target.as_deref(),
            row.epoch
        ),
        (HomeState::Worker, Some("mini"), None, 2)
    );
    assert!(row.renewed_at.is_some());
    refused(&pool, "repeat adopt", adopt(&pool, C, "mini", 2)).await;
    refused(&pool, "reclaim old epoch", begin_reclaim(&pool, C, 1, "gw")).await;
    let held = applied(renew(&pool, C, "mini", 2).await);
    assert_eq!((held.holder(), held.state()), ("mini", HomeState::Worker));

    let row = applied(begin_reclaim(&pool, C, 2, "gw").await);
    assert_eq!(
        (
            row.state,
            row.holder.as_deref(),
            row.target.as_deref(),
            row.epoch
        ),
        (HomeState::Reclaiming, Some("mini"), Some("gw"), 2)
    );
    refused(&pool, "repeat reclaim", begin_reclaim(&pool, C, 2, "gw")).await;
    let held = applied(renew(&pool, C, "mini", 2).await);
    assert_eq!(held.state(), HomeState::Reclaiming);
    refused(
        &pool,
        "reclaim finish by gw",
        finish_reclaim(&pool, C, "gw", 2),
    )
    .await;
    refused(
        &pool,
        "reclaim finish old",
        finish_reclaim(&pool, C, "mini", 1),
    )
    .await;
    refused(
        &pool,
        "reclaim as release",
        finish_release(&pool, C, "mini", 2),
    )
    .await;

    let row = applied(finish_reclaim(&pool, C, "mini", 2).await);
    assert_eq!(
        (
            row.state,
            row.holder.as_deref(),
            row.target.as_deref(),
            row.epoch
        ),
        (HomeState::Reclaimed, None, Some("gw"), 3)
    );
    refused(&pool, "renew reclaimed", renew(&pool, C, "mini", 2)).await;
    refused(
        &pool,
        "remove by worker",
        remove_reclaimed(&pool, C, "mini", 3),
    )
    .await;
    refused(
        &pool,
        "remove old epoch",
        remove_reclaimed(&pool, C, "gw", 2),
    )
    .await;
    applied(remove_reclaimed(&pool, C, "gw", 3).await);
    assert_eq!(home(&pool).await, None);
    refused(&pool, "repeat remove", remove_reclaimed(&pool, C, "gw", 3)).await;

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn force_orphans_only_a_holder_silent_past_the_window_and_adopts_nothing_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    applied(delegate(&pool, C, "claude", "gw", "mini").await);
    applied(finish_release(&pool, C, "gw", 1).await);
    let force = force_orphan(&pool, C, 2, F, "op").await.expect("force");
    assert_eq!(force, ForceOutcome::Stale, "released has no holder");
    applied(adopt(&pool, C, "mini", 2).await);

    age_lease(&pool, 150).await;
    let before = home(&pool).await;
    let force = force_orphan(&pool, C, 2, F, "op").await.expect("force");
    assert_eq!(force, ForceOutcome::Fresh, "lease renewed inside F");
    assert_eq!(home(&pool).await, before);

    age_lease(&pool, 250).await;
    let force = force_orphan(&pool, C, 1, F, "op").await.expect("force");
    assert_eq!(force, ForceOutcome::Stale, "observed epoch moved on");
    let ForceOutcome::Orphaned(row) = force_orphan(&pool, C, 2, F, "op").await.expect("force")
    else {
        panic!("expected orphaned");
    };
    assert_eq!(
        (
            row.state,
            row.holder.as_deref(),
            row.epoch,
            row.detail.as_deref()
        ),
        (HomeState::Orphaned, None, 3, Some("op"))
    );
    // Orphaned holds: nobody renews, adopts or releases out of it.
    refused(&pool, "old holder renews", renew(&pool, C, "mini", 2)).await;
    refused(&pool, "old holder renews new", renew(&pool, C, "mini", 3)).await;
    for (target, epoch) in [("mini", 2), ("mini", 3), ("gw", 3)] {
        refused(&pool, "adopt orphan", adopt(&pool, C, target, epoch)).await;
    }
    refused(&pool, "reclaim orphan", begin_reclaim(&pool, C, 3, "gw")).await;
    refused(&pool, "remove orphan", remove_reclaimed(&pool, C, "gw", 3)).await;
    let force = force_orphan(&pool, C, 3, F, "op").await.expect("force");
    assert_eq!(force, ForceOutcome::Stale);

    pool.close().await;
    pg_db.drop().await;
}

/// T-H9 and the F floor: no window shorter than F exists, the floor sits exactly at F, and an
/// orphaned home is adopted by nobody and never reads as released.
#[tokio::test]
async fn force_never_runs_inside_f_and_leaves_an_orphan_nobody_adopts_pg() {
    let second = Duration::from_secs(1);
    assert_eq!(ForceWindow::at_least(FORCE_AFTER - second), None);
    assert_eq!(ForceWindow::at_least(FORCE_AFTER), Some(ForceWindow::MIN));
    assert!(ForceWindow::at_least(FORCE_AFTER + second).is_some());

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    applied(delegate(&pool, C, "claude", "gw", "mini").await);
    applied(finish_release(&pool, C, "gw", 1).await);
    applied(adopt(&pool, C, "mini", 2).await);
    age_lease(&pool, 150).await;
    let held = home(&pool).await;
    if let Some(short) = ForceWindow::at_least(Duration::from_secs(100)) {
        let _ = force_orphan(&pool, C, 2, short, "op").await;
    }
    age_lease(&pool, 199).await;
    let before = home(&pool).await;
    assert_eq!(
        before.as_ref().map(|h| (h.state, h.epoch)),
        held.map(|h| (h.state, h.epoch))
    );
    let force = force_orphan(&pool, C, 2, F, "op").await.expect("force");
    assert_eq!(force, ForceOutcome::Fresh, "inside F");
    assert_eq!(home(&pool).await, before);

    age_lease(&pool, 201).await;
    let force = force_orphan(&pool, C, 2, F, "op").await.expect("force");
    assert!(matches!(force, ForceOutcome::Orphaned(_)), "{force:?}");
    let orphan = home(&pool).await.expect("row");
    assert_eq!(
        (orphan.state, orphan.holder.as_deref()),
        (HomeState::Orphaned, None)
    );
    for (node, epoch) in [("mini", 2), ("mini", 3), ("gw", 2), ("gw", 3)] {
        refused(&pool, "adopt orphan", adopt(&pool, C, node, epoch)).await;
        refused(
            &pool,
            "release orphan",
            finish_release(&pool, C, node, epoch),
        )
        .await;
        refused(
            &pool,
            "reclaim orphan",
            finish_reclaim(&pool, C, node, epoch),
        )
        .await;
    }
    assert_eq!(
        home(&pool).await.map(|h| h.state),
        Some(HomeState::Orphaned)
    );
    pool.close().await;
    pg_db.drop().await;
}

async fn lock_waiters(pool: &PgPool, expected: i64) {
    for _ in 0..200 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity
              WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .expect("lock waiters");
        if waiting >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{expected} writers never queued on the row lock");
}

/// A renewal and a force queue on one locked row; whichever runs first decides, and the other
/// sees the row it left. `renew_first` sets the queue order.
async fn race_renewal_and_force(renew_first: bool) -> (HomeWrite<HeldHome>, ForceOutcome) {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(6).await;
    applied(delegate(&pool, C, "claude", "gw", "mini").await);
    applied(finish_release(&pool, C, "gw", 1).await);
    applied(adopt(&pool, C, "mini", 2).await);
    age_lease(&pool, 250).await;

    let mut blocker = pool.begin().await.expect("blocker");
    sqlx::query("SELECT 1 FROM o_channel_homes WHERE channel_id = $1 FOR UPDATE")
        .bind(C)
        .execute(&mut *blocker)
        .await
        .expect("lock row");
    let (renew_pool, force_pool) = (pool.clone(), pool.clone());
    let renewal = async move { renew(&renew_pool, C, "mini", 2).await };
    let force = async move { force_orphan(&force_pool, C, 2, F, "op").await };
    let (renewal, force) = if renew_first {
        let renewal = tokio::spawn(renewal);
        lock_waiters(&pool, 1).await;
        let force = tokio::spawn(force);
        lock_waiters(&pool, 2).await;
        (renewal, force)
    } else {
        let force = tokio::spawn(force);
        lock_waiters(&pool, 1).await;
        let renewal = tokio::spawn(renewal);
        lock_waiters(&pool, 2).await;
        (renewal, force)
    };
    blocker.commit().await.expect("release lock");
    let renewal = renewal.await.expect("join").expect("renew");
    let force = force.await.expect("join").expect("force");
    let after = home(&pool).await.expect("row");
    match (&renewal, &force) {
        (HomeWrite::Applied(_), ForceOutcome::Fresh) => {
            assert_eq!((after.state, after.epoch), (HomeState::Worker, 2));
        }
        (HomeWrite::Stale, ForceOutcome::Orphaned(_)) => {
            assert_eq!((after.state, after.epoch), (HomeState::Orphaned, 3));
        }
        other => panic!("both writers decided: {other:?}"),
    }
    pool.close().await;
    pg_db.drop().await;
    (renewal, force)
}

#[tokio::test]
async fn a_renewal_and_a_force_racing_on_one_row_never_both_win_pg() {
    let (renewal, force) = race_renewal_and_force(true).await;
    assert!(matches!(renewal, HomeWrite::Applied(_)), "{renewal:?}");
    assert_eq!(
        force,
        ForceOutcome::Fresh,
        "force re-checks the renewed row"
    );
    let (renewal, force) = race_renewal_and_force(false).await;
    assert_eq!(renewal, HomeWrite::Stale);
    assert!(matches!(force, ForceOutcome::Orphaned(_)), "{force:?}");
}

/// The database layer refuses a provider other than claude or codex itself, before any write, so
/// a caller that skips the CLI's own check still stores nothing.
#[tokio::test]
async fn a_delegate_naming_an_unknown_provider_writes_nothing_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let refused = delegate(&pool, C, "gemini", "gw", "mini").await;
    assert!(
        matches!(&refused, Err(HomeError::UnknownProvider(name)) if name == "gemini"),
        "{refused:?}"
    );
    assert_eq!(home(&pool).await, None);
    pool.close().await;
    pg_db.drop().await;
}

/// The row and PG's clock come from one SELECT, so a reader ages the lease on the database's
/// clock with no second round trip.
#[tokio::test]
async fn a_row_read_carries_pgs_clock_from_the_same_statement_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let clock = || async {
        let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&pool)
            .await
            .unwrap();
        now
    };
    assert_eq!(read_home_at(&pool, C).await.unwrap(), None);
    delegate(&pool, C, "claude", "gw", "mini").await.unwrap();
    age_lease(&pool, 5).await;
    let before = clock().await;
    let (row, read_at) = read_home_at(&pool, C).await.unwrap().expect("row");
    let after = clock().await;
    assert_eq!(
        Some(&row),
        home(&pool).await.as_ref(),
        "the same row as read_home"
    );
    assert!(
        before <= read_at && read_at <= after,
        "{before} <= {read_at} <= {after}"
    );
    let age = read_at - row.renewed_at.expect("renewed");
    assert!(
        age >= chrono::Duration::seconds(5) && age < chrono::Duration::seconds(6),
        "{age:?}"
    );
    pool.close().await;
    pg_db.drop().await;
}

async fn open_intake(pool: &PgPool, key: &str, home_epoch: Option<i64>) {
    sqlx::query(
        "INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id,
            request_owner_id, user_text, turn_kind, agent_id, provider, status, home_epoch
         ) VALUES ('mini', 'gw', $1, $2, 'user', 'text', 'standard', 'agent', 'claude',
            'pending', $3)",
    )
    .bind(C)
    .bind(key)
    .bind(home_epoch)
    .execute(pool)
    .await
    .expect("seed open intake");
}

async fn settle_intake(pool: &PgPool) {
    sqlx::query("UPDATE intake_outbox SET status = 'done' WHERE channel_id = $1")
        .bind(C)
        .execute(pool)
        .await
        .expect("settle intake");
}

/// A released row goes only from a node that is not its target, at its exact epoch; every other
/// state of a channel's lifecycle is kept by abort-release.
#[tokio::test]
async fn abort_release_removes_only_a_released_row_at_its_exact_epoch_from_the_gateway_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let released = applied(delegate(&pool, C, "claude", "gw", "mini").await);
    let released = applied(finish_release(&pool, C, "gw", released.epoch).await).epoch;
    for epoch in [released - 1, released + 1] {
        refused(&pool, "other epoch", abort_release(&pool, C, epoch, "gw")).await;
    }
    let on_target = abort_release(&pool, C, released, "mini");
    refused(&pool, "run on the target", on_target).await;
    let row = applied(abort_release(&pool, C, released, "gw").await);
    let removed = (row.state, row.target.as_deref(), row.epoch);
    assert_eq!(removed, (HomeState::Released, Some("mini"), released));
    assert_eq!(home(&pool).await, None);
    refused(&pool, "no row", abort_release(&pool, C, released, "gw")).await;

    let row = applied(delegate(&pool, C, "claude", "gw", "mini").await);
    let epoch = applied(finish_release(&pool, C, "gw", row.epoch).await).epoch;
    applied(adopt(&pool, C, "mini", epoch).await);
    for node in ["gw", "mini"] {
        refused(&pool, "worker", abort_release(&pool, C, epoch, node)).await;
    }
    applied(begin_reclaim(&pool, C, epoch, "gw").await);
    for node in ["gw", "mini"] {
        refused(&pool, "reclaiming", abort_release(&pool, C, epoch, node)).await;
    }
    let reclaimed = applied(finish_reclaim(&pool, C, "mini", epoch).await).epoch;
    for node in ["gw", "mini"] {
        refused(&pool, "reclaimed", abort_release(&pool, C, reclaimed, node)).await;
    }
    applied(remove_reclaimed(&pool, C, "gw", reclaimed).await);

    applied(delegate(&pool, C, "claude", "gw", "mini").await);
    let epoch = home(&pool).await.expect("row").epoch;
    age_lease(&pool, 250).await;
    let ForceOutcome::Orphaned(orphan) = force_orphan(&pool, C, epoch, F, "op").await.unwrap()
    else {
        panic!("expected orphaned");
    };
    for node in ["gw", "mini"] {
        let abort = abort_release(&pool, C, orphan.epoch, node);
        refused(&pool, "orphaned", abort).await;
    }
    pool.close().await;
    pg_db.drop().await;
}

/// A releasing row goes only from its holder at its exact epoch, and an open intake row routed
/// before the delegation keeps it.
#[tokio::test]
async fn abort_release_takes_a_releasing_row_only_from_its_holder_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let epoch = applied(delegate(&pool, C, "claude", "gw", "mini").await).epoch;
    for node in ["mini", "book"] {
        refused(
            &pool,
            "not the holder",
            abort_release(&pool, C, epoch, node),
        )
        .await;
    }
    for other in [epoch - 1, epoch + 1] {
        refused(&pool, "other epoch", abort_release(&pool, C, other, "gw")).await;
    }
    open_intake(&pool, "unrouted", None).await;
    refused(&pool, "open intake", abort_release(&pool, C, epoch, "gw")).await;
    settle_intake(&pool).await;
    let row = applied(abort_release(&pool, C, epoch, "gw").await);
    let removed = (row.state, row.holder.as_deref(), row.epoch);
    assert_eq!(removed, (HomeState::Releasing, Some("gw"), epoch));
    assert_eq!(home(&pool).await, None);
    pool.close().await;
    pg_db.drop().await;
}

/// One open intake row of any routed epoch, alone, keeps a row abort-release would otherwise drop.
#[tokio::test]
async fn abort_release_refuses_while_any_intake_is_open_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let first = applied(delegate(&pool, C, "claude", "gw", "mini").await).epoch;
    let released = applied(finish_release(&pool, C, "gw", first).await).epoch;
    let open = [
        ("current", Some(released)),
        ("other_epoch", Some(first)),
        ("unrouted", None),
    ];
    for (key, routed) in open {
        open_intake(&pool, key, routed).await;
        let expected = OpenIntake {
            current: i64::from(key == "current"),
            other_epoch: i64::from(key == "other_epoch"),
            unrouted: i64::from(key == "unrouted"),
        };
        let counted = open_intake_by_epoch(&pool, C, released).await.unwrap();
        assert_eq!(counted, expected, "{key}");
        refused(&pool, key, abort_release(&pool, C, released, "gw")).await;
        settle_intake(&pool).await;
    }
    applied(abort_release(&pool, C, released, "gw").await);
    assert_eq!(home(&pool).await, None);
    pool.close().await;
    pg_db.drop().await;
}

/// Queues `first` then `second` on the channel's locked row, then lets both run in that order.
async fn queued_on_the_row<A, B>(pool: &PgPool, first: A, second: B) -> (A::Output, B::Output)
where
    A: std::future::Future + Send + 'static,
    A::Output: Send + 'static,
    B: std::future::Future + Send + 'static,
    B::Output: Send + 'static,
{
    let mut blocker = pool.begin().await.expect("blocker");
    sqlx::query("SELECT 1 FROM o_channel_homes WHERE channel_id = $1 FOR UPDATE")
        .bind(C)
        .execute(&mut *blocker)
        .await
        .expect("lock row");
    let first = tokio::spawn(first);
    lock_waiters(pool, 1).await;
    let second = tokio::spawn(second);
    lock_waiters(pool, 2).await;
    blocker.commit().await.expect("release lock");
    (first.await.expect("join"), second.await.expect("join"))
}

async fn drop_home(pool: &PgPool) {
    sqlx::query("DELETE FROM o_channel_homes WHERE channel_id = $1")
        .bind(C)
        .execute(pool)
        .await
        .expect("drop row");
}

/// The target's adopt and the gateway's abort on one released row: whichever runs first decides,
/// and the other finds the row it left.
#[tokio::test]
async fn an_abort_and_an_adopt_racing_on_one_released_row_never_both_win_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(6).await;
    for adopt_first in [true, false] {
        let row = applied(delegate(&pool, C, "claude", "gw", "mini").await);
        let epoch = applied(finish_release(&pool, C, "gw", row.epoch).await).epoch;
        let (adopt_pool, abort_pool) = (pool.clone(), pool.clone());
        let adopting = async move { adopt(&adopt_pool, C, "mini", epoch).await.unwrap() };
        let aborting = async move { abort_release(&abort_pool, C, epoch, "gw").await.unwrap() };
        let (adopted, aborted) = if adopt_first {
            queued_on_the_row(&pool, adopting, aborting).await
        } else {
            let (aborted, adopted) = queued_on_the_row(&pool, aborting, adopting).await;
            (adopted, aborted)
        };
        let after = home(&pool).await.map(|row| (row.state, row.epoch));
        match (adopt_first, &adopted, &aborted) {
            (true, HomeWrite::Applied(_), HomeWrite::Stale) => {
                assert_eq!(after, Some((HomeState::Worker, epoch)));
            }
            (false, HomeWrite::Stale, HomeWrite::Applied(_)) => assert_eq!(after, None),
            other => panic!("adopt first {adopt_first}: {other:?}"),
        }
        drop_home(&pool).await;
    }
    pool.close().await;
    pg_db.drop().await;
}

/// The holder's drain finishing and the gateway's abort on one releasing row: exactly one applies,
/// leaving the row released or gone, and neither retries.
#[tokio::test]
async fn a_finished_release_and_an_abort_racing_on_one_releasing_row_never_both_win_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(6).await;
    for release_first in [true, false] {
        let epoch = applied(delegate(&pool, C, "claude", "gw", "mini").await).epoch;
        let (release_pool, abort_pool) = (pool.clone(), pool.clone());
        let leaving = async move { finish_release(&release_pool, C, "gw", epoch).await.unwrap() };
        let aborting = async move { abort_release(&abort_pool, C, epoch, "gw").await.unwrap() };
        let (left, aborted) = if release_first {
            queued_on_the_row(&pool, leaving, aborting).await
        } else {
            let (aborted, left) = queued_on_the_row(&pool, aborting, leaving).await;
            (left, aborted)
        };
        let after = home(&pool).await;
        let after = after.map(|row| (row.state, row.holder, row.target, row.epoch > epoch));
        match (release_first, &left, &aborted) {
            (true, HomeWrite::Applied(_), HomeWrite::Stale) => {
                let released = (HomeState::Released, None, Some("mini".into()), true);
                assert_eq!(after, Some(released));
            }
            (false, HomeWrite::Stale, HomeWrite::Applied(_)) => assert_eq!(after, None),
            other => panic!("release first {release_first}: {other:?}"),
        }
        drop_home(&pool).await;
    }
    pool.close().await;
    pg_db.drop().await;
}
