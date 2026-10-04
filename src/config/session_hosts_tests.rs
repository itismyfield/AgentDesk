use super::*;
use crate::config::load_from_path;

const BINDINGS: &str = "claude: {id: '41', runtime: tui}\n      codex: {id: '42', runtime: tui}\n      gemini: {id: '43'}";

/// An endpoint whose paths are written as YAML strings; a temp dir path is absolute everywhere.
fn endpoint(node: &str, socket: &str, home: &str) -> String {
    format!(
        "{{execution_node: {node}, socket_path: {socket}, herdr_home: {home}, herdr_session: agentdesk}}"
    )
}

fn load(root: &std::path::Path, session_hosts: &str) -> Result<Config> {
    let path = root.join("agentdesk.yaml");
    let data_dir = serde_json::to_string(&root.join("data")).unwrap();
    std::fs::write(
        &path,
        format!(
            "server: {{}}\ndata:\n  dir: {data_dir}\ncluster: {{instance_id: ' mac-mini '}}\n\
             agents:\n  - id: hosts-fixture\n    name: Hosts fixture\n    channels:\n      {BINDINGS}\n\
             {session_hosts}\n"
        ),
    )?;
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    load_from_path(&path)
}

fn herdr(endpoints: &str, channels: &str) -> String {
    format!("session_hosts: {{herdr: {{endpoints: {endpoints}, channels: {channels}}}}}")
}

// The boot loader takes a complete endpoint for registered Claude or Codex TUI channels, refuses
// every other shape, and an empty section serializes to nothing and changes no restart verdict.
#[test]
fn session_hosts_boot_loader_takes_tui_channels_on_complete_endpoints_and_refuses_the_rest() {
    let root = tempfile::tempdir().unwrap();
    let socket_path = root.path().join("herdr").join("agentdesk.sock");
    let socket = serde_json::to_string(&socket_path).unwrap();
    let home = serde_json::to_string(&root.path().join("herdr")).unwrap();
    let mini = format!("{{mini: {}}}", endpoint("mac-mini", &socket, &home));
    let config = load(root.path(), &herdr(&mini, "{'41': mini, '42': mini}")).unwrap();
    let boot = BootSessionHosts::from_config(&config).unwrap();
    assert_eq!(boot.local_node(), Some("mac-mini"));
    assert_eq!(
        boot.channels().keys().copied().collect::<Vec<_>>(),
        [41, 42]
    );
    let bound = boot.herdr_endpoint(41).unwrap();
    assert_eq!(
        (bound.key.as_str(), bound.execution_node.as_str()),
        ("mini", "mac-mini")
    );
    assert_eq!(bound.socket_path, socket_path);
    assert!(boot.herdr_endpoint(43).is_none());

    let relative = endpoint("mac-mini", "herdr.sock", &home);
    let relative_home = endpoint("mac-mini", &socket, "herdr");
    let no_node = endpoint("' '", &socket, &home);
    let cases = [
        ("channel 0", herdr(&mini, "{'0': mini}")),
        ("not a channel id", herdr(&mini, "{dash: mini}")),
        ("unknown endpoint", herdr(&mini, "{'41': book}")),
        ("unregistered", herdr(&mini, "{'99': mini}")),
        ("non-TUI provider", herdr(&mini, "{'43': mini}")),
        (
            "relative socket",
            herdr(&format!("{{mini: {relative}}}"), "{}"),
        ),
        (
            "relative home",
            herdr(&format!("{{mini: {relative_home}}}"), "{}"),
        ),
        ("empty node", herdr(&format!("{{mini: {no_node}}}"), "{}")),
        (
            "unknown key",
            "session_hosts: {herdr: {endpoint: {}}}".into(),
        ),
    ];
    for (case, section) in cases {
        assert!(
            load(root.path(), &section).is_err(),
            "{case} must refuse boot"
        );
    }

    let empty = load(root.path(), "").unwrap();
    assert!(empty.session_hosts.is_empty());
    let serialized = serde_json::to_value(&empty).unwrap();
    assert!(
        serialized.get("session_hosts").is_none(),
        "nothing is written for an empty section"
    );
    let boot = BootSessionHosts::from_config(&empty).unwrap();
    assert!(boot.channels().is_empty());
    let changed = |old: &Config, new: &Config| {
        crate::config_live_reload::restart_required_changes(old, new).contains(&"session_hosts")
    };
    assert!(!changed(&empty, &empty.clone()));
    assert!(
        changed(&empty, &config),
        "a session_hosts edit needs a restart"
    );
}
