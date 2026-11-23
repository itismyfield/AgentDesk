//! #8351: a held native boot restore is retried instead of dropping the Claude runtime.
use super::{NativeBoot, await_native_boot};
use crate::services::discord::health::boot_hold;
use crate::services::provider::ProviderKind;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const BUSY: &str = "native control history busy";
const TICK: Duration = Duration::from_millis(1);

struct Slot {
    shutting_down: AtomicBool,
    counted: AtomicBool,
    remaining: AtomicUsize,
}

impl Slot {
    fn new(shutting_down: bool) -> Self {
        Self {
            shutting_down: AtomicBool::new(shutting_down),
            counted: AtomicBool::new(false),
            remaining: AtomicUsize::new(3),
        }
    }
}

fn claude_reasons() -> Vec<String> {
    boot_hold::health_reasons()
        .into_iter()
        .filter(|reason| reason.starts_with("provider:claude:"))
        .collect()
}

/// Fails `busy` times with the observed restore error, then succeeds.
async fn run_busy_then_ready(busy: usize, slot: &Slot) -> (bool, usize, Vec<Vec<String>>) {
    let mut calls = 0;
    let mut seen = Vec::new();
    let ready = await_native_boot(
        &ProviderKind::Claude,
        &slot.shutting_down,
        &slot.counted,
        &slot.remaining,
        TICK,
        || {
            calls += 1;
            seen.push(claude_reasons());
            let outcome = if calls <= busy {
                NativeBoot::RestoreHeld(BUSY.into())
            } else {
                NativeBoot::Ready
            };
            async move { outcome }
        },
    )
    .await;
    (ready, calls, seen)
}

// The holds table is process-wide; one test owns the claude entry at a time.
static HOLDS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn held_restore_is_retried_until_ready_and_health_shows_the_wait() {
    let _guard = HOLDS_LOCK.lock().await;
    let slot = Slot::new(false);
    let (ready, calls, seen) = run_busy_then_ready(3, &slot).await;
    assert!(ready, "a history-busy restore must not end the runtime");
    assert_eq!(calls, 4, "three held attempts, then the ready one");
    assert_eq!(
        seen[1],
        vec!["provider:claude:native_boot_restore_held:native_control_history_busy".to_string()],
        "health names the held runtime and its cause while it waits"
    );
    assert!(claude_reasons().is_empty(), "ready clears the hold");
    assert_eq!(slot.remaining.load(Ordering::Acquire), 3);
}

#[tokio::test]
async fn refused_preparation_still_stops_but_is_visible() {
    let _guard = HOLDS_LOCK.lock().await;
    let slot = Slot::new(false);
    let mut calls = 0;
    let ready = await_native_boot(
        &ProviderKind::Claude,
        &slot.shutting_down,
        &slot.counted,
        &slot.remaining,
        TICK,
        || {
            calls += 1;
            async { NativeBoot::Refused("reaper preparation incomplete".into()) }
        },
    )
    .await;
    assert!(!ready);
    assert_eq!(calls, 1, "refusal is not retried in-process");
    assert_eq!(
        claude_reasons(),
        vec!["provider:claude:native_boot_refused:reaper_preparation_incomplete".to_string()]
    );
    assert_eq!(slot.remaining.load(Ordering::Acquire), 3, "slot unchanged");
    boot_hold::release("claude");
}

#[tokio::test]
async fn shutdown_ends_the_retry_and_gives_up_the_slot_once() {
    let _guard = HOLDS_LOCK.lock().await;
    let slot = Slot::new(true);
    for _ in 0..2 {
        let (ready, calls, _) = run_busy_then_ready(usize::MAX, &slot).await;
        assert!(!ready);
        assert_eq!(calls, 1, "shutdown stops after the held attempt");
    }
    assert_eq!(slot.remaining.load(Ordering::Acquire), 2, "decremented once");
    assert!(slot.counted.load(Ordering::Acquire));
    boot_hold::release("claude");
}

/// The production path after a ready gate: the preferred node reaches preferred-poll
/// and advertises its Claude gateway waiter, which is what makes a holder yield.
#[tokio::test]
async fn held_restore_then_ready_reaches_preferred_waiter_pg() {
    use crate::services::discord::runtime_bootstrap::gateway_handback_breaker::GatewayHandbackBreaker;
    use crate::services::discord::runtime_bootstrap::gateway_lease::{
        acquire_as_preferred_gateway, try_acquire_discord_gateway_lease,
    };
    let _guard = HOLDS_LOCK.lock().await;
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

    let slot = Slot::new(false);
    let (ready, calls, _) = run_busy_then_ready(2, &slot).await;
    let waiting = if ready {
        let mut breaker = GatewayHandbackBreaker::with_sources(
            "claude",
            token,
            None,
            || 0,
            Default::default,
        );
        let polled = tokio::time::timeout(
            Duration::from_secs(1),
            acquire_as_preferred_gateway(&pool, token, &ProviderKind::Claude, &shared, &mut breaker),
        )
        .await;
        assert!(polled.is_err(), "the held lease keeps preferred-poll waiting");
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
