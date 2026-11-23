use super::*;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, NODE, PANE, SESSION};

fn on(node: &str, mut row: InflightTurnState) -> InflightTurnState {
    let locator = serde_json::json!({"host_kind": "herdr", "host_session_id": SESSION,
        "pane": PANE, "execution_node": node});
    row.host_locator = Some(serde_json::from_value(locator).unwrap());
    row
}

// Past its age a Herdr row, fresh or restart-cut, goes once its endpoint no longer lists its pane;
// a listed pane, an empty or unreachable server, another node or a path-less row keep it.
#[test]
fn stale_herdr_row_goes_once_its_recorded_pane_is_gone() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let name = "AgentDesk-claude-i8343h-herdr";
    let marker = crate::services::tmux_common::session_temp_path(name, "host_kind");
    std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(&marker, "herdr").unwrap();
    let (rig, out) = (HerdrRig::start(), Some("/tmp/i8343h.jsonl".to_string()));
    let _registry = rig.registry_on_this_thread();
    #[rustfmt::skip]
    let row = on(NODE, InflightTurnState::new(ProviderKind::Claude, 8_343_801, None, 1, 2, 3,
        "herdr turn".to_string(), None, Some(name.to_string()), out, None, 0));
    let mut cut = row.clone();
    (cut.restart_mode, cut.restart_generation) = (Some(InflightRestartMode::DrainRestart), Some(6));
    let (age, drain) = (INFLIGHT_MAX_AGE_SECS + 1, DRAIN_RESTART_MAX_AGE_SECS + 1);
    let stale = |row: &InflightTurnState, age| stale_removal_reason(row, age, 7).is_some();
    let removed = || (stale(&row, age), stale(&cut, drain));
    rig.show_panes(&[PANE, "w1-2"]);
    assert_eq!(removed(), (false, false), "listed pane");
    rig.show_panes(&[]);
    assert_eq!(removed(), (false, false), "server listing no pane");
    rig.show_panes(&["w1-2"]);
    assert!(!stale(&on("other-node", row.clone()), age), "other node");
    let path_less = InflightTurnState {
        output_path: None,
        ..row.clone()
    };
    assert!(!stale(&path_less, age), "path-less row");
    assert_eq!(removed(), (true, true), "closed pane");
    drop(rig);
    assert_eq!(removed(), (false, false), "unreachable server");
}
