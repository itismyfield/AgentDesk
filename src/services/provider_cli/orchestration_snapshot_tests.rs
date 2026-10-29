use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn configured_provider_agents_use_validated_snapshot() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "agents: [{id: snapshot-a, name: Snapshot A, provider: claude}]",
            "agents: [{id: disk-b, name: Disk B, provider: claude}]",
        );
        assert_eq!(
            configured_provider_agents("claude")
                .into_iter()
                .map(|a| a.agent_id)
                .collect::<Vec<_>>(),
            ["snapshot-a"]
        );
        fixture.reload();
        assert_eq!(
            configured_provider_agents("claude")
                .into_iter()
                .map(|a| a.agent_id)
                .collect::<Vec<_>>(),
            ["disk-b"]
        );
    });
}

#[test]
fn configured_provider_agents_preserve_standalone_file_fallback() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.write("agents: [{id: disk-b, name: Disk B, provider: claude}]");
        assert_eq!(configured_provider_agents("claude")[0].agent_id, "disk-b");
    });
}
