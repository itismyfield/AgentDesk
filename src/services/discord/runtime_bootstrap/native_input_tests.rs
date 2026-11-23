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
async fn refused_preparation_stops_is_visible_and_answers_sigterm() {
    let claude = barrier(3);
    let (latch, sigterm) = watch::channel(false);
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
    assert_eq!(remaining(&claude), 3, "the slot waits for SIGTERM");
    latch.send_replace(true);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !stopped(&claude) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the refused runtime gives up its slot on SIGTERM");
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

/// Child process: Claude is held in its gate; with `codex`, Codex is past its gate with its
/// real SIGTERM handler and holds the other barrier slot.
fn sigterm_child(dir: std::path::PathBuf, codex: bool) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let slots = Arc::new(AtomicUsize::new(1 + usize::from(codex)));
        let claude =
            crate::services::discord::make_shared_data_for_tests_with_barrier(None, slots.clone());
        let _handler = codex.then(|| {
            let codex =
                crate::services::discord::make_shared_data_for_tests_with_barrier(None, slots);
            super::super::super::shutdown::run_bot_spawn_sigterm_handler(
                &codex,
                ProviderKind::Codex,
            )
        });
        let release = dir.join("release");
        let mut announced = false;
        let sigterm = super::super::super::boot_sigterm::subscribe();
        let ready = await_native_boot(
            &ProviderKind::Claude,
            &claude,
            sigterm,
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
        .await;
        if ready {
            println!("RUNTIME_START");
        }
        println!("GATE_RETURNED");
        // Only the last slot holder's exit(0) ends this process.
        std::future::pending::<()>().await
    });
}

/// Runs `test` as the child, sends it one real SIGTERM while Claude is held, then frees the lock.
fn run_sigterm_child(test: &str) -> (Option<std::process::ExitStatus>, Vec<String>) {
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
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !seen.iter().any(|line| line.ends_with("HELD")) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match received.recv_timeout(left) {
            Ok(line) => seen.push(line),
            Err(_) => {
                let _ = child.kill();
                panic!("child never held: {seen:?}");
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

fn assert_stopped_and_exited(test: &str) {
    let (status, seen) = run_sigterm_child(test);
    assert!(
        !seen.iter().any(|line| line.contains("RUNTIME_START")),
        "Claude started after SIGTERM: {seen:?}"
    );
    let status = status.unwrap_or_else(|| panic!("the shutdown barrier never ended: {seen:?}"));
    assert_eq!(status.code(), Some(0), "{seen:?}");
}

/// One real SIGTERM while Claude sleeps between held restores and Codex already serves:
/// Claude starts nothing, both slots go, and the process exits 0.
#[test]
fn real_sigterm_while_restore_is_held_stops_claude_and_ends_the_barrier() {
    if let Ok(dir) = std::env::var(CHILD_DIR) {
        return sigterm_child(dir.into(), true);
    }
    assert_stopped_and_exited(
        "real_sigterm_while_restore_is_held_stops_claude_and_ends_the_barrier",
    );
}

/// Claude holds the last slot, so its gate must end the process itself.
#[test]
fn real_sigterm_with_claude_holding_the_last_slot_exits() {
    if let Ok(dir) = std::env::var(CHILD_DIR) {
        return sigterm_child(dir.into(), false);
    }
    assert_stopped_and_exited("real_sigterm_with_claude_holding_the_last_slot_exits");
}

/// The production path after a ready gate: the preferred node reaches preferred-poll
/// and advertises its Claude gateway waiter, which is what makes a holder yield.
#[tokio::test]
async fn held_restore_then_ready_reaches_preferred_waiter_pg() {
    use crate::services::discord::runtime_bootstrap::gateway_handback_breaker::GatewayHandbackBreaker;
    use crate::services::discord::runtime_bootstrap::gateway_lease::{
        acquire_as_preferred_gateway, try_acquire_discord_gateway_lease,
    };
    let root = tempfile::tempdir().unwrap();
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let _paths = [
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        ),
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &root.path().join("config.yaml"),
        ),
    ];
    let mut config = crate::config::Config::default();
    config.data.dir = root.path().join("data");
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
    let token = "discord_7465737467617465";
    // The non-preferred node holds the lease, as on 11-23.
    let held = try_acquire_discord_gateway_lease(&pool, token, &ProviderKind::Claude)
        .await
        .unwrap()
        .unwrap();
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
