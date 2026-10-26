//! A hot-reloaded `session_hosts` edit waits for restart: probes keep the boot hosting.
use super::*;
use crate::config_live_reload::{ReloadOutcome, reload_from_path};

/// Writes the fixture config with a nameless Codex sibling, mapping `mapped` to a new endpoint.
fn write_config(rig: &Rig, other: u64, mapped: &[u64]) -> std::path::PathBuf {
    let id = rig.channel.get();
    let mut yaml = format!(
        "server: {{}}\ncluster: {{instance_id: test-node}}\nagents:\n  - id: fixture\n    name: fixture\n    provider: claude\n    channels:\n      claude: {{id: '{id}', name: s1e-{id}, runtime: tui}}\n      codex: {{id: '{other}', provider: codex, runtime: tui}}\n"
    );
    if !mapped.is_empty() {
        yaml.push_str("session_hosts:\n  herdr:\n    endpoints:\n      user-host:\n        execution_node: test-node\n        socket_path: /adk/herdr-user/agentdesk.sock\n        herdr_home: /adk/herdr-user\n        herdr_session: agentdesk\n    channels:\n");
        for channel in mapped {
            yaml.push_str(&format!("      '{channel}': user-host\n"));
        }
    }
    let path = crate::runtime_layout::config_file_path(rig.root.path());
    std::fs::write(&path, yaml).unwrap();
    path
}

async fn probe(rig: &Rig, provider: ProviderKind, channel: u64) -> (Observed, Option<Scope>) {
    let channel = ChannelId::new(channel);
    let (answer, stamp) = read(&rig.shared, &provider, channel, rig.watch()).await;
    (
        answer,
        stamp.snapshot.as_ref().map(|s| s.scope(&rig.shared).result),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn session_hosts_hot_reload_keeps_probes_on_boot_hosting_until_restart_pg() {
    let id = 69_185_411;
    let other = id + 1;
    let rig = Rig::new(id, false, false).await;
    // Load the unmapped file once so later reloads compare two loaded configs.
    let path = write_config(&rig, other, &[]);
    assert!(matches!(
        reload_from_path(&path),
        ReloadOutcome::Applied { .. }
    ));
    let idle = observed(Activity::Idle, "no_session");
    let local = (idle, Some(Scope::LocalInventory));
    let nameless = (idle, Some(Scope::LocalNameless));
    assert_eq!(probe(&rig, ProviderKind::Claude, id).await, local);
    assert_eq!(probe(&rig, ProviderKind::Codex, other).await, nameless);
    // First the sibling moves, then this channel; neither reaches a probe before restart.
    for mapped in [vec![other], vec![other, id]] {
        let path = write_config(&rig, other, &mapped);
        // The file changed but the watcher has not swapped the live snapshot yet.
        assert_eq!(probe(&rig, ProviderKind::Claude, id).await, local);
        let ReloadOutcome::Applied { restart_required } = reload_from_path(&path) else {
            panic!("the session_hosts edit was rejected: {mapped:?}");
        };
        assert!(
            restart_required.contains(&"session_hosts"),
            "{restart_required:?}"
        );
        assert_eq!(
            probe(&rig, ProviderKind::Claude, id).await,
            local,
            "mapped {mapped:?} before restart"
        );
        assert_eq!(
            probe(&rig, ProviderKind::Codex, other).await,
            nameless,
            "mapped {mapped:?} before restart"
        );
    }
    // A restart boots with the edited mapping, and both channels are then judged on Herdr.
    let _restarted = crate::config::session_hosts::force_for_test(
        Some("test-node"),
        &[(other, "test-node"), (id, "test-node")],
    );
    assert_eq!(
        probe(&rig, ProviderKind::Claude, id).await,
        (idle, Some(Scope::HerdrFirstCreate))
    );
    let (answer, _) = probe(&rig, ProviderKind::Codex, other).await;
    assert_eq!(
        answer,
        observed(Activity::Unknown, "session_identity_unresolved")
    );
}
