use super::*;
use crate::config::load_from_path;

#[test]
fn boot_selection_logs_once_per_value_even_with_concurrent_readers() {
    const CHILD: &str = "ADK_BOOT_SELECTION_LOG_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "config::session_hosts::tests::boot_selection_logs_once_per_value_even_with_concurrent_readers", "--nocapture"])
            .env(CHILD, "1")
            .env_remove(GUARD_ENV)
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let sink = Sink::default();
    let writer = sink.clone();
    crate::logging::test_capture::pin_callsite_interest();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);
    let count = || {
        String::from_utf8(sink.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|line| line.contains("Claude submit guard boot selection"))
            .count()
    };
    let mut config: Config = serde_yaml::from_str("server: {}\nagents:\n  - id: fixture\n    name: fixture\n    channels:\n      claude: {id: '41', runtime: tui}\n").unwrap();
    let run = |config: &Config| {
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let barrier = &barrier;
                let dispatch = &dispatch;
                scope.spawn(move || {
                    tracing::dispatcher::with_default(dispatch, || {
                        barrier.wait();
                        for _ in 0..16 {
                            let boot = BootSessionHosts::from_config(config).unwrap();
                            assert_eq!(
                                boot.config().claude_guard_channels,
                                config.session_hosts.claude_guard_channels
                            );
                        }
                    })
                });
            }
        });
    };
    run(&config);
    assert_eq!(
        count(),
        1,
        "identical concurrent selections must emit one INFO"
    );
    config.session_hosts.claude_guard_channels.insert(41);
    run(&config);
    assert_eq!(count(), 2, "a changed selection must emit one new INFO");
    config.session_hosts.claude_guard_channels.clear();
    run(&config);
    assert_eq!(
        count(),
        3,
        "returning to an earlier value is a new transition"
    );
}

#[test]
fn cg_d1_boot_env_replaces_yaml_empty_disables_and_invalid_fails_whole_list() {
    const CELL: &str = "CG_D1_BOOT_CASE";
    const ENV: &str = "AGENTDESK_CLAUDE_SUBMIT_GUARD_CHANNEL_IDS";
    if let Ok(cell) = std::env::var(CELL) {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::tempdir().unwrap();
        let yaml = if cell == "empty-overrides-unsupported-yaml" {
            42
        } else {
            41
        };
        let path = root.path().join("cg-d1-fixture.yaml");
        std::fs::write(&path, format!("server: {{}}\nagents:\n  - id: fixture\n    name: fixture\n    channels:\n      claude: {{id: '41', runtime: tui}}\n      codex: {{id: '42', runtime: tui}}\n  - id: other\n    name: other\n    channels:\n      claude: {{id: '44', runtime: tui}}\nsession_hosts: {{claude_guard_channels: [{yaml}]}}\n")).unwrap();
        struct Writer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = std::sync::Arc::clone(&log);
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || Writer(std::sync::Arc::clone(&writer)))
            .finish();
        let result = tracing::subscriber::with_default(subscriber, || {
            load_from_path(&path).and_then(|cfg| BootSessionHosts::from_config(&cfg))
        });
        if cell == "bad" {
            assert!(result.is_err(), "a bad override must fail the whole boot");
            return;
        }
        let boot =
            result.expect("validate the effective list before rejecting the YAML list it replaces");
        let wanted: BTreeSet<u64> = match cell.as_str() {
            "unset" => [41].into(),
            "replace" => [44].into(),
            _ => BTreeSet::new(),
        };
        assert_eq!(
            boot.config().claude_guard_channels,
            wanted,
            "boot selection follows the winning input"
        );
        let source = match cell.as_str() {
            "unset" => "yaml",
            "replace" => "env",
            _ => "env-empty",
        };
        let log = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        assert!(
            log.contains(&format!("source=\"{source}\"")) && log.contains("policy=\"idle-only\""),
            "{log}"
        );
        assert_eq!(boot.claude_guarded(41), wanted.contains(&41));
        assert_eq!(boot.claude_guarded(44), wanted.contains(&44));
        let _changed = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            ENV,
            "invalid-after-boot".as_ref(),
        );
        let frozen = load_from_path(&path)
            .and_then(|cfg| BootSessionHosts::from_config(&cfg))
            .expect("boot input never hot reloads; only the next child process sees its new input");
        assert_eq!(frozen.config().claude_guard_channels, wanted);
        return;
    }
    for (cell, value) in [
        ("unset", None),
        ("empty", Some("")),
        ("replace", Some("44")),
        ("empty-overrides-unsupported-yaml", Some("")),
    ] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.args(["--exact", "config::session_hosts::tests::cg_d1_boot_env_replaces_yaml_empty_disables_and_invalid_fails_whole_list", "--nocapture"]).env(CELL, cell);
        if let Some(value) = value {
            child.env(ENV, value);
        } else {
            child.env_remove(ENV);
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{cell}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    for bad in [
        "41,0",
        "41,no",
        "41,",
        "41,42",
        "41,99",
        "[41]",
        "  ",
        "41,18446744073709551616",
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact", "config::session_hosts::tests::cg_d1_boot_env_replaces_yaml_empty_disables_and_invalid_fails_whole_list", "--nocapture"])
            .env(CELL, "bad").env(ENV, bad).output().unwrap();
        assert!(
            output.status.success(),
            "{bad:?}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

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

// Provider keys match as the installed hosting resolver matches them (trimmed, case-folded, last
// entry in key order wins), so the boot check and the resolver give the same TUI verdict.
#[test]
fn guard_and_herdr_checks_read_provider_keys_as_the_installed_resolver_does() {
    use crate::services::provider::ProviderKind;
    use crate::services::provider_hosting as hosting;
    let root = tempfile::tempdir().unwrap();
    let load = |providers: &str| {
        let path = root.path().join("agentdesk.yaml");
        let data_dir = serde_json::to_string(&root.path().join("data")).unwrap();
        std::fs::write(
            &path,
            format!(
                "server: {{}}\ndata:\n  dir: {data_dir}\nproviders: {providers}\n\
                 agents:\n  - id: keys-fixture\n    name: Keys fixture\n    channels:\n      claude: {{id: '41'}}\n      codex: {{id: '42'}}\n"
            ),
        )
        .unwrap();
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        load_from_path(&path).unwrap()
    };
    let herdr_endpoint = EndpointConfig {
        execution_node: "mac-mini".into(),
        socket_path: root.path().join("herdr.sock"),
        herdr_home: root.path().join("herdr"),
        herdr_session: "agentdesk".into(),
    };
    let mut mismatched = Vec::new();
    for (providers, provider, channel, tui) in [
        (
            "{Codex: {tui_hosting: true}}",
            ProviderKind::Codex,
            42,
            true,
        ),
        ("{Claude: {runtime: pipe}}", ProviderKind::Claude, 41, false),
        (
            "{' CLAUDE ': {tui_hosting: false}}",
            ProviderKind::Claude,
            41,
            false,
        ),
        (
            "{' CLAUDE ': {runtime: '', tui_hosting: true}}",
            ProviderKind::Claude,
            41,
            true,
        ),
        (
            "{Claude: {runtime: pipe}, claude: {runtime: tui}}",
            ProviderKind::Claude,
            41,
            true,
        ),
        (
            "{claude: {tui_hosting: true}, Claude: {tui_hosting: false}}",
            ProviderKind::Claude,
            41,
            true,
        ),
    ] {
        let mut config = load(providers);
        if provider == ProviderKind::Codex {
            let herdr = &mut config.session_hosts.herdr;
            herdr
                .endpoints
                .insert("mini".into(), herdr_endpoint.clone());
            herdr.channels.insert(channel.to_string(), "mini".into());
        } else {
            config.session_hosts.claude_guard_channels.insert(channel);
        }
        let installed = {
            let _lock = hosting::TEST_CONFIG_LOCK
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            hosting::install_provider_hosting_config(&config);
            let selection = hosting::resolve_provider_session_selection_with_channel(
                &provider,
                true,
                Some(channel),
            );
            hosting::install_provider_hosting_config(&Config::default());
            selection.requested_tui_hosting
        };
        let validated = validate(&config).is_ok();
        if (validated, installed) != (tui, tui) {
            mismatched.push(format!(
                "{providers}: validated={validated} installed={installed}"
            ));
        }
    }
    assert!(mismatched.is_empty(), "{mismatched:#?}");
}
