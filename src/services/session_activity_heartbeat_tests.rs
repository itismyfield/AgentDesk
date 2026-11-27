//! A local session whose host marker is not tmux is read by its heartbeat, never by a tmux probe.
use std::collections::HashSet;

use chrono::Utc;

use crate::services::session_activity::SessionActivityResolver;

fn ts(secs_ago: i64) -> String {
    (Utc::now() - chrono::Duration::seconds(secs_ago))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

// A local session whose host marker is not tmux is read by its heartbeat like a remote
// one, never by a tmux probe of its name; an unmarked one is probed as in main.
#[cfg(unix)]
#[test]
fn resolve_reads_another_hosts_local_session_by_heartbeat() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = crate::services::discord::host_defer_gate::tests::ScriptedTmux::install();
    let (herdr, legacy) = ("AgentDesk-claude-p4c2-herdr", "AgentDesk-claude-p4c2-tmux");
    let marker = crate::services::tmux_common::session_temp_path(herdr, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(&marker, "herdr").unwrap();
    let mut resolver = SessionActivityResolver::new();
    resolver.local_host_aliases = Some(HashSet::from(["localbox".to_string()]));
    let mut resolve = |name: &str, heartbeat: i64| {
        let key = format!("localbox:{name}");
        let state = resolver.resolve(Some(&key), Some("turn_active"), None, Some(&ts(heartbeat)));
        state.is_working
    };

    assert!(resolve(herdr, 5), "a recent heartbeat keeps it working");
    assert!(!resolve(herdr, 900), "a stale heartbeat reads not working");
    assert_eq!(tmux.take_calls(), Vec::<String>::new(), "no tmux probe");
    assert!(
        !resolve(legacy, 5),
        "main reads the absent tmux pane as not live"
    );
    assert!(!tmux.take_calls().is_empty(), "main probes tmux");
}
