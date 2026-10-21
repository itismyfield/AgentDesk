//! Native heartbeat row contention must leave the fixture's transaction runtime runnable.
use super::*;

const TEST_NAME: &str = concat!(
    module_path!(),
    "::a2_watcher_runtime_row_lock_allows_native_heartbeat_and_commit_pg"
);

fn observe_heartbeat_lock(
    options: sqlx::postgres::PgConnectOptions,
    holder: i32,
    entered: tokio::sync::oneshot::Sender<i32>,
    completed: std::sync::mpsc::Receiver<()>,
    socket: std::path::PathBuf,
) -> std::thread::JoinHandle<()> {
    let name = TEST_NAME
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap_or(TEST_NAME);
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("observer runtime");
                let (pool, waiter) = runtime.block_on(async move {
                    let pool = sqlx::postgres::PgPoolOptions::new()
                        .max_connections(1)
                        .connect_with(options)
                        .await
                        .expect("observer owned PG connection");
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
                    loop {
                        let waiting: Vec<(i32, bool)> = sqlx::query_as(
                            "SELECT a.pid, EXISTS (SELECT 1 FROM pg_locks l WHERE l.pid=a.pid AND l.relation='sessions'::regclass AND l.locktype='relation' AND l.mode='RowExclusiveLock') FROM pg_stat_activity a WHERE a.datname=current_database() AND $1=ANY(pg_blocking_pids(a.pid)) AND a.wait_event_type='Lock' AND a.pid<>$1",
                        )
                        .bind(holder)
                        .fetch_all(&pool)
                        .await
                        .expect("observe native heartbeat lock metadata");
                        assert!(waiting.len() <= 1, "R1 prerequisite: ambiguous blocked writer");
                        if let Some((pid, writer)) = waiting.first() {
                            assert!(*writer, "R1 prerequisite: blocked backend must write sessions");
                            break (pool, *pid);
                        }
                        assert!(
                            tokio::time::Instant::now() < deadline,
                            "R1 prerequisite: native heartbeat never waited on the exact seed lock"
                        );
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                });
                eprintln!(
                    "A2_R12_HEARTBEAT_BLOCKED holder_pid={holder} waiter_pid={waiter} relation=sessions"
                );
                entered.send(waiter).expect("owner awaits observed heartbeat");
                // This OS deadline remains live even when the owner Tokio thread is blocked.
                let finished = match completed.recv_timeout(Duration::from_secs(30)) {
                    Ok(()) => true,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        panic!("R1 prerequisite: owner proof exited before native completion")
                    }
                };
                assert!(
                    finished,
                    "R1: the current-thread transaction must commit and join its native watcher after confirmed heartbeat contention"
                );
                runtime.block_on(pool.close());
            }));
            if result.is_err() {
                let _ = crate::services::platform::binary_resolver::runtime_command("tmux")
                    .unwrap().arg("-S").arg(socket).arg("kill-server").output();
                std::process::exit(101);
            }
        })
        .expect("native heartbeat observer OS thread")
}

#[tokio::test(flavor = "current_thread")]
async fn a2_watcher_runtime_row_lock_allows_native_heartbeat_and_commit_pg() {
    if !isolated(TEST_NAME) {
        return;
    }
    let fixture = Fixture::connect_and_migrate_fixture(69_184_151, ProviderKind::Claude).await;
    let observation =
        inflight::o_seed_observation::Guard::new(&fixture.provider, fixture.rig.channel.get());
    let native = fixture.prepare_old_watcher().await;
    let name = fixture.rig.shared.core.lock().await.sessions[&fixture.rig.channel]
        .channel_name
        .clone()
        .unwrap();
    let tmux = fixture.provider.build_tmux_session_name(&name);
    let key = adk_session::build_namespaced_session_key(
        &fixture.rig.shared.token_hash,
        &fixture.provider,
        &tmux,
    );
    let cancel = fixture
        .rig
        .shared
        .tmux_watchers
        .get(&fixture.rig.channel)
        .expect("prepared native watcher registration")
        .cancel
        .clone();
    let pool = fixture.rig.shared.pg_pool.as_ref().unwrap();
    let options = (*pool.connect_options()).clone();
    let mut transaction = pool.begin().await.expect("seed lock transaction");
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    let before: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_heartbeat FROM sessions WHERE session_key=$1 FOR UPDATE")
            .bind(&key)
            .fetch_one(&mut *transaction)
            .await
            .expect("lock exactly the prepared seed row");
    eprintln!("A2_R12_SEED_LOCKED holder_pid={holder}");
    let (entered, blocked) = tokio::sync::oneshot::channel();
    let (completed, completion) = std::sync::mpsc::channel();
    let observer = observe_heartbeat_lock(
        options,
        holder,
        entered,
        completion,
        fixture.host.socket.clone(),
    );
    let watcher = fixture.spawn_watcher(native);
    let waiter = blocked
        .await
        .expect("native heartbeat contention prerequisite");
    assert_ne!(holder, waiter);
    assert!(observation.snapshot().watcher_initialization_observed);
    assert!(
        !watcher.is_finished(),
        "native watcher remains live behind its heartbeat"
    );
    transaction
        .commit()
        .await
        .expect("owner runtime commits while watcher waits");
    eprintln!("A2_R12_COMMITTED holder_pid={holder}");
    until(async || {
        let after: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT last_heartbeat FROM sessions WHERE session_key=$1")
                .bind(&key)
                .fetch_one(pool)
                .await
                .unwrap();
        after.is_some_and(|at| before.is_none_or(|old| at > old))
    })
    .await;
    eprintln!("A2_R12_HEARTBEAT_ADVANCED native=1");
    cancel.store(true, Ordering::SeqCst);
    tokio::time::timeout(DRAIN_LIMIT, watcher)
        .await
        .expect("native watcher exits after its real cancel flag")
        .expect("native watcher task joins");
    let blocked_after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity a WHERE a.datname=current_database() AND $1=ANY(pg_blocking_pids(a.pid))",
    )
    .bind(holder)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        blocked_after, 0,
        "committed seed lock must leave no blocked backend"
    );
    completed.send(()).expect("stop the OS heartbeat watchdog");
    observer.join().expect("heartbeat observer OS thread joins");
    eprintln!(
        "A2_R12_HEARTBEAT_COMMITTED native_heartbeat_advanced=1 watcher_joined=1 blocked_backends=0"
    );
}
