use super::{Config, load_from_path};

const BINDINGS: &str = "claude: {id: '41', runtime: tui}\n      codex: {id: '42', runtime: tui}";

fn load_fixture(
    root: &std::path::Path,
    writer: &str,
    bindings: &str,
    providers: &str,
) -> anyhow::Result<Config> {
    let path = root.join("agentdesk.yaml");
    let data_dir = serde_json::to_string(&root.join("data")).unwrap();
    std::fs::write(
        &path,
        format!(
            "server: {{}}\ndata:\n  dir: {data_dir}\nproviders: {providers}\n\
             agents:\n  - id: writer-fixture\n    name: Writer fixture\n    channels:\n      {bindings}\n\
             {writer}\n"
        ),
    )?;
    load_from_path(&path)
}

fn roundtrip_channels(config: &Config) -> serde_json::Value {
    let serialized = serde_json::to_value(config).unwrap();
    let restored: Config = serde_json::from_value(serialized).unwrap();
    serde_json::to_value(restored)
        .unwrap()
        .pointer("/tui_o/writer/channels")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]))
}

// The disk loader rejects malformed membership and targets without one valid TUI identity.
#[test]
fn writer_channels_disk_loader_rejects_invalid_membership_and_identity() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    load_fixture(
        root.path(),
        "tui_o: {writer: {channels: [41]}}",
        BINDINGS,
        "{}",
    )
    .expect("valid TUI channel fixture");
    let cases = [
        ("zero", "[0]", BINDINGS, "{}"),
        ("string", "['41']", BINDINGS, "{}"),
        ("negative", "[-1]", BINDINGS, "{}"),
        ("float", "[41.0]", BINDINGS, "{}"),
        ("overflow", "[18446744073709551616]", BINDINGS, "{}"),
        ("unregistered", "[99]", BINDINGS, "{}"),
        (
            "non_tui_runtime",
            "[41]",
            "claude: {id: '41', runtime: pipe}",
            "{}",
        ),
        (
            "non_tui_provider",
            "[41]",
            "gemini: {id: '41', runtime: tui}",
            "{}",
        ),
        (
            "provider_kind_conflict",
            "[41]",
            "claude: {id: '41', runtime: tui}\n      codex: {id: '41', runtime: tui}",
            "{}",
        ),
        (
            "invalid_channel_runtime",
            "[41]",
            "claude: {id: '41', runtime: typographical-error}",
            "{}",
        ),
        (
            "invalid_provider_runtime",
            "[41]",
            "claude: {id: '41'}",
            "{claude: {runtime: typographical-error, tui_hosting: true}}",
        ),
        (
            "normalized_provider_non_tui",
            "[41]",
            "claude: {id: '41'}",
            "{' CLAUDE ': {runtime: pipe}}",
        ),
        (
            "ambiguous_provider_aliases",
            "[41]",
            "claude: {id: '41', runtime: tui}",
            "{claude: {runtime: tui}, ' CLAUDE ': {runtime: pipe}}",
        ),
    ];
    let mut accepted = Vec::new();
    for (name, channels, bindings, providers) in cases {
        let writer = format!("tui_o: {{writer: {{channels: {channels}}}}}");
        match load_fixture(root.path(), &writer, bindings, providers) {
            Ok(_) => accepted.push(name),
            Err(error) => assert!(
                format!("{error:#}").contains("tui_o.writer.channels"),
                "{name}: rejection must identify writer membership: {error:#}"
            ),
        }
    }
    assert!(
        accepted.is_empty(),
        "invalid writer settings accepted: {accepted:?}"
    );
}

// Membership round-trips as a set, and only a changed applied set requires restart.
#[test]
fn writer_channels_disk_loader_normalizes_and_reports_restart_required() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let load = |writer: &str| load_fixture(root.path(), writer, BINDINGS, "{}").unwrap();
    let duplicated = load("tui_o: {writer: {channels: [41, 41]}}");
    assert_eq!(roundtrip_channels(&duplicated), serde_json::json!([41]));

    let missing = load("");
    let missing_channels = load("tui_o: {writer: {}}");
    let empty = load("tui_o: {writer: {channels: []}}");
    for config in [&missing, &missing_channels, &empty] {
        assert_eq!(roundtrip_channels(config), serde_json::json!([]));
    }
    assert!(crate::config_live_reload::restart_required_changes(&missing, &empty).is_empty());

    let changed = load("tui_o: {writer: {channels: [42]}}");
    for (old, new) in [
        (&missing, &duplicated),
        (&duplicated, &changed),
        (&changed, &empty),
    ] {
        assert!(
            crate::config_live_reload::restart_required_changes(old, new)
                .contains(&"tui_o.writer.channels"),
            "adding, replacing, and removing writer membership all require restart"
        );
    }
    let unordered = load("tui_o: {writer: {channels: [42, 41, 42]}}");
    let ordered = load("tui_o: {writer: {channels: [41, 42]}}");
    assert_eq!(roundtrip_channels(&unordered), serde_json::json!([41, 42]));
    assert!(crate::config_live_reload::restart_required_changes(&unordered, &ordered).is_empty());
}
