//! Runtime-profile health projection and gateway standby semantics.
use super::*;

pub(super) fn attach_runtime_profile(json: &mut serde_json::Value, config: &crate::config::Config) {
    json["runtime_profile"] = serde_json::json!(config.cluster.runtime_profile);
    json["modules"] = serde_json::json!(config.cluster.runtime_profile.modules());
    json["dashboard_required"] =
        serde_json::json!(config.cluster.runtime_profile.modules().dashboard);
    json["herdr"] = herdr_health(config);
    if let Some(homes) = crate::services::cluster::channel_home::health() {
        json["channel_homes"] = homes;
    }
}

/// The boot `session_hosts` view and the admission switch. No E7 runs here; `last_e7` is `never`
/// until a launch records one. Admission is read only once a channel is configured.
fn herdr_health(config: &crate::config::Config) -> serde_json::Value {
    use crate::config::session_hosts::{BootSessionHosts, with_boot};
    let view = |boot: &BootSessionHosts| {
        let channels: Vec<String> = boot.channels().keys().map(u64::to_string).collect();
        let admission = match channels.is_empty() {
            true => "not_evaluated".to_string(),
            false => match crate::services::herdr_admission::check() {
                Ok(()) => "open".to_string(),
                Err(cause) => format!("stopped({})", cause.as_str()),
            },
        };
        let mut endpoints = serde_json::Map::new();
        for (key, endpoint) in &boot.config().herdr.endpoints {
            let node = endpoint.execution_node.trim();
            let local = boot.local_node() == Some(node);
            let entry = serde_json::json!({"node": node, "local": local, "last_e7": "never"});
            endpoints.insert(key.clone(), entry);
        }
        let mut view = serde_json::json!({
            "configured_channels": channels,
            "admission": admission,
            "endpoints": endpoints,
            "restart_required": *boot.config() != config.session_hosts,
        });
        attach_local_herdr(&mut view);
        view
    };
    with_boot(|boot| match boot {
        Some(boot) => view(boot),
        None => view(&BootSessionHosts::default()),
    })
}

/// The restart reconnect's latest counts and the held inputs it counted, only on a node with a
/// local endpoint; nothing is read here.
fn attach_local_herdr(view: &mut serde_json::Value) {
    #[cfg(unix)]
    if let Some((counts, holds)) = crate::services::discord::herdr_reconnect_health() {
        view["reconnect"] = serde_json::json!(counts);
        view["input_holds"] = match holds {
            Some(Ok(held)) => held.into(),
            Some(Err(_)) => "unreadable".into(),
            None => "not_counted_yet".into(),
        };
    }
    #[cfg(not(unix))]
    let _ = view;
}

pub(super) async fn cluster_standby_without_gateway(
    state: &AppState,
    server_up: bool,
    degraded_reasons: &[serde_json::Value],
) -> bool {
    if !server_up
        || !state.config.cluster.enabled
        || !state.config.cluster.runtime_profile.modules().gateway
    {
        return false;
    }
    if !degraded_reasons
        .iter()
        .any(|reason| reason.as_str() == Some("no_providers_registered"))
    {
        return false;
    }
    let instance_id = state
        .config
        .cluster
        .instance_id
        .as_deref()
        .unwrap_or("")
        .trim();
    if instance_id.is_empty() {
        return false;
    }
    health_diagnostics::is_recent_cluster_worker(
        state.pg_pool_ref(),
        instance_id,
        state.config.cluster.lease_ttl_secs,
    )
    .await
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::config::session_hosts::{force_for_test, with_boot};
    use crate::services::herdr_admission::{Admission, force_for_test as force_admission};

    fn herdr(config: &crate::config::Config) -> serde_json::Value {
        let mut health = json!({});
        super::attach_runtime_profile(&mut health, config);
        health["herdr"].take()
    }

    // The health body shows the boot section as booted: nothing configured reads no admission,
    // a configured node shows its stopped switch and endpoints, and a live edit needs a restart.
    #[test]
    fn health_projects_the_boot_session_hosts_and_admission() {
        let live = crate::config::Config::default();
        let _empty = force_for_test(None, &[]);
        let quiet = json!({"configured_channels": [], "admission": "not_evaluated",
            "endpoints": {}, "restart_required": false});
        assert_eq!(herdr(&live), quiet);

        let _hosts = force_for_test(Some("mac-mini"), &[(41, "mac-mini"), (42, "mac-book")]);
        let _off = force_admission(Admission::new(Some("off".as_ref()), None));
        let mut booted = live.clone();
        booted.session_hosts = with_boot(|boot| boot.unwrap().config().clone());
        let endpoint =
            |node: &str, local: bool| json!({"node": node, "local": local, "last_e7": "never"});
        let mut expected = json!({"configured_channels": ["41", "42"], "admission": "stopped(env)",
            "endpoints": {"mac-book-endpoint": endpoint("mac-book", false),
                "mac-mini-endpoint": endpoint("mac-mini", true)},
            "restart_required": false});
        assert_eq!(herdr(&booted), expected);
        expected["restart_required"] = json!(true);
        assert_eq!(
            herdr(&live),
            expected,
            "the live section differs from the booted one"
        );
    }

    // With no delegated home registered the body has no channel home key at all; a registered
    // one shows its state and what its drain waits on, read from memory.
    #[test]
    fn health_shows_channel_homes_only_once_one_is_registered() {
        use crate::db::o_channel_homes::{HeldHome, HomeState};
        use crate::services::cluster::channel_home::{self, HomeGate};
        let config = crate::config::Config::default();
        let mut quiet = json!({});
        super::attach_runtime_profile(&mut quiet, &config);
        assert_eq!(quiet.get("channel_homes"), None);

        let home = std::sync::Arc::new(HomeGate::new("77", "mini"));
        channel_home::register(std::sync::Arc::clone(&home));
        let mut health = json!({});
        super::attach_runtime_profile(&mut health, &config);
        let lost = json!({"homes": [{"channel": "77", "holder": "mini", "home": "lost",
            "epoch": null}], "home_draining": []});
        assert_eq!(health["channel_homes"], lost);

        let renewal = HeldHome::for_test("77", "mini", 5, HomeState::Reclaiming);
        home.confirm(&renewal, tokio::time::Instant::now()).unwrap();
        home.note_drain(Some("owed"));
        super::attach_runtime_profile(&mut health, &config);
        let draining = json!({"homes": [{"channel": "77", "holder": "mini", "home": "draining",
            "epoch": 5}], "home_draining": [{"channel": "77", "blocker": "owed"}]});
        assert_eq!(health["channel_homes"], draining);
        channel_home::unregister("77");
        let mut after = json!({});
        super::attach_runtime_profile(&mut after, &config);
        assert_eq!(after, quiet);
    }

    // A local endpoint adds the latest restart reconnect counts and the held inputs that pass
    // counted, never read here; without one the projection above stays as it was.
    #[cfg(unix)]
    #[test]
    fn health_counts_reconnects_and_held_inputs_only_with_a_local_endpoint() {
        use crate::services::session_host::herdr_socket_rig_tests::HerdrRig;
        let _root = crate::config::TestRuntimeRootGuard::new();
        let live = crate::config::Config::default();
        let _hosts = force_for_test(Some("mac-mini"), &[]);
        assert_eq!(herdr(&live).get("reconnect"), None);
        let holds = crate::config::runtime_root()
            .unwrap()
            .join("runtime/herdr_input_holds");
        std::fs::create_dir_all(&holds).unwrap();
        std::fs::write(holds.join("n1"), "2026-10-05T07:00:00+00:00").unwrap();

        let rig = HerdrRig::start();
        let _registry = rig.registry_on_this_thread();
        let health = herdr(&live);
        let none = json!({"channels": 0, "published": 0, "withheld": 0, "unknown": 0,
            "pending": 0});
        let uncounted = json!("not_counted_yet");
        assert_eq!(
            (&health["reconnect"], &health["input_holds"]),
            (&none, &uncounted)
        );
    }
}
