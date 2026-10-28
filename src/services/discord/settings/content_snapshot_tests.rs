use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn peer_guidance_reloads_snapshot_even_when_disk_mtime_already_matches_cache() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "agents:\n  - {id: self, name: Self, provider: claude}\n  - {id: peer-a, name: Snapshot Peer, provider: claude, peer_keywords: [snapshot-domain]}\n",
            "agents:\n  - {id: self, name: Self, provider: claude}\n  - {id: peer-b, name: Disk Peer, provider: claude, peer_keywords: [disk-domain]}\n",
        );
        let modified = fs::metadata(&fixture.path).unwrap().modified().unwrap();
        let first = render_peer_agent_guidance("self").unwrap();
        assert!(
            first.contains("Snapshot Peer") && first.contains("snapshot-domain"),
            "{first}"
        );
        assert!(!first.contains("Disk Peer"), "{first}");
        assert_eq!(
            render_peer_agent_guidance("self").as_deref(),
            Some(first.as_str())
        );
        fixture.reload();
        assert_eq!(
            fs::metadata(&fixture.path).unwrap().modified().unwrap(),
            modified
        );
        let second = render_peer_agent_guidance("self").unwrap();
        assert!(
            second.contains("Disk Peer") && second.contains("disk-domain"),
            "{second}"
        );
        assert!(!second.contains("Snapshot Peer"), "{second}");
    });
}
