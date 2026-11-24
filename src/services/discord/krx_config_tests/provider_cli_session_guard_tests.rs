//! Session-guard host test that scripts tmux through the discord host-defer gate, moved out of KRX
//! provider_cli.

use crate::services::provider_cli::io::save_launch_artifact;
use crate::services::provider_cli::registry::LaunchArtifact;
use crate::services::provider_cli::session_guard::*;
use chrono::Utc;

// An old-channel launch whose tmux session another host's marker claims reads as unknown,
// never as ended, and is not probed by name; an unmarked one is probed as in main.
#[test]
fn another_hosts_launch_reads_unknown_not_ended() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = crate::services::discord::host_defer_gate::tests::ScriptedTmux::install();
    let launches = tempfile::tempdir().expect("launch root");
    let (herdr, legacy) = (
        "AgentDesk-claude-p4c2-guard-h",
        "AgentDesk-claude-p4c2-guard-t",
    );
    let marker = crate::services::tmux_common::session_temp_path(herdr, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(&marker, "herdr").unwrap();
    for (agent, name) in [("agent-h", herdr), ("agent-t", legacy)] {
        let artifact = LaunchArtifact {
            provider: "claude".to_string(),
            agent_id: Some(agent.to_string()),
            channel_id: None,
            session_key: Some(name.to_string()),
            channel: "current".to_string(),
            cli_path: "/bin/claude".to_string(),
            canonical_path: "/bin/claude".to_string(),
            cli_version: "1".to_string(),
            process_id: None,
            tmux_session: Some(name.to_string()),
            launched_at: Utc::now(),
        };
        save_launch_artifact(launches.path(), &artifact).expect("launch artifact");
    }
    let agents = ["agent-h".to_string(), "agent-t".to_string()];
    let evaluation =
        evaluate_session_migration_guards(launches.path(), "claude", &agents, "candidate");
    let guard = |agent: &str| {
        let found = evaluation
            .guards
            .iter()
            .find(|guard| guard.agent_id == agent);
        found.expect("guard per agent").clone()
    };
    let (herdr_guard, legacy_guard) = (guard("agent-h"), guard("agent-t"));
    let status = |guard: &SessionMigrationGuard| guard.evidence["status"].clone();
    assert_eq!(status(&herdr_guard), "old_channel_session_host_unknown");
    assert_eq!(
        herdr_guard.safe_end_completed_at, None,
        "not reported ended"
    );
    assert_eq!(herdr_guard.evidence["tmux_live_pane"], "host_unsupported");
    assert_eq!(status(&legacy_guard), "old_channel_session_not_active");
    let calls = tmux.take_calls();
    assert!(calls.iter().all(|call| !call.contains(herdr)), "{calls:?}");
    assert!(calls.iter().any(|call| call.contains(legacy)), "{calls:?}");
}
