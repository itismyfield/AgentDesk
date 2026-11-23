use super::*;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, NODE, PANE, SESSION};
use crate::services::session_host::{HostKind, HostedRuntimeLocator};

fn herdr_row(name: &str, node: &str) -> InflightTurnState {
    let (tmux, out) = (
        Some(name.to_string()),
        Some("/tmp/i8343h.jsonl".to_string()),
    );
    let text = "herdr turn".to_string();
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        8_343_801,
        None,
        1,
        2,
        3,
        text,
        None,
        tmux,
        out,
        None,
        0,
    );
    let locator = HostedRuntimeLocator {
        execution_node: Some(node.to_string()),
        host_kind: HostKind::Herdr,
        host_session_id: SESSION.to_string(),
        pane: Some(PANE.to_string()),
    };
    row.host_locator = Some(super::super::host_locator::PersistedHostLocator::Known(
        locator,
    ));
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
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let row = herdr_row(name, NODE);
    let mut cut = row.clone();
    (cut.restart_mode, cut.restart_generation) = (Some(InflightRestartMode::DrainRestart), Some(6));
    let removed = || {
        let fresh = stale_removal_reason(&row, INFLIGHT_MAX_AGE_SECS + 1, 7).is_some();
        (
            fresh,
            stale_removal_reason(&cut, DRAIN_RESTART_MAX_AGE_SECS + 1, 7).is_some(),
        )
    };
    rig.show_panes(&[PANE, "w1-2"]);
    assert_eq!(removed(), (false, false), "listed pane");
    rig.show_panes(&[]);
    assert_eq!(removed(), (false, false), "server listing no pane");
    rig.show_panes(&["w1-2"]);
    let other = herdr_row(name, "other-node");
    assert!(stale_removal_reason(&other, INFLIGHT_MAX_AGE_SECS + 1, 7).is_none());
    let path_less = InflightTurnState {
        output_path: None,
        ..row.clone()
    };
    assert!(stale_removal_reason(&path_less, INFLIGHT_MAX_AGE_SECS + 1, 7).is_none());
    assert_eq!(removed(), (true, true), "closed pane");
    drop(rig);
    assert_eq!(removed(), (false, false), "unreachable server");
}
