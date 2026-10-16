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
        writers: None,
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
/// a candidate with none is watched without a gate, and unreadable rows hold the candidates.
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
    assert!(
        !channel_home::any_registered(),
        "a candidate with no row mounts no gate"
    );
    assert!(running(C).0, "its watch follows the row");
    stop(C).await;
    let candidate = ports.load(Ordering::SeqCst);
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
        candidate,
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

/// A releasing holder whose drain is held keeps its gate and lease until abort-release drops its
/// row; then its watch unregisters the gate, ends the lease and claims hold no more.
#[tokio::test]
async fn a_releasing_holder_whose_row_is_aborted_returns_the_channel_to_the_gateway_rules_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let epoch = applied(o_channel_homes::delegate(&pool, C, "claude", "gw", "mini").await).epoch;
    let (ports, resets) = (Arc::default(), Arc::default());
    let refused = counted(&resets, Some("kept session"));
    let gw = boot(&pool, "gw", rows(&pool).await, vec![], refused, &ports);
    assert_eq!(start(Some(true), || gw).await.len(), 1);
    let blocker = || {
        let health = channel_home::health();
        health.is_some_and(|health| health["home_draining"][0]["blocker"] == "source_reset")
    };
    assert!(eventually(12, blocker).await);
    assert_eq!(running(C), (true, Some(epoch)), "the holder renews");
    assert!(channel_home::refusal(ID).is_some() && intake_hold(C, None).is_some());

    let aborted = o_channel_homes::abort_release(&pool, C, epoch, "gw").await;
    assert_eq!(applied(aborted).state, HomeState::Releasing);
    assert!(eventually(8, || channel_home::registered(C).is_none()).await);
    assert!(eventually(2, || running(C) == (false, None)).await);
    assert_eq!(
        (channel_home::refusal(ID), intake_hold(C, None)),
        (None, None)
    );
    assert_eq!(o_channel_homes::read_home(&pool, C).await.unwrap(), None);
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

/// The target's boot through the production row reader, its port carrying `restored`.
fn target_boot(
    pool: &PgPool,
    restored: &Arc<AtomicBool>,
) -> Option<Boot<impl Fn(u64) -> BootPort<Idle>>> {
    let (restored, reset) = (Arc::clone(restored), counted(&Arc::default(), None));
    let port = move |_| BootPort::new(Idle, Arc::clone(&reset)).restored_by(Arc::clone(&restored));
    Some(Boot {
        provider: "claude".into(),
        local: "mini".into(),
        pool: pool.clone(),
        rows: listed(pool),
        candidates: vec![],
        port,
        writers: None,
    })
}

/// Waits up to `secs` for the channel's row to be in `state`, polling every 20ms.
async fn row_in(pool: &PgPool, secs: u64, state: HomeState) -> ChannelHome {
    for _ in 0..secs * 50 {
        let row = o_channel_homes::read_home(pool, C)
            .await
            .unwrap()
            .expect("row");
        if row.state == state {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    o_channel_homes::read_home(pool, C)
        .await
        .unwrap()
        .expect("row")
}

/// From the operator's delegate through the gateway's drain, the released channel's target adopts
/// it and its own lease opens the gate, so the channel no longer stays released and held.
#[tokio::test]
async fn a_delegated_channel_is_adopted_by_its_target_after_the_gateway_drain_pg() {
    use crate::cli::channel_home::{ChannelHomeCommand, execute};
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let mut config = crate::config::Config::default();
    config.runtime.channel_home_delegation_enabled = Some(true);
    config.cluster.instance_id = Some("gw".into());
    let delegate = ChannelHomeCommand::Delegate {
        channel: ID,
        provider: "claude".into(),
        to: "mini".into(),
    };
    let connect = || async {
        let url = &pg_db.database_url;
        crate::db::postgres::connect_test_pool_with_max_connections(url, "channel-home cli", 2)
            .await
    };
    execute(&config, delegate, connect).await.expect("delegate");

    let (ports, resets) = (Arc::default(), Arc::default());
    let gw = boot(
        &pool,
        "gw",
        rows(&pool).await,
        vec![],
        counted(&resets, None),
        &ports,
    );
    start(Some(true), || gw).await;
    let released = row_in(&pool, 15, HomeState::Released).await;
    assert_eq!(
        (released.state, released.holder, released.target.as_deref()),
        (HomeState::Released, None, Some("mini")),
        "the gateway drain leaves the channel released to mini"
    );
    assert!(eventually(8, || channel_home::registered(C).is_none()).await);
    stop(C).await;

    let restored = Arc::new(AtomicBool::new(true));
    start(Some(true), || target_boot(&pool, &restored)).await;
    let adopted = row_in(&pool, 8, HomeState::Worker).await;
    assert_eq!(
        (adopted.state, adopted.holder.as_deref(), adopted.epoch),
        (HomeState::Worker, Some("mini"), released.epoch),
        "the target adopts the released channel instead of holding it forever"
    );
    let epoch = released.epoch;
    assert!(eventually(5, || owned(C) == Some((epoch, HomeIntake::Open))).await);
    assert_eq!(running(C), (true, Some(epoch)));
    stop(C).await;
    channel_home::unregister(C);
    pool.close().await;
    pg_db.drop().await;
}

/// A target whose runtime has not restored its turns leaves the released row alone, and adopts it
/// once the flag turns true.
#[tokio::test]
async fn an_unrestored_target_adopts_a_released_channel_only_once_restored_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let delegated = applied(o_channel_homes::delegate(&pool, C, "claude", "gw", "mini").await);
    let released = o_channel_homes::finish_release(&pool, C, "gw", delegated.epoch).await;
    let released = applied(released);
    let restored = Arc::new(AtomicBool::new(false));
    start(Some(true), || target_boot(&pool, &restored)).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let waiting = o_channel_homes::read_home(&pool, C).await.unwrap();
    assert_eq!(waiting, Some(released.clone()), "unrestored: row untouched");
    assert_eq!((running(C), owned(C)), ((true, None), None));

    restored.store(true, Ordering::Release);
    let adopted = row_in(&pool, 8, HomeState::Worker).await;
    assert_eq!(
        (adopted.holder.as_deref(), adopted.epoch),
        (Some("mini"), released.epoch)
    );
    assert!(eventually(5, || owned(C).is_some()).await);
    stop(C).await;
    channel_home::unregister(C);
    pool.close().await;
    pg_db.drop().await;
}

/// A gateway runtime's return of a reclaimed channel to its own writer, through the real writer host.
mod writer_return {
    use std::path::PathBuf;

    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::cutover::test_override;
    use crate::services::tui_o::shadow::{ShadowProvider, binding_reader::source_id_for};
    use crate::services::tui_o::writer::WriterAlarm;
    use crate::services::tui_o::writer::activation::test_hook::{self, Step};
    use crate::services::tui_o::writer::host::{self, HostParts, test_io::TestHost};

    const WC: &str = "1490141479707086947";
    const WID: u64 = 1_490_141_479_707_086_947;

    /// A leased gateway with supervised writers; `starts` records, per later writer start, whether
    /// another writer still hosted the channel and whether a home gate was still registered.
    struct Gateway {
        _runtime: tempfile::TempDir,
        transcript: PathBuf,
        io: Arc<TestHost>,
        ready: Arc<Readiness>,
        writers: Arc<ChannelWriters>,
        starts: Arc<Mutex<Vec<(bool, bool)>>>,
    }

    impl Gateway {
        async fn boot(pool: &PgPool, rows: Result<Vec<ChannelHome>, HomeError>) -> Self {
            let runtime = tempfile::tempdir().unwrap();
            let transcript = runtime.path().join("o.jsonl");
            std::fs::write(&transcript, b"").unwrap();
            let io = TestHost::new([(WID, source_id_for("s1", &transcript).unwrap())]);
            let (gate, ready) = (Arc::new(OwnershipGate::default()), Arc::default());
            gate.acquired();
            let writers = ChannelWriters::new(Arc::clone(&gate), Arc::clone(&ready));
            let writers = Arc::new(writers);
            let reset = counted(&Arc::default(), None);
            let ports = Arc::default();
            let mut boot = boot(pool, "gw", rows, vec![WID], reset, &ports).unwrap();
            boot.writers = Some(Arc::clone(&writers));
            start(Some(true), || Some(boot)).await;
            let (root, parts_io, parts_ready) = (runtime.path().to_path_buf(), &io, &ready);
            let (parts_io, parts_ready) = (Arc::clone(parts_io), Arc::clone(parts_ready));
            let parts = move || HostParts {
                io: Arc::clone(&parts_io),
                runtime_root: Some(root.clone()),
                gate: Arc::clone(&gate),
                readiness: Arc::clone(&parts_ready),
            };
            let owned = cutover::boot_ownership();
            let started = host::start_managed(ShadowProvider::Claude, true, owned, parts.clone());
            let starts: Arc<Mutex<Vec<(bool, bool)>>> = Arc::default();
            let (log, hosted) = (Arc::clone(&starts), Arc::clone(&ready));
            let start: StartWriter = Arc::new(move |channel| {
                let homed = channel_home::registered_channel(channel).is_some();
                log.lock().unwrap().push((hosted.is_hosted(channel), homed));
                let owned = cutover::boot_ownership().into_iter();
                let owned = owned.filter(|(owned, ..)| *owned == channel).collect();
                host::start_managed(ShadowProvider::Claude, true, owned, parts.clone()).pop()
            });
            writers.arm(Some(start), started);
            Self {
                _runtime: runtime,
                transcript,
                io,
                ready,
                writers,
                starts,
            }
        }

        fn post(&self, id: &str, text: &str) {
            use std::io::Write;
            let row = serde_json::json!({
                "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
                "message": {"id": id, "content": [{"type": "text", "text": text}]},
            });
            let mut line = serde_json::to_vec(&row).unwrap();
            line.push(b'\n');
            let mut options = std::fs::OpenOptions::new();
            let mut file = options.append(true).open(&self.transcript).unwrap();
            file.write_all(&line).unwrap();
        }

        fn halted(&self) -> bool {
            let raised = self.io.alarms.0.lock().unwrap();
            let halted = |(channel, alarm): &(u64, WriterAlarm)| {
                *channel == WID && matches!(alarm, WriterAlarm::Halted { .. })
            };
            raised.iter().any(halted)
        }

        async fn end(self) {
            self.writers.stop(WID).await;
            stop(WC).await;
            channel_home::unregister(WC);
        }
    }

    /// The row a gateway named `gw` is the reclaim's target of, its worker mini drained.
    async fn reclaimed(pool: &PgPool) {
        let row = applied(o_channel_homes::delegate(pool, WC, "claude", "gw", "mini").await);
        let row = applied(o_channel_homes::finish_release(pool, WC, "gw", row.epoch).await);
        let epoch = applied(o_channel_homes::adopt(pool, WC, "mini", row.epoch).await).epoch;
        applied(o_channel_homes::begin_reclaim(pool, WC, epoch, "gw").await);
        applied(o_channel_homes::finish_reclaim(pool, WC, "mini", epoch).await);
    }

    /// A gateway booting as a reclaimed row's target ends the home's writer before the row goes and
    /// only then starts one writer under its own lease, which takes work and posts.
    #[tokio::test]
    async fn a_reclaimed_boot_returns_the_channel_to_one_gateway_writer_pg() {
        let _hosts = crate::config::session_hosts::force_for_test(Some("gw"), &[(WID, "gw")]);
        let _selected = test_override::force_channels(&[(WID, ClaudeTui)]);
        let _pending = test_override::force_candidates(&[(WID, ClaudeTui)]);
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        reclaimed(&pool).await;
        let gw = Gateway::boot(&pool, rows(&pool).await).await;
        assert!(
            eventually(20, || gw.ready.accepts(WID)).await,
            "the returned channel's writer takes work"
        );
        assert_eq!(o_channel_homes::read_home(&pool, WC).await.unwrap(), None);
        assert!(channel_home::registered(WC).is_none());
        assert_eq!(
            *gw.starts.lock().unwrap(),
            [(false, false)],
            "one start, after the home's writer ended and its gate went"
        );
        gw.post("r1", "returned");
        assert!(eventually(10, || gw.io.posts.to(WID) == ["returned"]).await);
        gw.end().await;
        pool.close().await;
        pg_db.drop().await;
    }

    /// A returned writer whose host ends after its start, before its actor runs, is started again
    /// by the same watch's next round rather than counted as running.
    #[tokio::test]
    async fn a_returned_writer_that_ends_after_its_start_is_started_again_pg() {
        let _hosts = crate::config::session_hosts::force_for_test(Some("gw"), &[(WID, "gw")]);
        let _selected = test_override::force_channels(&[(WID, ClaudeTui)]);
        let _pending = test_override::force_candidates(&[(WID, ClaudeTui)]);
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        reclaimed(&pool).await;
        // The first gateway writer's activation fails once after its handle was returned.
        test_hook::set(WID, Step::BeforeLock, || {
            Err("injected start failure".into())
        });
        let gw = Gateway::boot(&pool, rows(&pool).await).await;
        assert!(
            eventually(20, || gw.halted()).await,
            "its first activation ends"
        );
        assert!(eventually(5, || !gw.ready.is_hosted(WID)).await);
        assert!(
            eventually(20, || gw.ready.accepts(WID)).await,
            "the same watch starts it again"
        );
        assert!(gw.starts.lock().unwrap().len() >= 2);
        gw.post("r1", "restarted");
        assert!(eventually(10, || gw.io.posts.to(WID) == ["restarted"]).await);
        gw.end().await;
        pool.close().await;
        pg_db.drop().await;
    }

    /// Waits until PG reports a backend queued on a lock that backend `holder` keeps.
    async fn queued_behind(pool: &PgPool, holder: i32) {
        for _ in 0..400 {
            let queued: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                  WHERE wait_event_type = 'Lock' AND $1 = ANY(pg_blocking_pids(pid)))",
            )
            .bind(holder)
            .fetch_one(pool)
            .await
            .unwrap();
            if queued {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the return never waited on the row lock");
    }

    /// A return whose CAS goes stale (row removed and delegated again while it waited) removes
    /// nothing and starts no gateway writer; the new delegation drains to its target.
    #[tokio::test]
    async fn a_stale_return_keeps_the_new_delegation_and_starts_no_writer_pg() {
        let _hosts = crate::config::session_hosts::force_for_test(Some("gw"), &[(WID, "gw")]);
        let _selected = test_override::force_channels(&[(WID, ClaudeTui)]);
        let _pending = test_override::force_candidates(&[(WID, ClaudeTui)]);
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(6).await;
        reclaimed(&pool).await;
        let mut racer = pool.begin().await.unwrap();
        let racer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *racer)
            .await
            .unwrap();
        sqlx::query("SELECT 1 FROM o_channel_homes WHERE channel_id = $1 FOR UPDATE")
            .bind(WC)
            .execute(&mut *racer)
            .await
            .unwrap();
        let gw = Gateway::boot(&pool, rows(&pool).await).await;
        queued_behind(&pool, racer_pid).await;
        // Another return of the row and the operator's next delegate, committed while it waits.
        sqlx::query("DELETE FROM o_channel_homes WHERE channel_id = $1")
            .bind(WC)
            .execute(&mut *racer)
            .await
            .unwrap();
        let redelegated: i64 = sqlx::query_scalar(
            "INSERT INTO o_channel_homes (channel_id, provider, state, holder, target, epoch, renewed_at)
             VALUES ($1, 'claude', 'releasing', 'gw', 'mini', nextval('o_channel_home_epochs'), NOW())
             RETURNING epoch",
        )
        .bind(WC)
        .fetch_one(&mut *racer)
        .await
        .unwrap();
        racer.commit().await.unwrap();

        let released = || async {
            let row = o_channel_homes::read_home(&pool, WC).await.unwrap();
            row.is_some_and(|row| row.state == HomeState::Released)
        };
        let mut drained = false;
        for _ in 0..1500 {
            if released().await {
                drained = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(drained, "the new delegation drains to its target");
        let row = o_channel_homes::read_home(&pool, WC)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (row.holder, row.target.as_deref()),
            (None, Some("mini")),
            "the stale return removed nothing"
        );
        assert!(row.epoch > redelegated);
        assert!(eventually(20, || channel_home::registered(WC).is_none()).await);
        assert_eq!(
            *gw.starts.lock().unwrap(),
            [],
            "the stale return started no gateway writer"
        );
        assert!(!gw.ready.is_hosted(WID));
        gw.end().await;
        pool.close().await;
        pg_db.drop().await;
    }

    /// A return to a gateway that holds no writer candidate for the channel, only a standby store
    /// off its O home: the row goes and the gate with it, but readiness stays closed.
    #[tokio::test]
    async fn a_return_without_a_writer_candidate_keeps_readiness_closed_pg() {
        use crate::services::tui_o::channel_policy::Adoption;
        let _hosts = crate::config::session_hosts::force_for_test(Some("gw"), &[(WID, "gw")]);
        let _standby = test_override::force_standby(&[(WID, ClaudeTui)], "mini", Adoption::Pending);
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        reclaimed(&pool).await;
        let gw = Gateway::boot(&pool, rows(&pool).await).await;
        let mut returned = false;
        for _ in 0..1000 {
            if o_channel_homes::read_home(&pool, WC)
                .await
                .unwrap()
                .is_none()
            {
                returned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(returned, "the return removes the reclaimed row");
        assert!(eventually(5, || channel_home::registered(WC).is_none()).await);
        // The return's start and the next round's retry, unless a writer got hosted first.
        let tried = || gw.starts.lock().unwrap().len() >= 2 || gw.ready.is_hosted(WID);
        assert!(eventually(20, tried).await, "the watch never tried a start");
        let open = (gw.ready.is_hosted(WID), gw.ready.accepts(WID));
        assert_eq!(open, (false, false), "readiness opened without a candidate");
        gw.end().await;
        pool.close().await;
        pg_db.drop().await;
    }

    /// An unreadable row is not its absence: the reclaimed channel's gate stays and no gateway
    /// writer starts.
    #[tokio::test]
    async fn an_unreadable_row_returns_nothing_and_starts_no_writer() {
        let _hosts = crate::config::session_hosts::force_for_test(Some("gw"), &[(WID, "gw")]);
        let _selected = test_override::force_channels(&[(WID, ClaudeTui)]);
        let _pending = test_override::force_candidates(&[(WID, ClaudeTui)]);
        let mut reclaimed = row(WC, "claude", None, Some("gw"));
        reclaimed.state = HomeState::Reclaimed;
        let pool = lazy_pool();
        let gw = Gateway::boot(&pool, Ok(vec![reclaimed])).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(channel_home::registered(WC).is_some(), "the gate stays");
        assert_eq!(*gw.starts.lock().unwrap(), [], "no writer starts");
        assert!(!gw.ready.accepts(WID));
        gw.end().await;
    }

    /// Boot rows that cannot be read are not their absence: the runtime's first spawn starts no
    /// writer, neither for the held candidate nor for a channel no watch follows.
    #[tokio::test]
    async fn unreadable_boot_rows_start_no_first_writer() {
        const OTHER: u64 = 1_490_141_479_707_086_948;
        let _hosts = crate::config::session_hosts::force_for_test(Some("gw"), &[(WID, "gw")]);
        let channels = [(WID, ClaudeTui), (OTHER, ClaudeTui)];
        let _selected = test_override::force_channels(&channels);
        let _pending = test_override::force_candidates(&channels);
        let runtime = tempfile::tempdir().unwrap();
        let (gate, ready) = (
            Arc::new(OwnershipGate::default()),
            Arc::new(Readiness::default()),
        );
        gate.acquired();
        let writers = Arc::new(ChannelWriters::new(Arc::clone(&gate), Arc::clone(&ready)));
        let unread = Err(HomeError::Db(sqlx::Error::PoolTimedOut));
        let (pool, reset, ports) = (lazy_pool(), counted(&Arc::default(), None), Arc::default());
        let mut boot = boot(&pool, "gw", unread, vec![WID], reset, &ports).unwrap();
        boot.writers = Some(Arc::clone(&writers));
        start(Some(true), || Some(boot)).await;
        let (io, root) = (
            TestHost::new(std::iter::empty()),
            runtime.path().to_path_buf(),
        );
        let parts_ready = Arc::clone(&ready);
        writers.start_boot(ShadowProvider::Claude, true, move || HostParts {
            io: Arc::clone(&io),
            runtime_root: Some(root.clone()),
            gate: Arc::clone(&gate),
            readiness: Arc::clone(&parts_ready),
        });
        let hosted = (ready.is_hosted(WID), ready.is_hosted(OTHER));
        let phases = (writers.phase(WID), writers.phase(OTHER));
        assert_eq!(
            (hosted, phases),
            ((false, false), (None, None)),
            "no first writer"
        );
        stop(WC).await;
        channel_home::unregister(WC);
    }
}
