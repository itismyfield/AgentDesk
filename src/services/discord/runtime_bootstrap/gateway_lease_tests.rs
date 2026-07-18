//! Tests for `gateway_lease`.
//!
//! Split out of `gateway_lease.rs` (#4356): the file crossed the 700-LoC
//! namespace cap once these gained coverage of self-id resolution.

use super::*;
use serde_json::json;

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
