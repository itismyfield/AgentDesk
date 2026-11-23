//! A held native boot restore is retried instead of dropping the Claude runtime, and a
//! SIGTERM during the gate stops it before any runtime starts.
use super::{NativeBoot, await_native_boot};
use crate::services::discord::SharedData;
use crate::services::discord::health::boot_hold;
use crate::services::provider::ProviderKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::watch;

const BUSY: &str = "native control history busy";
const TICK: Duration = Duration::from_millis(1);

/// A runtime whose process-wide shutdown barrier starts with `slots` slots.
fn barrier(slots: usize) -> Arc<SharedData> {
    crate::services::discord::make_shared_data_for_tests_with_barrier(
        None,
        Arc::new(AtomicUsize::new(slots)),
    )
}

fn remaining(shared: &SharedData) -> usize {
    shared.restart.shutdown_remaining.load(Ordering::Acquire)
}

fn stopped(shared: &SharedData) -> bool {
    shared.restart.shutting_down.load(Ordering::Acquire)
        && shared.restart.shutdown_counted.load(Ordering::Acquire)
}

fn claude_reasons() -> Vec<String> {
    boot_hold::health_reasons()
        .into_iter()
        .filter(|reason| reason.starts_with("provider:claude:"))
        .collect()
}

/// Fails `busy` times with the observed restore error, then succeeds.
async fn run_busy_then_ready(
    busy: usize,
    shared: &Arc<SharedData>,
    sigterm: watch::Receiver<bool>,
) -> (bool, usize, Vec<Vec<String>>) {
    let mut calls = 0;
    let mut seen = Vec::new();
    let ready = await_native_boot(&ProviderKind::Claude, shared, sigterm, TICK, || {
        calls += 1;
        seen.push(claude_reasons());
        let outcome = if calls <= busy {
            NativeBoot::RestoreHeld(BUSY.into())
        } else {
            NativeBoot::Ready
        };
        async move { outcome }
    })
    .await;
    (ready, calls, seen)
}

#[tokio::test]
async fn held_restore_is_retried_until_ready_and_health_shows_the_wait() {
    let claude = barrier(3);
    let (_latch, sigterm) = watch::channel(false);
    let (ready, calls, seen) = run_busy_then_ready(3, &claude, sigterm).await;
    assert!(ready, "a history-busy restore must not end the runtime");
    assert_eq!(calls, 4, "three held attempts, then the ready one");
    assert_eq!(
        seen[1],
        vec!["provider:claude:native_boot_restore_held:native_control_history_busy".to_string()],
        "health names the held runtime and its cause while it waits"
    );
    assert!(claude_reasons().is_empty(), "ready clears the hold");
    assert_eq!(remaining(&claude), 3);
}

#[tokio::test]
async fn refused_preparation_stops_is_visible_and_gives_up_its_slot() {
    let claude = barrier(3);
    let (_mark, sigterm) = watch::channel(false);
    let mut calls = 0;
    let ready = await_native_boot(&ProviderKind::Claude, &claude, sigterm, TICK, || {
        calls += 1;
        async { NativeBoot::Refused("reaper preparation incomplete".into()) }
    })
    .await;
    assert!(!ready);
    assert_eq!(calls, 1, "refusal is not retried in-process");
    assert_eq!(
        claude_reasons(),
        vec!["provider:claude:native_boot_refused:reaper_preparation_incomplete".to_string()]
    );
    let registry = crate::services::discord::health::HealthRegistry::new();
    let health = crate::services::discord::health::build_health_snapshot(&registry).await;
    let json = serde_json::to_value(health).unwrap();
    assert!(
        json["degraded_reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(
                "provider:claude:native_boot_refused:reaper_preparation_incomplete"
            )),
        "{json}"
    );
    // Like a failed lease, the refused runtime leaves run_bot and its slot with it.
    assert!(claude.restart.shutdown_counted.load(Ordering::Acquire));
    assert!(!claude.restart.shutting_down.load(Ordering::Acquire));
    assert_eq!(remaining(&claude), 2);
    boot_hold::release("claude");
}

/// The retry sleep is an hour: only the signal can end it.
#[tokio::test]
async fn sigterm_during_the_retry_wait_stops_and_gives_up_the_slot_once() {
    let claude = barrier(3);
    let (latch, sigterm) = watch::channel(false);
    let mut latch = Some(latch);
    let mut calls = 0;
    let gate = await_native_boot(
        &ProviderKind::Claude,
        &claude,
        sigterm.clone(),
        Duration::from_secs(3600),
        || {
            calls += 1;
            if let Some(latch) = latch.take() {
                tokio::spawn(async move {
                    latch.send_replace(true);
                    std::future::pending::<()>().await
                });
            }
            async { NativeBoot::RestoreHeld(BUSY.into()) }
        },
    );
    let ready = tokio::time::timeout(Duration::from_secs(5), gate)
        .await
        .expect("SIGTERM ends the retry wait");
    assert!(!ready);
    assert_eq!(calls, 1);
    assert!(stopped(&claude));
    assert!(claude.restart.restart_pending.load(Ordering::Acquire));
    assert_eq!(remaining(&claude), 2);
    // A second stop path for the same runtime finds the slot already given up.
    let (ready, calls, _) = run_busy_then_ready(0, &claude, sigterm).await;
    assert!(!ready, "a ready attempt after the request starts nothing");
    assert_eq!(calls, 1);
    assert_eq!(remaining(&claude), 2, "decremented once");
    boot_hold::release("claude");
}

#[tokio::test]
async fn a_ready_attempt_after_sigterm_starts_nothing() {
    let claude = barrier(3);
    let (latch, sigterm) = watch::channel(false);
    let ready = await_native_boot(&ProviderKind::Claude, &claude, sigterm, TICK, || {
        latch.send_replace(true);
        async { NativeBoot::Ready }
    })
    .await;
    assert!(!ready, "the request arrived while restore ran");
    assert!(stopped(&claude));
    assert_eq!(remaining(&claude), 2);
}

const CHILD_DIR: &str = "ADK_TEST_NATIVE_BOOT_SIGTERM_DIR";
const TESTS: &str = "services::discord::runtime_bootstrap::queued_recovery::native_input::tests::";

/// A cluster config naming this node the preferred gateway, a migrated database, and a lease
/// held by another connection: preferred-poll then waits for as long as the holder keeps it.
async fn preferred_fixture(
    root: &std::path::Path,
) -> (
    Vec<Box<dyn std::any::Any>>,
    crate::db::auto_queue::test_support::TestPostgresDb,
    sqlx::PgPool,
    String,
    crate::db::postgres::AdvisoryLockLease,
) {
    let mut guards: Vec<Box<dyn std::any::Any>> = Vec::new();
    guards.push(Box::new(crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    )));
    for (key, path) in [
        ("AGENTDESK_ROOT_DIR", root.to_path_buf()),
        ("AGENTDESK_CONFIG", root.join("config.yaml")),
    ] {
        guards.push(Box::new(
            crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(key, &path),
        ));
    }
    let mut config = crate::config::Config::default();
    config.data.dir = root.join("data");
    config.cluster.enabled = true;
    let instance =
        crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    config.cluster.instance_id = Some(instance.clone());
    config.cluster.gateway_preferred_instance_id = Some(instance.clone());
    crate::config_live_reload::install(config);
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    sqlx::query("INSERT INTO worker_nodes(instance_id,status,last_heartbeat_at,updated_at) VALUES ($1,'online',NOW(),NOW())")
        .bind(&instance)
        .execute(&pool)
        .await
        .unwrap();
    // The non-preferred node holds the lease, as on 11-23.
    let held = crate::services::discord::runtime_bootstrap::gateway_lease::try_acquire_discord_gateway_lease(
        &pool,
        TOKEN,
        &ProviderKind::Claude,
    )
    .await
    .unwrap()
    .unwrap();
    (guards, fixture, pool, instance, held)
}

const TOKEN: &str = "discord_7465737467617465";

#[derive(Clone, Copy, PartialEq)]
enum Child {
    /// Codex serves with its real SIGTERM handler; Claude is held in its gate.
    Pair,
    /// As `Pair`, but Codex already gave up its slot, so Claude holds the last one.
    PairClaudeLast,
    /// Claude alone, held in its gate.
    AloneWait,
    /// Claude alone, ready, then out of run_bot through the failed-lease slot release.
    AloneLeaseFailed,
    /// Claude alone, ready, then in preferred-poll behind a held lease.
    AlonePreferred,
}

/// Child process body; it prints its markers and ends only by exit or by signal.
fn sigterm_child(dir: std::path::PathBuf, mode: Child) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let pair = matches!(mode, Child::Pair | Child::PairClaudeLast);
        let slots = Arc::new(AtomicUsize::new(if mode == Child::Pair { 2 } else { 1 }));
        let mut preferred = None;
        if mode == Child::AlonePreferred {
            preferred = Some(preferred_fixture(&dir).await);
        }
        let pool = preferred.as_ref().map(|(_, _, pool, _, _)| pool.clone());
        let claude =
            crate::services::discord::make_shared_data_for_tests_with_barrier(pool, slots.clone());
        let _handler = pair.then(|| {
            let codex =
                crate::services::discord::make_shared_data_for_tests_with_barrier(None, slots.clone());
            if mode == Child::PairClaudeLast {
                codex.restart.shutdown_counted.store(true, Ordering::SeqCst);
            }
            super::super::super::shutdown::run_bot_spawn_sigterm_handler(&codex, ProviderKind::Codex)
        });
        let ready = if matches!(mode, Child::AloneLeaseFailed | Child::AlonePreferred) {
            // The production gate over an empty runtime root: nothing to restore, so Ready.
            let ready = super::prepare_native_boot(&claude, &ProviderKind::Claude).await;
            println!("GATE ready={ready}");
            ready
        } else {
            let release = dir.join("release");
            let mut announced = false;
            await_native_boot(
                &ProviderKind::Claude,
                &claude,
                super::super::super::boot_sigterm::watch(),
                Duration::from_millis(20),
                || {
                    let held = !release.exists();
                    if held && !announced {
                        announced = true;
                        println!("HELD");
                    }
                    async move {
                        if held {
                            NativeBoot::RestoreHeld(BUSY.into())
                        } else {
                            NativeBoot::Ready
                        }
                    }
                },
            )
            .await
        };
        match mode {
            Child::AloneLeaseFailed => {
                super::super::super::boot_sigterm::release_boot_slot(
                    &claude,
                    super::super::super::boot_sigterm::seen(),
                );
                println!("LEASE_FAILED remaining={}", slots.load(Ordering::SeqCst));
            }
            Child::AlonePreferred => {
                println!("PREFERRED_WAIT");
                let mut breaker = super::super::super::gateway_handback_breaker::GatewayHandbackBreaker::with_sources(
                    "claude", TOKEN, None, || 0, Default::default,
                );
                let pool = claude.pg_pool.clone().unwrap();
                let _ = super::super::super::gateway_lease::acquire_as_preferred_gateway(
                    &pool,
                    TOKEN,
                    &ProviderKind::Claude,
                    &claude,
                    &mut breaker,
                )
                .await;
                println!("RUNTIME_START");
            }
            _ if ready => println!("RUNTIME_START"),
            _ => {}
        }
        // Only an exit(0) or the signal's default action ends this process.
        std::future::pending::<()>().await
    });
}

/// Runs `test` as the child, waits for `marker`, sends one real SIGTERM, then frees the lock.
fn run_sigterm_child(test: &str, marker: &str) -> (Option<std::process::ExitStatus>, Vec<String>) {
    use std::io::BufRead;
    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .env(CHILD_DIR, dir.path())
        .env("AGENTDESK_ROOT_DIR", dir.path().join("root"))
        .args([
            &format!("{TESTS}{test}"),
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (lines, received) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let _ = lines.send(line);
        }
    });
    let mut seen: Vec<String> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while !seen.iter().any(|line| line.contains(marker)) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match received.recv_timeout(left) {
            Ok(line) => seen.push(line),
            Err(_) => {
                let _ = child.kill();
                panic!("child never reached {marker}: {seen:?}");
            }
        }
    }
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    std::fs::write(dir.path().join("release"), "").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        if let Ok(line) = received.recv_timeout(Duration::from_millis(100)) {
            seen.push(line);
        }
    };
    seen.extend(received.try_iter());
    (status, seen)
}

/// The child started nothing after SIGTERM and ended: by exit(0) or, with `default`, also by
/// the signal's default action.
fn assert_ended(test: &str, marker: &str, default: bool) -> Vec<String> {
    use std::os::unix::process::ExitStatusExt;
    let (status, seen) = run_sigterm_child(test, marker);
    assert!(
        !seen.iter().any(|line| line.contains("RUNTIME_START")),
        "Claude started after SIGTERM: {seen:?}"
    );
    let status = status.unwrap_or_else(|| panic!("SIGTERM did not end the process: {seen:?}"));
    let by_default = default && status.signal() == Some(libc::SIGTERM);
    assert!(
        status.code() == Some(0) || by_default,
        "{status:?} {seen:?}"
    );
    seen
}

fn child_mode() -> Option<std::path::PathBuf> {
    std::env::var(CHILD_DIR).ok().map(Into::into)
}

/// (나) Codex already serves: Claude in its retry wait starts nothing, both slots go, exit 0.
#[test]
fn real_sigterm_while_restore_is_held_stops_claude_and_ends_the_barrier() {
    if let Some(dir) = child_mode() {
        return sigterm_child(dir, Child::Pair);
    }
    assert_ended(
        "real_sigterm_while_restore_is_held_stops_claude_and_ends_the_barrier",
        "HELD",
        false,
    );
}

/// (나) Claude holds the last slot, so its gate ends the process itself.
#[test]
fn real_sigterm_with_claude_holding_the_last_slot_exits() {
    if let Some(dir) = child_mode() {
        return sigterm_child(dir, Child::PairClaudeLast);
    }
    assert_ended(
        "real_sigterm_with_claude_holding_the_last_slot_exits",
        "HELD",
        false,
    );
}

/// (가) A lone Claude registers no SIGTERM handler before its own, so the default action ends it.
#[test]
fn real_sigterm_in_the_retry_wait_of_a_lone_claude_ends_the_process() {
    if let Some(dir) = child_mode() {
        return sigterm_child(dir, Child::AloneWait);
    }
    assert_ended(
        "real_sigterm_in_the_retry_wait_of_a_lone_claude_ends_the_process",
        "HELD",
        true,
    );
}

/// (가) After the failed-lease return the slot is gone once, and SIGTERM still ends the process.
#[test]
fn real_sigterm_after_a_lone_claude_lease_failure_ends_the_process() {
    if let Some(dir) = child_mode() {
        return sigterm_child(dir, Child::AloneLeaseFailed);
    }
    let seen = assert_ended(
        "real_sigterm_after_a_lone_claude_lease_failure_ends_the_process",
        "LEASE_FAILED",
        true,
    );
    assert!(
        seen.iter()
            .any(|line| line.contains("LEASE_FAILED remaining=0")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|line| line.contains("GATE ready=true")),
        "{seen:?}"
    );
}

/// (가) A lone Claude in preferred-poll behind a held lease ends on SIGTERM.
#[test]
fn real_sigterm_in_preferred_wait_of_a_lone_claude_ends_the_process_pg() {
    if let Some(dir) = child_mode() {
        return sigterm_child(dir, Child::AlonePreferred);
    }
    let seen = assert_ended(
        "real_sigterm_in_preferred_wait_of_a_lone_claude_ends_the_process_pg",
        "PREFERRED_WAIT",
        true,
    );
    assert!(
        seen.iter().any(|line| line.contains("GATE ready=true")),
        "{seen:?}"
    );
}

/// The production path after a ready gate: the preferred node reaches preferred-poll
/// and advertises its Claude gateway waiter, which is what makes a holder yield.
#[tokio::test]
async fn held_restore_then_ready_reaches_preferred_waiter_pg() {
    use crate::services::discord::runtime_bootstrap::gateway_handback_breaker::GatewayHandbackBreaker;
    use crate::services::discord::runtime_bootstrap::gateway_lease::acquire_as_preferred_gateway;
    let root = tempfile::tempdir().unwrap();
    let (_guards, fixture, pool, instance, held) = preferred_fixture(root.path()).await;
    let token = TOKEN;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));

    let claude = barrier(3);
    let (_latch, sigterm) = tokio::sync::watch::channel(false);
    let (ready, calls, _) = run_busy_then_ready(2, &claude, sigterm).await;
    let waiting = if ready {
        let mut breaker =
            GatewayHandbackBreaker::with_sources("claude", token, None, || 0, Default::default);
        let polled = tokio::time::timeout(
            Duration::from_secs(1),
            acquire_as_preferred_gateway(
                &pool,
                token,
                &ProviderKind::Claude,
                &shared,
                &mut breaker,
            ),
        )
        .await;
        assert!(
            polled.is_err(),
            "the held lease keeps preferred-poll waiting"
        );
        let capabilities: serde_json::Value =
            sqlx::query_scalar("SELECT capabilities FROM worker_nodes WHERE instance_id=$1")
                .bind(&instance)
                .fetch_one(&pool)
                .await
                .unwrap();
        crate::services::cluster::node_registry::node_awaits_gateway(
            &serde_json::json!({ "capabilities": capabilities }),
            "claude",
        )
    } else {
        false
    };
    held.unlock().await.unwrap();
    pool.close().await;
    fixture.drop().await;
    assert_eq!(calls, 3);
    assert!(
        ready && waiting,
        "after two history-busy restores the preferred node must advertise its Claude waiter"
    );
}
