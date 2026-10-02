use super::*;
use crate::services::tmux_common::session_temp_path;

fn mark(name: &str, host: &str) {
    let path = session_temp_path(name, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
    std::fs::write(path, host).unwrap();
}

// A launch that marks or lists a pane on Herdr after the watcher started is seen at the next
// re-check, the snapshot never falls back from Herdr, and nothing else raises it.
#[test]
fn a_watcher_host_rises_to_herdr_only_on_a_herdr_marker_or_a_listed_pane() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let names = [
        "AgentDesk-claude-p8-marked",
        "AgentDesk-claude-p8-listed",
        "AgentDesk-claude-p8-tmux",
    ];
    let [marked, listed, tmux] = names;
    let snapshots = names.map(|_| HostSnapshot::new(WatchHost::Legacy));
    let read = |snapshots: &[HostSnapshot; 3]| {
        let hosts = snapshots.iter().zip(names);
        hosts
            .map(|(snapshot, name)| snapshot.refresh_sync(name))
            .collect::<Vec<_>>()
    };
    assert_eq!(read(&snapshots), [WatchHost::Legacy; 3]);

    mark(marked, "herdr");
    crate::services::tui_prompt_dedupe::install_herdr_execution(listed, "p8-nonce");
    mark(tmux, "tmux");
    let raised = [WatchHost::Herdr, WatchHost::Herdr, WatchHost::Legacy];
    assert_eq!(read(&snapshots), raised);

    std::fs::remove_file(session_temp_path(marked, "host_kind")).unwrap();
    crate::services::tui_prompt_dedupe::withhold_herdr_execution(listed, Some("p8-nonce"));
    assert_eq!(read(&snapshots), raised, "a raised snapshot stays on Herdr");

    let unverified = HostSnapshot::new(WatchHost::Unverified);
    assert_eq!(unverified.refresh_sync(tmux), WatchHost::Unverified);
}
