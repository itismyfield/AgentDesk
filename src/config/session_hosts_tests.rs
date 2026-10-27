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

// Provider keys match as the installed hosting resolver matches them (trimmed, case-folded, last
// entry in key order wins), so the boot check and the resolver give the same TUI verdict.
#[test]
fn herdr_check_reads_provider_keys_as_the_installed_resolver_does() {
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
        mode: EndpointMode::LegacyDedicated,
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
        let herdr = &mut config.session_hosts.herdr;
        herdr
            .endpoints
            .insert("mini".into(), herdr_endpoint.clone());
        herdr.channels.insert(channel.to_string(), "mini".into());
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

// An old config or env naming the removed Claude guard list still boots and warns once per
// process; the key is never written back, and any other unknown key still refuses boot.
#[test]
fn retired_claude_guard_key_and_env_boot_with_one_warning() {
    const CHILD: &str = "ADK_RETIRED_GUARD_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "config::session_hosts::tests::retired_claude_guard_key_and_env_boot_with_one_warning",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(RETIRED_GUARD_ENV, "not,a,list")
            .output()
            .unwrap();
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
        .with_writer(move || writer.clone())
        .finish();
    let root = tempfile::tempdir().unwrap();
    let retired = "session_hosts: {claude_guard_channels: [99, 0]}";
    let config = tracing::subscriber::with_default(subscriber, || {
        let config = load(root.path(), retired).expect("the retired key still boots");
        for _ in 0..4 {
            BootSessionHosts::from_config(&config).expect("every boot read accepts it");
        }
        config
    });
    let log = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    let warnings: Vec<_> = log
        .lines()
        .filter(|line| line.contains("retired Claude submit guard setting ignored"))
        .collect();
    assert_eq!(warnings.len(), 1, "{log}");
    assert!(warnings[0].contains("WARN") && warnings[0].contains("yaml=true env=true"));
    assert!(config.session_hosts.is_empty());
    let written = serde_yaml::to_string(&config).unwrap();
    assert!(!written.contains("claude_guard_channels"), "{written}");
    let empty = load(root.path(), "").unwrap();
    assert!(
        !crate::config_live_reload::restart_required_changes(&empty, &config)
            .contains(&"session_hosts"),
        "dropping the ignored key needs no restart"
    );
    for typo in [
        "session_hosts: {claude_guard_channelz: [41]}",
        "session_hosts: {claude_guard: [41]}",
    ] {
        assert!(load(root.path(), typo).is_err(), "{typo} must refuse boot");
    }
}
