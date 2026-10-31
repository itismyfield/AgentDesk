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
    // Loading resolves paths against AGENTDESK_ROOT_DIR, which parallel tests set under this lock.
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
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

// `all_tui` selects the TUI bindings and skips the rest, never sits beside a non-empty list, and
// applies at restart; the list mode serializes as before.
#[test]
fn writer_all_tui_selects_only_tui_bindings_and_applies_on_restart() {
    use crate::config_live_reload::restart_required_changes;
    use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui};
    use crate::services::tui_o::channel_policy::BootChannels;
    // Loading resolves paths against AGENTDESK_ROOT_DIR, which parallel tests set under this lock.
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let path = root.path().join("agentdesk.yaml");
    let data_dir = serde_json::to_string(&root.path().join("data")).unwrap();
    let load = |cluster: &str, writer: &str| {
        let agents = "agents:\n  - id: tui\n    name: TUI\n    channels:\n      \
            claude: {id: '41', runtime: tui}\n      codex: {id: '42', runtime: tui}\n      \
            gemini: {id: '43'}\n  - id: pipe\n    name: Pipe\n    channels:\n      \
            claude: {id: '44', runtime: pipe}\n      codex: {id: '45'}\n";
        let yaml = format!("server: {{}}\ndata:\n  dir: {data_dir}\n{cluster}{agents}{writer}\n");
        std::fs::write(&path, yaml).unwrap();
        load_from_path(&path)
    };
    let all = load("", "tui_o: {writer: {all_tui: true}}").unwrap();
    let boot = BootChannels::validate(&all).unwrap();
    assert_eq!(
        boot.channels().iter().copied().collect::<Vec<_>>(),
        [41, 42]
    );
    assert_eq!(
        (boot.kind(41), boot.kind(42)),
        (Some(ClaudeTui), Some(CodexTui))
    );
    let pointer = |config: &Config| {
        let value = serde_json::to_value(config).unwrap();
        value.pointer("/tui_o/writer/all_tui").cloned()
    };
    assert_eq!(pointer(&all), Some(serde_json::json!(true)));

    let listed = load("", "tui_o: {writer: {channels: [41, 42]}}").unwrap();
    assert_eq!(pointer(&listed), None);
    assert!(restart_required_changes(&listed, &all).is_empty());
    let one = load("", "tui_o: {writer: {channels: [41]}}").unwrap();
    for (old, new) in [(&one, &all), (&all, &one)] {
        let changes = restart_required_changes(old, new);
        assert!(changes.contains(&"tui_o.writer.channels"), "{changes:?}");
    }

    let both = load("", "tui_o: {writer: {all_tui: true, channels: [41]}}").unwrap_err();
    assert!(
        format!("{both:#}").contains("cannot be combined"),
        "{both:#}"
    );
    load("", "tui_o: {writer: {all_tui: true, channels: []}}").unwrap();
    let unnamed = "cluster: {enabled: true, instance_id: a}\n";
    let home = load(unnamed, "tui_o: {writer: {all_tui: true}}").unwrap_err();
    assert!(
        format!("{home:#}").contains("cluster.gateway_preferred_instance_id"),
        "{home:#}"
    );
    load(unnamed, "tui_o: {writer: {all_tui: false}}").unwrap();
}

#[test]
fn readers_s1b_dormant_disk_loader_defaults_empty_and_omits_empty_set() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    for writer in [
        "",
        "tui_o: {writer: {channels: [41]}}",
        "tui_o: {writer: {channels: [41], dormant_channels: []}}",
    ] {
        let config = load_fixture(root.path(), writer, BINDINGS, "{}").unwrap();
        let value = serde_json::to_value(config).unwrap();
        assert_eq!(
            value.pointer("/tui_o/writer/dormant_channels"),
            None,
            "default OFF must not serialize an activation list"
        );
    }
}

#[test]
fn readers_s1b_dormant_disk_loader_normalizes_without_granting_ownership() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let config = load_fixture(
        root.path(),
        "tui_o: {writer: {channels: [41, 42], dormant_channels: [42, 41, 42]}}",
        BINDINGS,
        "{}",
    )
    .expect("valid dormant subset must pass the real disk loader");
    let value = serde_json::to_value(&config).unwrap();
    assert_eq!(
        value.pointer("/tui_o/writer/dormant_channels"),
        Some(&serde_json::json!([41, 42]))
    );
    assert_eq!(roundtrip_channels(&config), serde_json::json!([41, 42]));
    let restored: Config = serde_json::from_value(value).unwrap();
    let value = serde_json::to_value(restored).unwrap();
    assert_eq!(
        value.pointer("/tui_o/writer/dormant_channels"),
        Some(&serde_json::json!([41, 42]))
    );
}

#[test]
fn readers_s1b_dormant_disk_loader_rejects_zero_invalid_and_unselected_ids() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    load_fixture(
        root.path(),
        "tui_o: {writer: {channels: [41], dormant_channels: [41]}}",
        BINDINGS,
        "{}",
    )
    .expect("rejection cases must not pass merely because the key is unknown");
    for dormant in [
        "[0]",
        "['41']",
        "[-1]",
        "[41.0]",
        "[18446744073709551616]",
        "[42]",
        "[99]",
    ] {
        let writer = format!("tui_o: {{writer: {{channels: [41], dormant_channels: {dormant}}}}}");
        let error = load_fixture(root.path(), &writer, BINDINGS, "{}").unwrap_err();
        assert!(
            format!("{error:#}").contains("dormant_channels"),
            "{dormant}: rejection must identify dormant membership: {error:#}"
        );
    }
    assert!(
        load_fixture(
            root.path(),
            "tui_o: {writer: {all_tui: false, dormant_channels: [41]}}",
            BINDINGS,
            "{}",
        )
        .is_err(),
        "a dormant list alone must not select an O writer"
    );
}

#[test]
fn readers_s1b_dormant_disk_loader_validates_actual_all_tui_selection() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let bindings = "claude: {id: '41', runtime: tui}\n      codex: {id: '42', runtime: pipe}";
    let config = load_fixture(
        root.path(),
        "tui_o: {writer: {all_tui: true, dormant_channels: [41, 41]}}",
        bindings,
        "{}",
    )
    .expect("all_tui dormant membership uses the resolved TUI selection");
    let value = serde_json::to_value(config).unwrap();
    assert_eq!(
        value.pointer("/tui_o/writer/dormant_channels"),
        Some(&serde_json::json!([41]))
    );
    for dormant in ["[0]", "[42]", "[99]"] {
        let writer = format!("tui_o: {{writer: {{all_tui: true, dormant_channels: {dormant}}}}}");
        assert!(
            load_fixture(root.path(), &writer, bindings, "{}").is_err(),
            "all_tui must reject non-selected dormant IDs: {dormant}"
        );
    }
}

/// What the boot selection hands the writer and turn mode: selected channels, late adoption, and
/// the owned channels turn confirmation would offer to retirement.
fn boot_selection(config: &Config) -> (Vec<u64>, bool, Vec<u64>) {
    use crate::services::tui_o::{channel_policy::BootChannels, turn_mode};
    let boot = BootChannels::validate(config).unwrap();
    let channels: Vec<u64> = boot.channels().iter().copied().collect();
    let mut offered = Vec::new();
    let turn = crate::services::tui_o::shadow::tap::TuiOConfig::of(config)
        .turn
        .clone();
    assert_eq!(&turn, boot.turn());
    turn_mode::confirm_selected(
        Some(&turn),
        || channels.clone(),
        |channel| {
            offered.push(channel);
            false
        },
    );
    (channels, boot.late_adopt_capable(), offered)
}

// Unset writer and turn flags select every TUI binding; a list or an explicit false keeps its mode.
#[test]
fn tui_o_defaults_select_every_tui_binding_unless_a_list_or_false_says_otherwise() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let bindings = "claude: {id: '41', runtime: tui}\n      codex: {id: '42', runtime: tui}\n      \
                    gemini: {id: '43'}";
    let select =
        |writer: &str| boot_selection(&load_fixture(root.path(), writer, bindings, "{}").unwrap());
    let all = (vec![41, 42], true, vec![41, 42]);
    for writer in [
        "",
        "tui_o: {tool_cards: {enabled: false}}",
        "tui_o: {writer: {channels: []}}",
    ] {
        assert_eq!(select(writer), all, "{writer:?}");
    }
    let cases = [
        ("tui_o: {writer: {all_tui: false}}", (vec![], true, vec![])),
        (
            "tui_o: {writer: {channels: [41]}}",
            (vec![41], true, vec![41]),
        ),
        (
            "tui_o: {writer: {late_adopt: false}}",
            (vec![41, 42], false, vec![41, 42]),
        ),
        (
            "tui_o: {turn: {all_owned: false}}",
            (vec![41, 42], true, vec![]),
        ),
        (
            "tui_o: {turn: {channels: [42]}}",
            (vec![41, 42], true, vec![42]),
        ),
    ];
    for (writer, expected) in cases {
        assert_eq!(select(writer), expected, "{writer:?}");
    }
}

// A cluster node without both ids keeps booting with the default selection off; an explicit
// selection still refuses the boot.
#[test]
fn tui_o_default_selection_turns_off_on_a_cluster_node_without_ids() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let cluster = "cluster: {enabled: true, instance_id: a}\ntui_o:";
    let load =
        |writer: &str| load_fixture(root.path(), &format!("{cluster} {writer}"), BINDINGS, "{}");
    let (channels, _, offered) = boot_selection(&load("{}").expect("default selection boots"));
    assert_eq!((channels, offered), (vec![], vec![]));
    for writer in ["{writer: {all_tui: true}}", "{writer: {channels: [41]}}"] {
        let error = load(writer).unwrap_err();
        assert!(
            format!("{error:#}").contains("cluster.gateway_preferred_instance_id"),
            "{writer}: {error:#}"
        );
    }
}
