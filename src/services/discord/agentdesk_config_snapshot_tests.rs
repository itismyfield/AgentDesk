use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

const A: &str = "agents:\n  - {id: self, name: Self, provider: claude}\n  - {id: peer-a, name: Peer A, provider: claude}\n";
const B: &str = "agents:\n  - {id: disk-b, name: Disk B, provider: claude}\n";

#[test]
fn shared_config_uses_snapshot_and_editor_keeps_fresh_disk() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(A, B);
        let config = load_agentdesk_config().unwrap();
        assert_eq!(
            config.agents.len(),
            2,
            "the published config retains both agents"
        );
        assert_eq!(config.agents[1].id, "peer-a");
        assert_eq!(
            load_agent_setup_config(fixture.root()).unwrap().0.agents[0].id,
            "disk-b"
        );
        fixture.reload();
        assert_eq!(load_agentdesk_config().unwrap().agents[0].id, "disk-b");
    });
}

#[test]
fn shared_config_keeps_standalone_missing_and_invalid_file_contract() {
    isolated(|| {
        let fixture = Fixture::new();
        assert!(load_agentdesk_config().is_none());
        fixture.write(B);
        assert_eq!(load_agentdesk_config().unwrap().agents[0].id, "disk-b");
        std::fs::write(&fixture.path, "agents: [").unwrap();
        assert!(load_agentdesk_config().is_none());
    });
}
