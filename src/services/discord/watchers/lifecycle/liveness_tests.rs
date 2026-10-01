//! The watcher's liveness answer against the local host evidence.

use std::path::Path;

use super::*;
use crate::services::session_host::test_support::InjectedLivenessGuard;
use crate::services::session_host::{HostLiveness, HostSessionRef};
use crate::services::tmux_common::{session_dead_marker_path, session_temp_path};

fn write(path: &str, text: &str) {
    std::fs::create_dir_all(Path::new(path).parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

// Only a pane tmux confirms dead, or the wrapper's `.pane_dead` after a failed probe, reads
// dead. A session whose marker names another host is never probed and keeps its files.
#[tokio::test]
async fn watcher_probe_reads_dead_only_on_a_confirmed_tmux_death() {
    use HostLiveness::{DeadOrAbsent, Live, ProbeError};
    let _root = crate::config::TestRuntimeRootGuard::new();
    // (`.host_kind`, injected pane, `.pane_dead` present, alive, `.pane_dead` left)
    let cases = [
        (None, Live, false, true, false),
        (None, DeadOrAbsent, false, false, false),
        (None, ProbeError, false, true, false),
        (None, ProbeError, true, false, true),
        (None, Live, true, true, false),
        (Some("tmux"), DeadOrAbsent, false, false, false),
        (Some("herdr"), DeadOrAbsent, true, true, true),
        (Some("zellij"), DeadOrAbsent, false, true, false),
    ];
    for (n, (host, pane, dead_marker, alive, marker_left)) in cases.into_iter().enumerate() {
        let name = format!("AgentDesk-claude-p4b1-watch-{n}");
        let _pane = InjectedLivenessGuard::set(HostSessionRef::tmux(&name), pane);
        if let Some(host) = host {
            write(&session_temp_path(&name, "host_kind"), host);
        }
        let dead_path = session_dead_marker_path(&name);
        if dead_marker {
            write(&dead_path, "");
        }
        let label = format!("{n}: {host:?} {pane:?} pane_dead={dead_marker}");
        assert_eq!(probe_tmux_session_liveness(&name).await, alive, "{label}");
        assert_eq!(Path::new(&dead_path).exists(), marker_left, "{label}");
    }
}
