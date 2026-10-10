use std::collections::BTreeSet;

use super::plan::{InputMode, InputSelection};
use crate::config::Config;

fn config(turn: &str, writer: &str, bindings: &str) -> Config {
    serde_yaml::from_str(&format!(
        "server: {{}}\ndata: {{}}\n\
         agents:\n  - id: scope\n    name: Scope\n    channels:\n      {bindings}\n\
         tui_o:\n  turn: {{{turn}}}\n  writer: {{{writer}}}\n"
    ))
    .unwrap()
}

const BINDINGS: &str = "claude: {id: '41', runtime: tui}\n      codex: {id: '42', runtime: tui}";

fn selection(channels: &[u64]) -> InputSelection {
    InputSelection {
        mode: InputMode::Ledger,
        channels: Some(channels.iter().copied().collect()),
    }
}

#[test]
fn g2_selection_requires_explicit_list_even_all_owned() {
    for turn in ["channels: [41]", "all_owned: true"] {
        let config = config(turn, "all_tui: true", BINDINGS);
        let omitted_or_null = InputSelection {
            mode: InputMode::Ledger,
            channels: None,
        };
        assert!(
            omitted_or_null.validate(&config).is_err(),
            "missing list must not inherit the output selection"
        );
    }
}

#[test]
fn g2_selection_default_legacy_and_explicit_empty_select_nothing() {
    let config = config("all_owned: true", "channels: [0]", BINDINGS);
    assert!(
        InputSelection::default()
            .validate(&config)
            .unwrap()
            .is_empty()
    );
    let legacy_with_remaining_list = InputSelection {
        mode: InputMode::Legacy,
        channels: Some(BTreeSet::from([41])),
    };
    assert!(
        legacy_with_remaining_list
            .validate(&config)
            .unwrap()
            .is_empty()
    );
    assert!(selection(&[]).validate(&config).unwrap().is_empty());
}

#[test]
fn g2_selection_explicit_list_preserves_output_scope_and_fresh_candidates() {
    let config = config("all_owned: true", "all_tui: true", BINDINGS);
    for (channels, expected) in [
        (vec![41], vec![(41, "claude")]),
        (vec![42], vec![(42, "codex")]),
        (vec![42, 41, 41], vec![(41, "claude"), (42, "codex")]),
    ] {
        let selected = selection(&channels).validate(&config).unwrap();
        assert_eq!(selected.into_iter().collect::<Vec<_>>(), expected);
    }
    let output = config.tui_o.as_ref().unwrap();
    assert!(output.turn.all_owned && output.writer.all_tui);
    assert!(output.turn.channels.is_empty());
}

#[test]
fn g2_selection_rejects_zero_unregistered_and_outside_turn_or_writer() {
    let both = config("channels: [41, 42]", "channels: [41, 42]", BINDINGS);
    let one_turn = config("channels: [41]", "channels: [41, 42]", BINDINGS);
    let one_writer = config("all_owned: true", "channels: [41]", BINDINGS);
    for (config, channel) in [(&both, 0), (&both, 99), (&one_turn, 42), (&one_writer, 42)] {
        assert!(selection(&[channel]).validate(config).is_err(), "{channel}");
    }
}

#[test]
fn g2_selection_rejects_unsupported_and_uncertain_config_binding() {
    let cases = [
        "claude: {id: '41', runtime: pipe}",
        "gemini: {id: '41', runtime: tui}",
        "claude: {id: '41', runtime: unknown}",
        "claude: {id: '41', runtime: tui}\n      codex: {id: '41', runtime: tui}",
    ];
    for bindings in cases {
        let config = config("all_owned: true", "all_tui: true", bindings);
        assert!(selection(&[41]).validate(&config).is_err(), "{bindings}");
    }
}

#[test]
fn g2_selection_honors_provider_runtime_setting() {
    let mut config = config("all_owned: true", "all_tui: true", "claude: {id: '41'}");
    config.providers = serde_yaml::from_str("{' CLAUDE ': {runtime: pipe}}").unwrap();
    assert!(selection(&[41]).validate(&config).is_err());
    config.providers = serde_yaml::from_str("{' CLAUDE ': {runtime: tui}}").unwrap();
    assert_eq!(selection(&[41]).validate(&config).unwrap()[&41], "claude");
}
