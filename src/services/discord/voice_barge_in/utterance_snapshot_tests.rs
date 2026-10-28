use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn cached_voice_config_keeps_snapshot_and_existing_five_second_ttl() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "server: {}\nvoice: {active_agent_ttl_seconds: 41}\n",
            "server: {}\nvoice: {active_agent_ttl_seconds: 73}\n",
        );
        let runtime = VoiceBargeInRuntime::disabled();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let first = runtime.cached_config().await;
            assert_eq!(first.voice.active_agent_ttl_seconds, 41);
            fixture.reload();
            let cached = runtime.cached_config().await;
            assert!(Arc::ptr_eq(&first, &cached));
            assert_eq!(cached.voice.active_agent_ttl_seconds, 41);
            runtime
                .config_cache
                .store(Instant::now() - Duration::from_secs(6), cached);
            let refreshed = runtime.cached_config().await;
            assert_eq!(refreshed.voice.active_agent_ttl_seconds, 73);
            assert!(!Arc::ptr_eq(&first, &refreshed));
        });
    });
}

#[test]
fn completed_utterance_source_uses_the_installed_agent_mapping() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "server: {}\nvoice: {lobby_channel_id: '41'}\nagents: [{id: reader, name: Reader, provider: claude, channels: {claude: '77'}, voice: {channel_id: '41'}}]\n",
            "server: {}\nvoice: {lobby_channel_id: '42'}\nagents: [{id: reader, name: Reader, provider: claude, channels: {claude: '77'}, voice: {channel_id: '42'}}]\n",
        );
        let (config, channel) = utterance_source_config(ChannelId::new(77));
        assert_eq!(channel, ChannelId::new(41));
        assert_eq!(config.voice.lobby_channel_id.as_deref(), Some("41"));
        assert!(
            config
                .agents
                .iter()
                .any(|agent| agent_voice_matches_channel(agent, channel))
        );
        fixture.reload();
        let (config, channel) = utterance_source_config(ChannelId::new(77));
        assert_eq!(channel, ChannelId::new(42));
        assert_eq!(config.voice.lobby_channel_id.as_deref(), Some("42"));
    });
}

#[test]
fn utterance_source_selection_stays_after_transcription() {
    let body = include_str!("utterance_pipeline.rs")
        .split_once("pub(in crate::services::discord) async fn process_completed_utterance(")
        .unwrap()
        .1;
    let transcribe = body.find(".transcribe_completed_utterance(").unwrap();
    let select = body.find("utterance_source_config(channel_id)").unwrap();
    let empty = body.find("if transcript.is_empty()").unwrap();
    let binding = body.find("super::settings::resolve_role_binding(").unwrap();
    assert!(transcribe < select && select < empty && empty < binding);
}
