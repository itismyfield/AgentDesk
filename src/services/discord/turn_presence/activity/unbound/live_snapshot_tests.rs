use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn unbound_registrations_use_snapshot_agents() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "agents: [{id: snapshot-a, name: Snapshot A, provider: claude, channels: {claude: {id: '41'}}}]",
            "agents: [{id: disk-b, name: Disk B, provider: claude, channels: {claude: {id: '73'}}}]",
        );
        assert_eq!(
            registrations()
                .unwrap()
                .into_iter()
                .map(|b| b.channel_id)
                .collect::<Vec<_>>(),
            [41]
        );
        fixture.reload();
        assert_eq!(
            registrations()
                .unwrap()
                .into_iter()
                .map(|b| b.channel_id)
                .collect::<Vec<_>>(),
            [73]
        );
    });
}

#[test]
fn unbound_registrations_keep_snapshot_when_disk_yaml_is_invalid() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install("agents: [{id: snapshot-a, name: Snapshot A, provider: claude, channels: {claude: {id: '41'}}}]", "agents: []");
        std::fs::write(&fixture.path, "agents: [").unwrap();
        assert_eq!(
            registrations()
                .unwrap()
                .into_iter()
                .map(|b| b.channel_id)
                .collect::<Vec<_>>(),
            [41]
        );
    });
}
