use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::o_channel_homes::{ForceOutcome, ForceWindow};
use crate::services::cluster::channel_home::{
    HOLD_FOR, HomeIntake, LeaseRound, intake_hold, lease_round, register, run_lease,
};

const C: &str = "1490141479707086938";

/// Discord as the writers see it: every admitted POST, and the ones still running per node.
#[derive(Default)]
struct Sink {
    posts: Mutex<Vec<(&'static str, u32, i64)>>,
    running: Mutex<BTreeMap<&'static str, usize>>,
    most_at_once: AtomicUsize,
}

impl Sink {
    fn begin(&self, node: &'static str, piece: u32, epoch: i64) {
        self.posts.lock().unwrap().push((node, piece, epoch));
        let mut running = self.running.lock().unwrap();
        *running.entry(node).or_default() += 1;
        let total = running.values().sum();
        self.most_at_once.fetch_max(total, Ordering::SeqCst);
    }

    fn end(&self, node: &'static str) {
        *self.running.lock().unwrap().entry(node).or_default() -= 1;
    }

    fn running(&self, node: &'static str) -> usize {
        self.running.lock().unwrap().get(node).copied().unwrap_or(0)
    }

    fn posts(&self) -> Vec<(&'static str, u32, i64)> {
        self.posts.lock().unwrap().clone()
    }

    fn most_at_once(&self) -> usize {
        self.most_at_once.load(Ordering::SeqCst)
    }

    /// One POST through `home`'s admission, still running when this returns.
    fn start(&self, node: &'static str, home: &HomeGate, piece: u32) -> bool {
        home.admit(|epoch| self.begin(node, piece, epoch)).is_some()
    }
}

/// One node's O actor: its owed pieces go out only through the gate it is handed.
struct Actor {
    node: &'static str,
    sink: Arc<Sink>,
    owed: Mutex<Vec<u32>>,
    unreadable: AtomicBool,
    turn: AtomicBool,
    reads: AtomicUsize,
    /// The owed read with this index finds one more piece, as if it arrived just then.
    arrives_at_read: Mutex<Option<usize>>,
    resets: AtomicUsize,
}

impl Actor {
    fn new(node: &'static str, sink: &Arc<Sink>, owed: &[u32]) -> Self {
        Self {
            node,
            sink: Arc::clone(sink),
            owed: Mutex::new(owed.to_vec()),
            unreadable: AtomicBool::new(false),
            turn: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            arrives_at_read: Mutex::new(None),
            resets: AtomicUsize::new(0),
        }
    }

    /// Posts each owed piece the gate admits; a refused piece stays owed.
    fn deliver(&self, home: &HomeGate) {
        self.owed.lock().unwrap().retain(|&piece| {
            let posted = self.sink.start(self.node, home, piece);
            if posted {
                self.sink.end(self.node);
            }
            !posted
        });
    }
}

impl DrainPort for Actor {
    async fn turn_running(&self) -> Option<bool> {
        Some(self.turn.load(Ordering::SeqCst))
    }

    async fn owed(&self) -> Option<Owed> {
        let read = self.reads.fetch_add(1, Ordering::SeqCst);
        if *self.arrives_at_read.lock().unwrap() == Some(read) {
            self.owed.lock().unwrap().push(99);
        }
        if self.unreadable.load(Ordering::SeqCst) {
            return None;
        }
        let owed = self.owed.lock().unwrap().len();
        Some(Owed {
            owed,
            ..Owed::default()
        })
    }

    async fn posts_in_flight(&self) -> Option<usize> {
        Some(self.sink.running(self.node))
    }

    async fn reset_legacy_source(&self) -> Result<(), ResetRefused> {
        self.resets.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn applied(write: Result<HomeWrite<ChannelHome>, HomeError>) -> ChannelHome {
    match write {
        Ok(HomeWrite::Applied(row)) => row,
        other => panic!("expected applied: {other:?}"),
    }
}

async fn row(pool: &PgPool) -> Option<(HomeState, Option<String>, i64)> {
    let home = o_channel_homes::read_home(pool, C).await.expect("read");
    home.map(|home| (home.state, home.holder, home.epoch))
}

/// The holder's own renewal write, the only thing that opens its gate.
async fn renew(pool: &PgPool, home: &HomeGate, epoch: i64) -> LeaseRound {
    let renewal = o_channel_homes::renew(pool, C, home.holder(), epoch);
    lease_round(home, epoch, Instant::now(), renewal).await
}

fn owned(home: &HomeGate) -> Option<(i64, HomeIntake)> {
    match home.ownership() {
        HomeOwnership::Owned {
            home_epoch, intake, ..
        } => Some((home_epoch, intake)),
        HomeOwnership::Lost => None,
    }
}

async fn delegated(pool: &PgPool) -> i64 {
    let home = o_channel_homes::delegate(pool, C, "claude", "gw", "mini").await;
    applied(home).epoch
}

/// A worker-owned row held by mini, opened by mini's renewal.
async fn worker_home(pool: &PgPool) -> (Arc<HomeGate>, i64) {
    let first = delegated(pool).await;
    let released = applied(o_channel_homes::finish_release(pool, C, "gw", first).await);
    let epoch = released.epoch;
    applied(o_channel_homes::adopt(pool, C, "mini", epoch).await);
    let mini = Arc::new(HomeGate::new(C, "mini"));
    assert!(matches!(
        renew(pool, &mini, epoch).await,
        LeaseRound::Renewed(_)
    ));
    assert_eq!(owned(&mini), Some((epoch, HomeIntake::Open)));
    (mini, epoch)
}

/// T-H7: releasing closes only intake; the holder posts the owed tail at its epoch, once each,
/// and leaves only after the final close, with no intake taken in between.
#[tokio::test]
async fn a_releasing_holder_posts_its_owed_tail_then_leaves_after_the_final_close_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let epoch = delegated(&pool).await;
    let gw = Arc::new(HomeGate::new(C, "gw"));
    register(Arc::clone(&gw));
    let sink = Arc::new(Sink::default());
    let actor = Actor::new("gw", &sink, &[1, 2, 3]);

    let step = drain_round(&pool, &gw, &actor).await;
    assert!(
        matches!(step, DrainStep::Waiting(Blocker::NotHeld)),
        "{step:?}"
    );
    assert!(matches!(
        renew(&pool, &gw, epoch).await,
        LeaseRound::Renewed(_)
    ));
    assert_eq!(owned(&gw), Some((epoch, HomeIntake::Closed)));

    let mut left = None;
    for _ in 0..4 {
        assert!(intake_hold(C, Some(epoch)).is_some() && !gw.intake_open());
        match drain_round(&pool, &gw, &actor).await {
            DrainStep::Left(row) => {
                left = Some(row);
                break;
            }
            DrainStep::Waiting(Blocker::Owed(_)) => actor.deliver(&gw),
            other => panic!("unexpected drain step: {other:?}"),
        }
    }
    let left = left.expect("the drain completes once the tail is posted");
    assert_eq!(
        (left.state, left.holder.as_deref()),
        (HomeState::Released, None)
    );
    assert!(left.epoch > epoch);
    let posts = sink.posts();
    assert_eq!(
        posts,
        [("gw", 1, epoch), ("gw", 2, epoch), ("gw", 3, epoch)]
    );
    assert_eq!(
        actor.resets.load(Ordering::SeqCst),
        1,
        "the gateway resets its source once"
    );
    assert_eq!((owned(&gw), gw.admit(|e| e)), (None, None));
    let stale = renew(&pool, &gw, epoch).await;
    assert!(matches!(stale, LeaseRound::Stale), "{stale:?}");
    channel_home::unregister(C);
    pool.close().await;
    pg_db.drop().await;
}

/// A piece owed after the last check before the final close keeps the row where it is; the
/// drain reopens the gate at the same epoch to post it, and only then leaves.
#[tokio::test]
async fn a_piece_owed_after_the_last_check_is_found_after_the_final_close_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (mini, epoch) = worker_home(&pool).await;
    applied(o_channel_homes::begin_reclaim(&pool, C, epoch, "gw").await);
    let sink = Arc::new(Sink::default());
    let actor = Actor::new("mini", &sink, &[]);
    *actor.arrives_at_read.lock().unwrap() = Some(1);

    let step = drain_round(&pool, &mini, &actor).await;
    assert!(
        matches!(step, DrainStep::Waiting(Blocker::Owed(_))),
        "{step:?}"
    );
    let reclaiming = Some((HomeState::Reclaiming, Some("mini".to_string()), epoch));
    assert_eq!(
        row(&pool).await,
        reclaiming,
        "no leaving write with a piece owed"
    );
    assert_eq!(owned(&mini), Some((epoch, HomeIntake::Closed)));
    actor.deliver(&mini);
    assert_eq!(sink.posts(), [("mini", 99, epoch)]);

    let step = drain_round(&pool, &mini, &actor).await;
    let DrainStep::Left(left) = step else {
        panic!("expected left: {step:?}");
    };
    assert_eq!(
        (left.state, left.target.as_deref()),
        (HomeState::Reclaimed, Some("gw"))
    );
    assert_eq!(
        actor.resets.load(Ordering::SeqCst),
        0,
        "the worker keeps its pane"
    );
    pool.close().await;
    pg_db.drop().await;
}

/// An unreadable projection is not an empty one, and a dropped hold reopens only through the
/// holder's own renewal at the same epoch.
#[tokio::test]
async fn an_unreadable_projection_or_a_dropped_hold_keeps_the_drain_waiting_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (mini, epoch) = worker_home(&pool).await;
    applied(o_channel_homes::begin_reclaim(&pool, C, epoch, "gw").await);
    let sink = Arc::new(Sink::default());
    let actor = Actor::new("mini", &sink, &[]);
    actor.unreadable.store(true, Ordering::SeqCst);
    let reclaiming = Some((HomeState::Reclaiming, Some("mini".to_string()), epoch));
    for _ in 0..2 {
        let step = drain_round(&pool, &mini, &actor).await;
        assert!(
            matches!(step, DrainStep::Waiting(Blocker::OwedUnreadable)),
            "{step:?}"
        );
        assert_eq!(row(&pool).await, reclaiming);
        assert_eq!(owned(&mini), Some((epoch, HomeIntake::Closed)));
    }

    actor.unreadable.store(false, Ordering::SeqCst);
    actor.turn.store(true, Ordering::SeqCst);
    let step = drain_round(&pool, &mini, &actor).await;
    assert!(
        matches!(step, DrainStep::Waiting(Blocker::TurnRunning)),
        "{step:?}"
    );
    actor.turn.store(false, Ordering::SeqCst);
    mini.renewal_stale(epoch);
    let step = drain_round(&pool, &mini, &actor).await;
    assert!(
        matches!(step, DrainStep::Waiting(Blocker::NotHeld)),
        "{step:?}"
    );
    assert_eq!(row(&pool).await, reclaiming);
    assert!(matches!(
        renew(&pool, &mini, epoch).await,
        LeaseRound::Renewed(_)
    ));
    assert_eq!(owned(&mini), Some((epoch, HomeIntake::Closed)));

    // An open intake row of the channel keeps the row and the gate as they are until it ends.
    sqlx::query(
        "INSERT INTO intake_outbox (target_instance_id, forwarded_by_instance_id, channel_id,
            user_msg_id, request_owner_id, user_text, turn_kind, agent_id, provider, status)
         VALUES ('mini', 'gw', $1, 'm1', 'user', 'hi', 'standard', 'agent', 'claude', 'pending')",
    )
    .bind(C)
    .execute(&pool)
    .await
    .expect("seed intake row");
    let step = drain_round(&pool, &mini, &actor).await;
    assert!(
        matches!(step, DrainStep::Waiting(Blocker::OpenIntake(1))),
        "{step:?}"
    );
    assert_eq!(row(&pool).await, reclaiming);
    assert_eq!(owned(&mini), Some((epoch, HomeIntake::Closed)));
    sqlx::query("UPDATE intake_outbox SET status = 'done'")
        .execute(&pool)
        .await
        .expect("settle intake row");
    let step = drain_round(&pool, &mini, &actor).await;
    assert!(matches!(step, DrainStep::Left(_)), "{step:?}");
    pool.close().await;
    pg_db.drop().await;
}

/// How many of a channel's live gates are owned; never more than one.
fn owners(gates: &[&HomeGate]) -> usize {
    gates.iter().filter(|gate| owned(gate).is_some()).count()
}

async fn wait_owned(home: &HomeGate) {
    for _ in 0..250 {
        if owned(home).is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{} never took the home", home.holder());
}

async fn reset(pool: &PgPool) {
    let cleared = sqlx::query("DELETE FROM o_channel_homes")
        .execute(pool)
        .await;
    cleared.expect("clear homes");
}

/// T-H6: two nodes, two gates and their lease tasks on one database. Through a hand-over with a
/// POST still running, and each restart of 7.7-7, no two gates are owned and no two POSTs overlap.
#[tokio::test]
async fn two_nodes_never_hold_or_post_at_once_across_hand_over_and_restarts_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let sink = Arc::new(Sink::default());

    // Hand-over: gw's last POST runs past its final close; mini adopts only after it ends.
    let epoch = delegated(&pool).await;
    let gw = Arc::new(HomeGate::new(C, "gw"));
    let gw_lease = tokio::spawn(run_lease(pool.clone(), Arc::clone(&gw), epoch));
    wait_owned(&gw).await;
    assert!(
        sink.start("gw", &gw, 1),
        "an owed piece goes out while draining"
    );
    let actor = Actor::new("gw", &sink, &[]);
    let mini = Arc::new(HomeGate::new(C, "mini"));
    let mut left = None;
    for _ in 0..3 {
        match drain_round(&pool, &gw, &actor).await {
            DrainStep::Left(row) => {
                left = Some(row);
                break;
            }
            DrainStep::Waiting(Blocker::PostsInFlight(1)) => {
                assert_eq!(gw.admit(|e| e), None, "closed while its last POST runs");
            }
            other => panic!("unexpected drain step: {other:?}"),
        }
        if let Some((HomeState::Released, None, released)) = row(&pool).await {
            applied(o_channel_homes::adopt(&pool, C, "mini", released).await);
            assert!(matches!(
                renew(&pool, &mini, released).await,
                LeaseRound::Renewed(_)
            ));
            sink.start("mini", &mini, 2);
        }
        assert!(owners(&[&gw, &mini]) <= 1);
        assert_eq!(sink.most_at_once(), 1, "POSTs overlapped");
        sink.end("gw");
    }
    let released = left.expect("the drain leaves once the POST ends").epoch;
    let gw_lease = tokio::time::timeout(Duration::from_secs(10), gw_lease).await;
    gw_lease
        .expect("the lease ends with the row")
        .expect("lease task");
    applied(o_channel_homes::adopt(&pool, C, "mini", released).await);
    let mini_lease = tokio::spawn(run_lease(pool.clone(), Arc::clone(&mini), released));
    wait_owned(&mini).await;
    assert!(sink.start("mini", &mini, 3));
    sink.end("mini");
    assert_eq!(sink.most_at_once(), 1);

    // Holder restart: the new process opens only through its own renewal at the row's epoch.
    mini_lease.abort();
    let gw = Arc::new(HomeGate::new(C, "gw"));
    let mini = Arc::new(HomeGate::new(C, "mini"));
    for node in [&gw, &mini] {
        let boot = channel_home::boot_home(&pool, node).await;
        let lease = boot.lease_epoch(node.holder());
        assert_eq!(lease, (node.holder() == "mini").then_some(released));
        assert_eq!(owners(&[&gw, &mini]), 0, "a read opens nothing");
    }
    assert!(matches!(
        renew(&pool, &gw, released).await,
        LeaseRound::Stale
    ));
    assert!(matches!(
        renew(&pool, &mini, released).await,
        LeaseRound::Renewed(_)
    ));
    assert_eq!(owners(&[&gw, &mini]), 1);

    // Reclaim with both restarting mid-drain: only the row's holder reopens, intake stays shut.
    applied(o_channel_homes::begin_reclaim(&pool, C, released, "gw").await);
    let (gw, mini) = (HomeGate::new(C, "gw"), HomeGate::new(C, "mini"));
    assert_eq!(owners(&[&gw, &mini]), 0);
    assert!(matches!(
        renew(&pool, &gw, released).await,
        LeaseRound::Stale
    ));
    assert!(matches!(
        renew(&pool, &mini, released).await,
        LeaseRound::Renewed(_)
    ));
    assert_eq!(owned(&mini), Some((released, HomeIntake::Closed)));
    let actor = Actor::new("mini", &sink, &[]);
    let DrainStep::Left(reclaimed) = drain_round(&pool, &mini, &actor).await else {
        panic!("the restarted holder finishes its drain");
    };
    // Target restart: the gateway booting fresh only reads; a reclaimed row opens nobody.
    let gw = HomeGate::new(C, "gw");
    assert!(matches!(
        renew(&pool, &gw, reclaimed.epoch).await,
        LeaseRound::Stale
    ));
    assert_eq!(owners(&[&gw, &mini]), 0);

    // Holder dies: past F it is forced out; the orphaned row opens nobody and drains nowhere.
    reset(&pool).await;
    let (dead, epoch) = worker_home(&pool).await;
    drop(dead);
    let age = "UPDATE o_channel_homes SET renewed_at = NOW() - INTERVAL '201 seconds'";
    sqlx::query(age).execute(&pool).await.expect("age lease");
    let force = o_channel_homes::force_orphan(&pool, C, epoch, ForceWindow::MIN, "op").await;
    let Ok(ForceOutcome::Orphaned(orphan)) = force else {
        panic!("expected orphaned: {force:?}");
    };
    let (gw, mini, standby) = (
        HomeGate::new(C, "gw"),
        HomeGate::new(C, "mini"),
        HomeGate::new(C, "gw-standby"),
    );
    for node in [&gw, &mini, &standby] {
        for at in [epoch, orphan.epoch] {
            assert!(matches!(renew(&pool, node, at).await, LeaseRound::Stale));
            let adopt = o_channel_homes::adopt(&pool, C, node.holder(), at).await;
            assert!(matches!(adopt, Ok(HomeWrite::Stale)), "{adopt:?}");
        }
        let step = drain_round(&pool, node, &Actor::new("gw", &sink, &[])).await;
        assert!(matches!(step, DrainStep::NotDraining), "{step:?}");
    }
    assert_eq!(owners(&[&gw, &mini, &standby]), 0);
    assert_eq!(
        row(&pool).await,
        Some((HomeState::Orphaned, None, orphan.epoch))
    );

    // The gateway role moving to a standby with another id opens nothing for it.
    reset(&pool).await;
    let epoch = delegated(&pool).await;
    let (gw, standby) = (HomeGate::new(C, "gw"), HomeGate::new(C, "gw-standby"));
    assert!(matches!(
        renew(&pool, &standby, epoch).await,
        LeaseRound::Stale
    ));
    assert!(matches!(
        renew(&pool, &gw, epoch).await,
        LeaseRound::Renewed(_)
    ));
    assert_eq!(owners(&[&gw, &standby]), 1);
    assert!(gw.expire_if_due(gw.expiry().expect("held") + HOLD_FOR));
    assert_eq!(
        owners(&[&gw, &standby]),
        0,
        "a lapsed lease leaves nobody holding"
    );
    pool.close().await;
    pg_db.drop().await;
}

/// The gateway ends a reclaim by dropping the reclaimed row; only with no row left does the
/// channel leave this process's registry and follow the gateway rules again.
#[tokio::test]
async fn a_reclaim_ends_with_the_row_gone_and_the_channel_on_the_gateway_rules_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (mini, epoch) = worker_home(&pool).await;
    let gw = Arc::new(HomeGate::new(C, "gw"));
    register(Arc::clone(&gw));
    applied(o_channel_homes::begin_reclaim(&pool, C, epoch, "gw").await);
    assert!(!finish_return(&pool, &gw, epoch).await.expect("return"));
    assert!(
        intake_hold(C, None).is_some(),
        "a registered gate still holds the channel"
    );

    let actor = Actor::new("mini", &Arc::new(Sink::default()), &[]);
    let DrainStep::Left(reclaimed) = drain_round(&pool, &mini, &actor).await else {
        panic!("the worker leaves");
    };
    assert!(
        finish_return(&pool, &gw, reclaimed.epoch)
            .await
            .expect("return")
    );
    assert_eq!(row(&pool).await, None);
    assert!(channel_home::registered(C).is_none() && gw.withdrawn());
    assert_eq!(intake_hold(C, None), None, "the gateway rules again");
    pool.close().await;
    pg_db.drop().await;
}

impl DrainPort for Arc<Actor> {
    async fn turn_running(&self) -> Option<bool> {
        self.as_ref().turn_running().await
    }

    async fn owed(&self) -> Option<Owed> {
        self.as_ref().owed().await
    }

    async fn posts_in_flight(&self) -> Option<usize> {
        self.as_ref().posts_in_flight().await
    }

    async fn reset_legacy_source(&self) -> Result<(), ResetRefused> {
        self.as_ref().reset_legacy_source().await
    }
}

/// The drain loop shows what it waits on in health while it waits, and nothing once it leaves.
#[tokio::test]
async fn the_drain_loop_shows_its_blocker_in_health_until_it_leaves_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (mini, epoch) = worker_home(&pool).await;
    applied(o_channel_homes::begin_reclaim(&pool, C, epoch, "gw").await);
    register(Arc::clone(&mini));
    let actor = Arc::new(Actor::new("mini", &Arc::new(Sink::default()), &[]));
    actor.unreadable.store(true, Ordering::SeqCst);
    let drain = tokio::spawn(run_drain(
        pool.clone(),
        Arc::clone(&mini),
        Arc::clone(&actor),
    ));
    let waiting = json!([{"channel": C, "blocker": "owed_unreadable"}]);
    for _ in 0..250 {
        if channel_home::health().is_some_and(|health| health["home_draining"] == waiting) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let health = channel_home::health().expect("registered");
    assert_eq!(
        (&health["home_draining"], &health["homes"][0]["home"]),
        (&waiting, &json!("draining"))
    );
    let reclaiming = Some((HomeState::Reclaiming, Some("mini".to_string()), epoch));
    assert_eq!(row(&pool).await, reclaiming);

    actor.unreadable.store(false, Ordering::SeqCst);
    let left = tokio::time::timeout(RENEW_EVERY * 3, drain).await;
    let left = left
        .expect("the drain ends")
        .expect("drain task")
        .expect("left");
    assert_eq!(left.state, HomeState::Reclaimed);
    let health = channel_home::health().expect("registered");
    assert_eq!(
        (&health["home_draining"], &health["homes"][0]["home"]),
        (&json!([]), &json!("lost"))
    );
    channel_home::unregister(C);
    pool.close().await;
    pg_db.drop().await;
}

async fn ddl(pool: &PgPool, statement: &str) {
    sqlx::query(statement).execute(pool).await.expect(statement);
}

/// A home row or intake that cannot be read, and a leaving write that fails, each keep the drain
/// waiting with the row and gate as they were; once the write lands the drain leaves.
#[tokio::test]
async fn an_unreadable_row_or_intake_or_a_failed_leave_keeps_the_row_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (mini, epoch) = worker_home(&pool).await;
    applied(o_channel_homes::begin_reclaim(&pool, C, epoch, "gw").await);
    let actor = Actor::new("mini", &Arc::new(Sink::default()), &[]);
    let reclaiming = Some((HomeState::Reclaiming, Some("mini".to_string()), epoch));

    ddl(&pool, "ALTER TABLE o_channel_homes RENAME TO homes_away").await;
    let step = drain_round(&pool, &mini, &actor).await;
    assert!(
        matches!(step, DrainStep::Waiting(Blocker::RowUnreadable)),
        "{step:?}"
    );
    assert_eq!(
        owned(&mini),
        Some((epoch, HomeIntake::Open)),
        "nothing closed"
    );
    ddl(&pool, "ALTER TABLE homes_away RENAME TO o_channel_homes").await;

    ddl(&pool, "ALTER TABLE intake_outbox RENAME TO intake_away").await;
    let step = drain_round(&pool, &mini, &actor).await;
    assert!(
        matches!(step, DrainStep::Waiting(Blocker::IntakeUnreadable)),
        "{step:?}"
    );
    assert_eq!(row(&pool).await, reclaiming);
    assert_eq!(
        owned(&mini),
        Some((epoch, HomeIntake::Closed)),
        "not finally closed"
    );
    ddl(&pool, "ALTER TABLE intake_away RENAME TO intake_outbox").await;

    ddl(
        &pool,
        "CREATE FUNCTION refuse_leave() RETURNS trigger LANGUAGE plpgsql AS
         $$ BEGIN RAISE EXCEPTION 'leave refused'; END $$",
    )
    .await;
    ddl(
        &pool,
        "CREATE TRIGGER refuse_leave BEFORE UPDATE ON o_channel_homes FOR EACH ROW
         WHEN (NEW.state = 'reclaimed') EXECUTE FUNCTION refuse_leave()",
    )
    .await;
    let step = drain_round(&pool, &mini, &actor).await;
    assert!(
        matches!(step, DrainStep::Waiting(Blocker::LeaveFailed)),
        "{step:?}"
    );
    assert_eq!(row(&pool).await, reclaiming);
    assert_eq!(
        (owned(&mini), mini.admit(|e| e)),
        (None, None),
        "stays closed"
    );
    ddl(&pool, "DROP TRIGGER refuse_leave ON o_channel_homes").await;
    let step = drain_round(&pool, &mini, &actor).await;
    let DrainStep::Left(left) = step else {
        panic!("expected left: {step:?}");
    };
    assert_eq!(left.state, HomeState::Reclaimed);
    pool.close().await;
    pg_db.drop().await;
}

/// The gateway's cleanup of a gate that was replaced deletes the reclaimed row but leaves the
/// gate that replaced it registered and open; the current gate's cleanup unregisters it.
#[tokio::test]
async fn a_replaced_gates_cleanup_never_unregisters_the_gate_that_replaced_it_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (mini, epoch) = worker_home(&pool).await;
    applied(o_channel_homes::begin_reclaim(&pool, C, epoch, "gw").await);
    let reclaimed = o_channel_homes::finish_reclaim(&pool, C, mini.holder(), epoch).await;
    let returned = applied(reclaimed).epoch;
    let old = Arc::new(HomeGate::new(C, "gw"));
    register(Arc::clone(&old));
    let new = Arc::new(HomeGate::new(C, "gw"));
    register(Arc::clone(&new));
    assert!(old.withdrawn() && !new.withdrawn());

    let removed = finish_return(&pool, &old, returned).await.expect("cleanup");
    assert!(!removed, "the replaced gate unregisters nothing");
    assert_eq!(row(&pool).await, None, "the reclaimed row is gone");
    let current = channel_home::registered(C).expect("still registered");
    assert!(Arc::ptr_eq(&current, &new) && !new.withdrawn());

    assert!(finish_return(&pool, &new, returned).await.expect("cleanup"));
    assert!(channel_home::registered(C).is_none() && new.withdrawn());
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn command_drain_waits_before_reset_and_after_final_close_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let epoch = delegated(&pool).await;
    let home = Arc::new(HomeGate::new(C, "gw"));
    renew(&pool, &home, epoch).await;
    register(home.clone());
    let sink = Arc::new(Sink::default());
    let actor = Actor::new("gw", &sink, &[]);
    let permit = home.admit_recovery("claude").unwrap();
    let step = drain_round(&pool, &home, &actor).await;
    assert!(
        matches!(step, DrainStep::Waiting(Blocker::CommandsInFlight(1))),
        "{step:?}"
    );
    assert_eq!(actor.resets.load(Ordering::SeqCst), 0);
    assert!(!home.final_closed(epoch));
    assert_eq!(row(&pool).await.unwrap().0, HomeState::Releasing);
    home.close();
    let step = drain_round(&pool, &home, &actor).await;
    assert!(
        matches!(step, DrainStep::Waiting(Blocker::CommandsInFlight(1))),
        "final-closed reentry: {step:?}"
    );
    drop(permit);
    let step = drain_round(&pool, &home, &actor).await;
    assert!(matches!(step, DrainStep::Left(_)), "{step:?}");
    assert_eq!(row(&pool).await.unwrap().0, HomeState::Released);
    channel_home::unregister(C);
    pool.close().await;
    pg_db.drop().await;
}

struct RecoveryAtReset {
    actor: Actor,
    home: Arc<HomeGate>,
    permit: Mutex<Option<channel_home::CommandPermit>>,
}
impl DrainPort for RecoveryAtReset {
    async fn turn_running(&self) -> Option<bool> {
        self.actor.turn_running().await
    }
    async fn owed(&self) -> Option<Owed> {
        self.actor.owed().await
    }
    async fn posts_in_flight(&self) -> Option<usize> {
        self.actor.posts_in_flight().await
    }
    async fn reset_legacy_source(&self) -> Result<(), ResetRefused> {
        *self.permit.lock().unwrap() = self.home.admit_recovery("claude");
        assert!(
            self.permit.lock().unwrap().is_some(),
            "recovery won before final close"
        );
        self.actor.reset_legacy_source().await
    }
}

#[tokio::test(flavor = "current_thread")]
async fn command_drain_rechecks_recovery_admitted_during_reset_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let epoch = delegated(&pool).await;
    let home = Arc::new(HomeGate::new(C, "gw"));
    renew(&pool, &home, epoch).await;
    let sink = Arc::new(Sink::default());
    let port = RecoveryAtReset {
        actor: Actor::new("gw", &sink, &[]),
        home: home.clone(),
        permit: Mutex::default(),
    };
    let step = drain_round(&pool, &home, &port).await;
    assert!(
        matches!(step, DrainStep::Waiting(Blocker::CommandsInFlight(1))),
        "{step:?}"
    );
    assert!(home.final_closed(epoch));
    assert!(
        home.admit_recovery("claude").is_none(),
        "close wins against a later release"
    );
    assert_eq!(port.actor.resets.load(Ordering::SeqCst), 1);
    drop(port.permit.lock().unwrap().take());
    assert!(matches!(
        drain_round(&pool, &home, &port).await,
        DrainStep::Left(_)
    ));
    pool.close().await;
    pg_db.drop().await;
}
