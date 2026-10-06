//! The boot start of delegated homes, from the switch through its rows to the watch and lease.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::o_channel_homes::HomeWrite;
use crate::services::cluster::channel_home::{HomeIntake, HomeOwnership, HomeRefusal, intake_hold};

const C: &str = "1490141479707086938";
const ID: u64 = 1_490_141_479_707_086_938;

/// A channel's drain reads: no turn, nothing owed, no POST running.
struct Idle;

impl DrainPort for Idle {
    async fn turn_running(&self) -> Option<bool> {
        Some(false)
    }

    async fn owed(&self) -> Option<Owed> {
        Some(Owed::default())
    }

    async fn posts_in_flight(&self) -> Option<usize> {
        Some(0)
    }

    async fn reset_legacy_source(&self) -> Result<(), ResetRefused> {
        unreachable!("the boot port supplies the reset")
    }
}

/// A reset that counts its calls and answers `refused` when set.
fn counted(calls: &Arc<AtomicUsize>, refused: Option<&str>) -> LegacyReset {
    let (calls, refused) = (Arc::clone(calls), refused.map(str::to_owned));
    Arc::new(move || {
        calls.fetch_add(1, Ordering::SeqCst);
        let answer = refused
            .clone()
            .map_or(Ok(()), |r| Err(ResetRefused::Refused(r)));
        Box::pin(std::future::ready(answer))
    })
}

fn boot(
    pool: &PgPool,
    local: &str,
    rows: Result<Vec<ChannelHome>, HomeError>,
    candidates: Vec<u64>,
    reset: LegacyReset,
    ports: &Arc<AtomicUsize>,
) -> Option<Boot<impl Fn(u64) -> BootPort<Idle>>> {
    let ports = Arc::clone(ports);
    let port = move |_| {
        ports.fetch_add(1, Ordering::SeqCst);
        BootPort::new(Idle, Arc::clone(&reset))
    };
    Some(Boot {
        provider: "claude".into(),
        local: local.into(),
        pool: pool.clone(),
        rows: Box::pin(std::future::ready(rows)),
        candidates,
        port,
    })
}

async fn rows(pool: &PgPool) -> Result<Vec<ChannelHome>, HomeError> {
    o_channel_homes::list_homes(pool).await
}

fn applied(write: Result<HomeWrite<ChannelHome>, HomeError>) -> ChannelHome {
    match write {
        Ok(HomeWrite::Applied(row)) => row,
        other => panic!("expected applied: {other:?}"),
    }
}

/// A row worker-owned by mini at its epoch.
async fn worker_row(pool: &PgPool) -> i64 {
    let released = applied(o_channel_homes::delegate(pool, C, "claude", "gw", "mini").await);
    let released = applied(o_channel_homes::finish_release(pool, C, "gw", released.epoch).await);
    applied(o_channel_homes::adopt(pool, C, "mini", released.epoch).await).epoch
}

/// Waits up to `secs` for `done`, polling every 20ms.
async fn eventually(secs: u64, mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..secs * 50 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    done()
}

fn owned(channel: &str) -> Option<(i64, HomeIntake)> {
    match channel_home::registered(channel)?.ownership() {
        HomeOwnership::Owned {
            home_epoch, intake, ..
        } => Some((home_epoch, intake)),
        HomeOwnership::Lost => None,
    }
}

fn lazy_pool() -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(50))
        .connect_lazy("postgres://127.0.0.1:1/none")
        .expect("lazy pool")
}

fn row(channel: &str, provider: &str, holder: Option<&str>, target: Option<&str>) -> ChannelHome {
    ChannelHome {
        channel_id: channel.into(),
        provider: provider.into(),
        state: HomeState::Worker,
        holder: holder.map(str::to_owned),
        target: target.map(str::to_owned),
        epoch: 1,
        renewed_at: None,
        updated_at: chrono::Utc::now(),
        detail: None,
    }
}

/// Off or unset, the boot builds nothing; on, only this provider's rows naming this node register,
/// and unreadable rows hold the local candidates.
#[tokio::test]
async fn the_switch_and_the_rows_decide_what_the_boot_registers() {
    let (pool, ports, resets) = (lazy_pool(), Arc::default(), Arc::default());
    let prepared = AtomicUsize::new(0);
    for switch in [None, Some(false)] {
        let prepare = || {
            prepared.fetch_add(1, Ordering::SeqCst);
            boot(
                &pool,
                "mini",
                Ok(Vec::new()),
                vec![ID],
                counted(&resets, None),
                &ports,
            )
        };
        assert!(start(switch, prepare).await.is_empty());
    }
    assert_eq!(prepared.load(Ordering::SeqCst), 0, "off: nothing prepared");

    let none = boot(
        &pool,
        "mini",
        Ok(Vec::new()),
        vec![ID],
        counted(&resets, None),
        &ports,
    );
    assert!(start(Some(true), || none).await.is_empty());
    let others = vec![
        row("101", "codex", Some("mini"), None),
        row("102", "claude", Some("gw"), Some("book")),
    ];
    let others = boot(
        &pool,
        "mini",
        Ok(others),
        vec![],
        counted(&resets, None),
        &ports,
    );
    assert!(start(Some(true), || others).await.is_empty());
    assert_eq!(
        ports.load(Ordering::SeqCst),
        0,
        "no port for an unnamed row"
    );
    assert!(!channel_home::any_registered() && live(|live| live.is_empty()));

    let unreadable = Err(HomeError::UnknownProvider("x".into()));
    let held = boot(
        &pool,
        "mini",
        unreadable,
        vec![ID],
        counted(&resets, None),
        &ports,
    );
    let started = start(Some(true), || held).await;
    assert_eq!(started.len(), 1);
    assert_eq!(owned(C), None, "held Lost");
    let health = channel_home::health().expect("registered");
    assert_eq!(health["home_draining"][0]["blocker"], "row_unreadable");
    stop(C).await;
    channel_home::unregister(C);
}

/// A holder's boot row gets one watch and one lease; the gate opens only by that lease's write,
/// and a second start ends both before its own gate is registered, as a restart begins Lost.
#[tokio::test]
async fn a_holders_row_starts_one_watch_and_lease_and_a_restart_reopens_by_write_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let epoch = worker_row(&pool).await;
    let (ports, resets) = (Arc::default(), Arc::default());
    let first = boot(
        &pool,
        "mini",
        rows(&pool).await,
        vec![],
        counted(&resets, None),
        &ports,
    );
    let first = start(Some(true), || first).await;
    assert_eq!(first.len(), 1);
    assert_eq!(owned(C), None, "registered Lost; a read opens nothing");
    assert!(eventually(5, || owned(C) == Some((epoch, HomeIntake::Open))).await);
    assert_eq!(running(C), (true, Some(epoch)));
    let (watch, lease) = handles(C);

    let second = boot(
        &pool,
        "mini",
        rows(&pool).await,
        vec![],
        counted(&resets, None),
        &ports,
    );
    let second = start(Some(true), || second).await;
    assert!(watch.is_finished() && lease.is_finished(), "ended before");
    assert!(first[0].withdrawn() && !Arc::ptr_eq(&first[0], &second[0]));
    assert_eq!(owned(C), None, "a restarted home begins Lost");
    assert!(eventually(5, || owned(C) == Some((epoch, HomeIntake::Open))).await);
    assert_eq!(
        (running(C), ports.load(Ordering::SeqCst)),
        ((true, Some(epoch)), 2)
    );
    stop(C).await;
    channel_home::unregister(C);
    pool.close().await;
    pg_db.drop().await;
}

/// A target holds the channel with no lease and moves no other channel's routing; once its row is
/// gone the watch returns the channel to the unregistered path, routing as before.
#[tokio::test]
async fn a_target_holds_without_a_lease_until_its_row_is_gone_pg() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::cutover::{intake_route, test_override};
    const OTHER: u64 = 1_490_141_479_707_086_939;
    let routes = |channel| {
        let placed = intake_route::route_for_placement("claude", channel);
        (intake_route::route("claude", channel), placed)
    };
    let _foreign = test_override::force_foreign(&[(ID, ClaudeTui), (OTHER, ClaudeTui)], "gw");
    let before = (routes(ID), routes(OTHER));
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    applied(o_channel_homes::delegate(&pool, C, "claude", "gw", "mini").await);
    let (ports, resets) = (Arc::default(), Arc::default());
    let target = boot(
        &pool,
        "mini",
        rows(&pool).await,
        vec![],
        counted(&resets, None),
        &ports,
    );
    assert_eq!(start(Some(true), || target).await.len(), 1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(running(C), (true, None), "no lease for a target");
    assert_eq!(channel_home::refusal(ID), Some(HomeRefusal::NotHeld));
    assert!(intake_hold(C, None).is_some());
    assert_eq!(routes(OTHER), before.1, "another channel routes as before");

    sqlx::query("DELETE FROM o_channel_homes WHERE channel_id = $1")
        .bind(C)
        .execute(&pool)
        .await
        .unwrap();
    assert!(eventually(8, || channel_home::registered(C).is_none()).await);
    assert!(!channel_home::any_registered());
    assert_eq!(
        (channel_home::refusal(ID), intake_hold(C, None)),
        (None, None)
    );
    assert!(eventually(2, || !running(C).0).await);
    assert_eq!(
        (routes(ID), routes(OTHER)),
        before,
        "routing as before the row"
    );
    stop(C).await;
    pool.close().await;
    pg_db.drop().await;
}

/// A releasing holder's drain runs the reset its runtime supplied exactly once before it leaves;
/// a refused reset keeps the row, and once the row names it no more the channel unregisters.
#[tokio::test]
async fn a_releasing_drain_runs_the_supplied_reset_once_and_waits_when_refused_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let epoch = applied(o_channel_homes::delegate(&pool, C, "claude", "gw", "mini").await).epoch;
    let (ports, resets) = (Arc::default(), Arc::new(AtomicUsize::new(0)));
    let refused = counted(&resets, Some("kept session"));
    let gw = boot(&pool, "gw", rows(&pool).await, vec![], refused, &ports);
    start(Some(true), || gw).await;
    let blocker = || {
        let health = channel_home::health();
        health.is_some_and(|health| health["home_draining"][0]["blocker"] == "source_reset")
    };
    assert!(eventually(12, blocker).await);
    let state = o_channel_homes::read_home(&pool, C).await.unwrap().unwrap();
    assert_eq!((state.state, state.epoch), (HomeState::Releasing, epoch));
    let refusals = resets.load(Ordering::SeqCst);
    assert!(refusals >= 1);

    let resets = Arc::new(AtomicUsize::new(0));
    let gw = boot(
        &pool,
        "gw",
        rows(&pool).await,
        vec![],
        counted(&resets, None),
        &ports,
    );
    start(Some(true), || gw).await;
    let released = || async {
        let row = o_channel_homes::read_home(&pool, C).await.unwrap().unwrap();
        row.state == HomeState::Released
    };
    let mut left = false;
    for _ in 0..750 {
        if released().await {
            left = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(left, "the drain leaves once the reset applied");
    assert_eq!(resets.load(Ordering::SeqCst), 1);
    assert!(eventually(8, || channel_home::registered(C).is_none()).await);
    stop(C).await;
    pool.close().await;
    pg_db.drop().await;
}

fn handles(channel: &str) -> (tokio::task::AbortHandle, tokio::task::AbortHandle) {
    live(|live| {
        let watched = live.get(channel).expect("started");
        let lease = watched.lease.locked();
        let lease = lease.as_ref().expect("a lease").1.abort_handle();
        (watched.watch.abort_handle(), lease)
    })
}

/// The gateway a reclaim targets drops the reclaimed row and the channel leaves the registry.
#[tokio::test]
async fn a_reclaims_target_drops_the_reclaimed_row_and_unregisters_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let epoch = worker_row(&pool).await;
    applied(o_channel_homes::begin_reclaim(&pool, C, epoch, "gw").await);
    let reclaimed = applied(o_channel_homes::finish_reclaim(&pool, C, "mini", epoch).await);
    assert_eq!(reclaimed.state, HomeState::Reclaimed);
    let (ports, resets) = (Arc::default(), Arc::default());
    let gw = boot(
        &pool,
        "gw",
        rows(&pool).await,
        vec![],
        counted(&resets, None),
        &ports,
    );
    assert_eq!(start(Some(true), || gw).await.len(), 1);
    assert!(eventually(5, || channel_home::registered(C).is_none()).await);
    assert_eq!(o_channel_homes::read_home(&pool, C).await.unwrap(), None);
    stop(C).await;
    pool.close().await;
    pg_db.drop().await;
}
