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

// The Claude guard list is empty by default, writes nothing back, takes only tmux Claude TUI
// channels, and an edit is restart-required like the rest of the section.
#[test]
fn claude_guard_channels_default_empty_round_trip_unchanged_and_only_claude_tui() {
    let root = tempfile::tempdir().unwrap();
    let empty = load(root.path(), "").unwrap();
    assert!(empty.session_hosts.claude_guard_channels.is_empty());
    let boot = BootSessionHosts::from_config(&empty).unwrap();
    assert!(!boot.claude_guarded(41));

    // An existing section without the key serializes back to exactly what it read.
    let socket = serde_json::to_string(&root.path().join("herdr.sock")).unwrap();
    let home = serde_json::to_string(&root.path().join("herdr")).unwrap();
    let mini = format!("{{mini: {}}}", endpoint("mac-mini", &socket, &home));
    let herdr_only = herdr(&mini, "{'42': mini}");
    let config = load(root.path(), &herdr_only).unwrap();
    let read: serde_yaml::Value = serde_yaml::from_str(&herdr_only).unwrap();
    let written = serde_yaml::to_value(&config.session_hosts).unwrap();
    assert_eq!(Some(&written), read.get("session_hosts"));
    let full = serde_yaml::to_string(&config).unwrap();
    assert!(!full.contains("claude_guard_channels"), "{full}");

    let guarded = load(root.path(), "session_hosts: {claude_guard_channels: [41]}").unwrap();
    let boot = BootSessionHosts::from_config(&guarded).unwrap();
    assert!(boot.claude_guarded(41) && !boot.claude_guarded(42));
    let written = serde_yaml::to_value(&guarded.session_hosts).unwrap();
    let expected: serde_yaml::Value =
        serde_yaml::from_str("{claude_guard_channels: [41]}").unwrap();
    assert_eq!(written, expected);

    let overlap = format!(
        "session_hosts: {{herdr: {{endpoints: {mini}, channels: {{'41': mini}}}}, claude_guard_channels: [41]}}"
    );
    for (case, section) in [
        (
            "channel 0",
            "session_hosts: {claude_guard_channels: [0]}".to_string(),
        ),
        (
            "unregistered",
            "session_hosts: {claude_guard_channels: [99]}".into(),
        ),
        (
            "codex",
            "session_hosts: {claude_guard_channels: [42]}".into(),
        ),
        (
            "non-TUI",
            "session_hosts: {claude_guard_channels: [43]}".into(),
        ),
        ("herdr channel", overlap),
    ] {
        assert!(
            load(root.path(), &section).is_err(),
            "{case} must refuse boot"
        );
    }
    assert!(
        crate::config_live_reload::restart_required_changes(&empty, &guarded)
            .contains(&"session_hosts"),
        "a guard list edit needs a restart"
    );
}

// The guard list reads a channel's TUI hosting as the hosting resolver does: a blank provider
// runtime falls back to tui_hosting, while an explicit non-TUI runtime still refuses boot.
#[test]
fn claude_guard_channel_runtime_follows_the_hosting_resolver_fallback() {
    let root = tempfile::tempdir().unwrap();
    let load = |provider: &str| {
        let path = root.path().join("agentdesk.yaml");
        let data_dir = serde_json::to_string(&root.path().join("data")).unwrap();
        std::fs::write(
            &path,
            format!(
                "server: {{}}\ndata:\n  dir: {data_dir}\nproviders:\n  claude: {provider}\n\
                 agents:\n  - id: fallback-fixture\n    name: Fallback fixture\n    channels:\n      claude: {{id: '45'}}\n\
                 session_hosts: {{claude_guard_channels: [45]}}\n"
            ),
        )
        .unwrap();
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        load_from_path(&path)
    };
    for (provider, accepted) in [
        ("{runtime: '', tui_hosting: true}", true),
        ("{runtime: tui}", true),
        ("{runtime: '', tui_hosting: false}", false),
        ("{runtime: pipe, tui_hosting: true}", false),
        ("{runtime: claude-e, tui_hosting: true}", false),
    ] {
        assert_eq!(
            load(provider).is_ok(),
            accepted,
            "providers.claude: {provider}"
        );
    }
}
