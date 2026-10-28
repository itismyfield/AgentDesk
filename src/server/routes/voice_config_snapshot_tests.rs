use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn runtime_voice_uses_snapshot_while_editor_uses_fresh_disk() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "voice: {active_agent_ttl_seconds: 41}",
            "voice: {active_agent_ttl_seconds: 73}",
        );
        let fallback = crate::voice::VoiceConfig {
            active_agent_ttl_seconds: 999,
            ..Default::default()
        };
        assert_eq!(live_voice_config(&fallback).active_agent_ttl_seconds, 41);
        assert_eq!(
            load_editable_config()
                .unwrap()
                .0
                .voice
                .active_agent_ttl_seconds,
            73
        );
        fixture.reload();
        assert_eq!(live_voice_config(&fallback).active_agent_ttl_seconds, 73);
    });
}

#[test]
fn invalid_disk_voice_does_not_replace_the_runtime_snapshot() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "voice: {active_agent_ttl_seconds: 41}",
            "voice: {active_agent_ttl_seconds: 73}",
        );
        std::fs::write(&fixture.path, "voice: [").unwrap();
        assert_eq!(
            live_voice_config(&crate::voice::VoiceConfig::default()).active_agent_ttl_seconds,
            41
        );
        assert!(load_editable_config().is_err());
    });
}
