//! Tests for `gateway_lease`.
//!
//! Split out of `gateway_lease.rs` (#4356): the file crossed the 700-LoC
//! namespace cap once these gained coverage of self-id resolution.

use super::super::gateway_lease_recovery::standby_retry_delay;
use super::create_gateway_self_fence_restart_request;
use super::*;
use crate::services::provider::ProviderKind;
use serde_json::json;

#[test]
fn standby_retry_delay_stays_inside_thirty_to_sixty_seconds() {
    for _ in 0..128 {
        let delay = standby_retry_delay();
        assert!(delay >= Duration::from_secs(30));
        assert!(delay <= Duration::from_secs(60));
    }
}

#[test]
fn gateway_self_fence_publishes_process_restart_marker() {
    let root = tempfile::tempdir().expect("restart marker root");
    let nonce = "gateway-self-fence-test";
    assert!(
        create_gateway_self_fence_restart_request(root.path(), &ProviderKind::Codex, nonce)
            .expect("publish gateway self-fence request")
    );
    let marker = std::fs::read_to_string(root.path().join("restart_pending"))
        .expect("read canonical restart marker");
    assert!(marker.lines().any(|line| line == format!("nonce={nonce}")));
    assert!(
        marker
            .lines()
            .any(|line| line == "reason=gateway_lease_self_fence")
    );
    assert!(marker.lines().any(|line| line == "provider=codex"));
}

#[test]
fn failed_lease_does_not_authorize_worker_or_poller_startup() {
    assert!(!GatewayLeaseOutcome::Failed.starts_provider_runtime());
    assert!(GatewayLeaseOutcome::Standby.starts_provider_runtime());
    assert!(GatewayLeaseOutcome::Proceed(None).starts_provider_runtime());
}

fn cluster(enabled: bool, preferred: Option<&str>) -> crate::config::ClusterConfig {
    crate::config::ClusterConfig {
        enabled,
        gateway_preferred_instance_id: preferred.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn no_preference_configured_keeps_first_come_behavior() {
    assert!(resolve_gateway_preference(&cluster(true, None), "a".into()).is_none());
    // Blank / whitespace is treated as unset, not as an instance named "".
    assert!(resolve_gateway_preference(&cluster(true, Some("   ")), "a".into()).is_none());
}

#[test]
fn preference_is_ignored_when_clustering_is_off() {
    // A single-node deploy must never wait for a peer that cannot exist.
    assert!(
        resolve_gateway_preference(&cluster(false, Some("mac-book-release")), "a".into()).is_none()
    );
}

#[test]
fn preferred_node_recognizes_itself() {
    let pref = resolve_gateway_preference(
        &cluster(true, Some("  mac-book-release  ")),
        "mac-book-release".into(),
    )
    .expect("preference");
    assert_eq!(pref.preferred_instance_id, "mac-book-release");
    assert!(pref.self_is_preferred());
}

/// #4356. The tests above all *hand* the self-id to the pure function, so they
/// could not see that the caller derived it from
/// `resolve_self_instance_id_without_config()` — which returns `hostname-pid`
/// until `cluster::bootstrap` populates `SELF_INSTANCE_ID`, and the gateway
/// acquires its lease before that happens. The preferred node never recognized
/// itself in production.
///
/// `SELF_INSTANCE_ID` is deliberately left unset here: that is exactly the state
/// at gateway-acquire time, and the configured id must still win.
#[tokio::test]
async fn self_id_is_read_from_config_not_from_the_bootstrap_race() {
    let mut cluster = cluster(true, Some("mac-book-release"));
    cluster.instance_id = Some("mac-book-release".to_string());

    let self_id = resolve_self_instance_id_for_preference(&cluster).await;

    assert_eq!(self_id, "mac-book-release");
    let pref = resolve_gateway_preference(&cluster, self_id).expect("preference");
    assert!(
        pref.self_is_preferred(),
        "the configured preferred node must recognize itself before cluster bootstrap runs"
    );
}

#[tokio::test]
async fn configured_self_id_is_trimmed() {
    let mut cluster = cluster(true, Some("mac-book-release"));
    cluster.instance_id = Some("  mac-book-release  ".to_string());
    assert_eq!(
        resolve_self_instance_id_for_preference(&cluster).await,
        "mac-book-release"
    );
}

#[test]
fn non_preferred_node_knows_it_must_yield() {
    let pref = resolve_gateway_preference(
        &cluster(true, Some("mac-book-release")),
        "mac-mini-release".into(),
    )
    .expect("preference");
    assert!(!pref.self_is_preferred());
    assert_eq!(
        pref.yield_grace,
        Duration::from_secs(crate::config::ClusterConfig::default().gateway_yield_grace_secs)
    );
}

fn waiting_node(instance_id: &str, status: &str, waiting: &[&str]) -> serde_json::Value {
    json!({
        "instance_id": instance_id,
        "status": status,
        "capabilities": { "discord_gateway": { "waiting_providers": waiting } },
    })
}

#[test]
fn yields_only_to_a_preferred_node_that_wants_this_gateway() {
    let nodes = vec![
        waiting_node("mac-mini-release", "online", &[]),
        waiting_node("mac-book-release", "ONLINE", &["claude", "codex"]),
    ];
    assert!(should_yield_to_preferred(
        &nodes,
        "mac-book-release",
        "claude"
    ));
    assert!(should_yield_to_preferred(
        &nodes,
        "mac-book-release",
        "CODEX"
    ));
    // Same node, a provider it is not waiting for.
    assert!(!should_yield_to_preferred(
        &nodes,
        "mac-book-release",
        "gemini"
    ));
    assert!(!should_yield_to_preferred(
        &nodes,
        "ghost-release",
        "claude"
    ));
}

/// #4351 review (cdx, REJECT round 1). The preferred node's dcserver can be
/// up and heartbeating while its bot never starts — no token for this
/// provider, a startup failure, or an acquire that gave up. Yielding on
/// `status == "online"` alone handed the gateway to nobody: the holder
/// released, self-fenced, restarted, re-acquired, and yielded again — an
/// outage loop. Only a live `waiting_providers` advertisement may trigger a
/// yield.
#[test]
fn never_yields_to_a_preferred_node_that_is_merely_online() {
    let no_capability = vec![json!({"instance_id": "mac-book-release", "status": "online"})];
    assert!(!should_yield_to_preferred(
        &no_capability,
        "mac-book-release",
        "claude"
    ));

    let up_but_not_contending = vec![waiting_node("mac-book-release", "online", &[])];
    assert!(!should_yield_to_preferred(
        &up_but_not_contending,
        "mac-book-release",
        "claude"
    ));

    // Bot for the *other* provider is waiting; ours is not.
    let other_provider_only = vec![waiting_node("mac-book-release", "online", &["codex"])];
    assert!(!should_yield_to_preferred(
        &other_provider_only,
        "mac-book-release",
        "claude"
    ));
}

#[test]
fn never_yields_to_an_offline_or_malformed_preferred_node() {
    // A stale peer must not hold the gateway hostage.
    let offline = vec![waiting_node("mac-book-release", "offline", &["claude"])];
    assert!(!should_yield_to_preferred(
        &offline,
        "mac-book-release",
        "claude"
    ));

    let malformed = vec![
        json!({"instance_id": "mac-book-release"}),
        json!({"status": "online"}),
        json!("not-an-object"),
        json!({"instance_id": "mac-book-release", "status": "online",
               "capabilities": {"discord_gateway": {"waiting_providers": "claude"}}}),
    ];
    assert!(!should_yield_to_preferred(
        &malformed,
        "mac-book-release",
        "claude"
    ));
}

/// Static check: every gateway lease unlock goes through `release_gateway` (Lost first), and the
/// keepalive-failure drop closes admission first. The runtime paths need a live PG lock.
#[test]
fn every_gateway_lease_release_path_closes_o_admission_first() {
    let source = include_str!("gateway_lease.rs");
    let lines: Vec<&str> = source.lines().map(str::trim).collect();
    let unlocks: Vec<&&str> = lines
        .iter()
        .filter(|line| line.contains(".unlock()"))
        .collect();
    assert_eq!(unlocks.len(), 2, "unexpected unlock sites: {unlocks:?}");
    assert!(
        unlocks
            .iter()
            .all(|line| line.contains("release_gateway(&gate, lease.unlock())"))
    );
    let dropped: Vec<usize> = (0..lines.len())
        .filter(|&at| lines[at] == "current_lease = None;")
        .collect();
    assert_eq!(dropped.len(), 1);
    assert!(
        dropped
            .iter()
            .all(|&at| lines[at - 1] == "gate.uncertain();")
    );
    let handoff = lines
        .iter()
        .position(|line| line.contains("singleton lease taken by another instance"));
    let fence = handoff.and_then(|at| {
        lines[at..]
            .iter()
            .position(|line| line.starts_with("self_fence_gateway"))
            .map(|off| at + off)
    });
    assert!(fence.is_some_and(|at| lines[at - 1] == "gate.lost();"));
}

fn snapshot_root() -> (
    tempfile::TempDir,
    crate::config_live_reload::TestLiveGuard,
    [crate::config::TestEnvVarGuard; 2],
) {
    let root = tempfile::tempdir().unwrap();
    let live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let paths = [
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        ),
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &root.path().join("config.yaml"),
        ),
    ];
    (root, live, paths)
}

#[tokio::test]
async fn gateway_snapshot_preference_and_application_name_ignore_disk_edits() {
    let (root, _live, _paths) = snapshot_root();
    let mut live = crate::config::Config::default();
    live.data.dir = root.path().join("data");
    live.cluster.enabled = true;
    live.cluster.instance_id = Some("snapshot-node".into());
    live.cluster.gateway_preferred_instance_id = Some("snapshot-node".into());
    live.cluster.gateway_yield_grace_secs = 7;
    let mut disk = live.clone();
    disk.cluster.instance_id = Some("disk-node".into());
    disk.cluster.gateway_preferred_instance_id = Some("disk-preferred".into());
    disk.cluster.gateway_yield_grace_secs = 99;
    crate::config::save_to_path(&root.path().join("config.yaml"), &disk).unwrap();
    crate::config_live_reload::install(live);
    let preference = gateway_preference().await.unwrap();
    assert!(preference.self_is_preferred());
    assert_eq!(preference.preferred_instance_id, "snapshot-node");
    assert_eq!(preference.yield_grace, Duration::from_secs(7));
    let recovery =
        super::super::gateway_lease_recovery::gateway_lease_application_name(&ProviderKind::Claude);
    let expected = super::super::gateway_lease_recovery::gateway_lease_application_name_for(
        "snapshot-node",
        std::process::id(),
        "claude",
    );
    assert_eq!(
        recovery, expected,
        "gateway identity must use the installed snapshot"
    );
}

#[tokio::test(start_paused = true)]
async fn gateway_snapshot_wait_warning_uses_captured_live_timing() {
    use std::io::{Read, Seek, SeekFrom};
    let (root, _live, _paths) = snapshot_root();
    let mut live = crate::config::Config::default();
    live.data.dir = root.path().join("data");
    live.cluster.gateway_lease_idle_expiry_secs = 0;
    live.cluster.gateway_yield_grace_secs = 0;
    let mut disk = live.clone();
    disk.cluster.gateway_lease_idle_expiry_secs = 200;
    disk.cluster.gateway_yield_grace_secs = 100;
    crate::config::save_to_path(&root.path().join("config.yaml"), &disk).unwrap();
    crate::config_live_reload::install(live);
    let file = tempfile::tempfile().unwrap();
    let mut reader = file.try_clone().unwrap();
    crate::logging::test_capture::pin_callsite_interest();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || file.try_clone().unwrap())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy_with(
        sqlx::postgres::PgConnectOptions::new_without_pgpass()
            .socket(root.path().join("closed-postgres")),
    );
    pool.close().await;
    let mut warning = expiry::PreferredWaitWarning::start();
    crate::config_live_reload::install(disk);
    tokio::time::advance(Duration::from_secs(59)).await;
    warning.observe(&pool, 1, &ProviderKind::Claude).await;
    let mut logs = String::new();
    reader.seek(SeekFrom::Start(0)).unwrap();
    reader.read_to_string(&mut logs).unwrap();
    assert!(!logs.contains("preferred node has waited longer"));
    tokio::time::advance(Duration::from_secs(1)).await;
    warning.observe(&pool, 1, &ProviderKind::Claude).await;
    logs.clear();
    reader.seek(SeekFrom::Start(0)).unwrap();
    reader.read_to_string(&mut logs).unwrap();
    assert_eq!(logs.matches("preferred node has waited longer").count(), 1);
    tokio::time::advance(Duration::from_secs(599)).await;
    warning.observe(&pool, 1, &ProviderKind::Claude).await;
    logs.clear();
    reader.seek(SeekFrom::Start(0)).unwrap();
    reader.read_to_string(&mut logs).unwrap();
    assert_eq!(logs.matches("preferred node has waited longer").count(), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    warning.observe(&pool, 1, &ProviderKind::Claude).await;
    logs.clear();
    reader.seek(SeekFrom::Start(0)).unwrap();
    reader.read_to_string(&mut logs).unwrap();
    assert_eq!(logs.matches("preferred node has waited longer").count(), 2);
}

#[tokio::test]
async fn gateway_snapshot_preferred_waiter_uses_live_lease_ttl_pg() {
    let (root, _live, _paths) = snapshot_root();
    let mut live = crate::config::Config::default();
    live.data.dir = root.path().join("data");
    live.cluster.lease_ttl_secs = 600;
    let mut disk = live.clone();
    disk.cluster.lease_ttl_secs = 1;
    crate::config::save_to_path(&root.path().join("config.yaml"), &disk).unwrap();
    crate::config_live_reload::install(live);
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    sqlx::query("INSERT INTO worker_nodes(instance_id,status,last_heartbeat_at,capabilities) VALUES ('snapshot-preferred','online',NOW()-INTERVAL '10 seconds',$1)")
        .bind(json!({"discord_gateway":{"waiting_providers":["claude"]}}))
        .execute(&pool).await.unwrap();
    let waiting =
        preferred_gateway_is_waiting(&pool, "snapshot-preferred", &ProviderKind::Claude).await;
    pool.close().await;
    fixture.drop().await;
    assert!(waiting, "snapshot TTL keeps the known waiter online");
}
