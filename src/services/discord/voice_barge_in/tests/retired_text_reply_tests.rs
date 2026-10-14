//! Unix only: the foreground model is a shell stub at `AGENTDESK_CLAUDE_PATH`.
#![cfg(unix)]

use super::*;

/// The text reply refuses a retired foreground (provider, no channel) before the model with one
/// notice; shared provider and target channel are no authority, and otherwise the stub answers.
#[tokio::test(flavor = "current_thread")]
async fn voice_text_reply_refuses_a_retired_foreground_before_the_model() {
    use crate::services::discord::health::legacy_supervision::test_support::MockDiscord;
    use crate::services::provider_hosting::{RetiredRuntimeError, retired_runtime_for_tests};
    use std::os::unix::fs::PermissionsExt;
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let dir = tempfile::tempdir().unwrap();
    let (calls, claude) = (dir.path().join("claude.calls"), dir.path().join("claude"));
    let stub = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n\
         if [ \"$1\" = --version ]; then echo '2.1.0 (Claude Code)'; exit 0; fi\n\
         cat >/dev/null\necho 'stub voice reply'\n",
        calls.display()
    );
    std::fs::write(&claude, stub).unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (voice, background) = (6_570_671_u64, 6_570_672_u64);
    let mut agent = test_agent("codex");
    agent.voice.channel_id = Some(voice.to_string());
    agent.channels = crate::config::AgentChannels::new()
        .with("claude", crate::config::AgentChannel::from("6570673"))
        .with("codex", crate::config::AgentChannel::from("6570672"));
    let mut config = crate::config::Config::default();
    config.agents = vec![agent];
    // Both route readers, the cached one and the fenced fresh one, see the same agent.
    let yaml = dir.path().join("agentdesk.yaml");
    std::fs::write(&yaml, serde_yaml::to_string(&config).unwrap()).unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let (_bin, _config) = (
        set("AGENTDESK_CLAUDE_PATH", &claude),
        set("AGENTDESK_CONFIG", &yaml),
    );
    let runtime = enabled_runtime();
    runtime.config_cache.seed(Instant::now(), Arc::new(config));
    let discord = MockDiscord::start().await;
    let reply = |retired: (ProviderKind, Option<u64>)| {
        let (runtime, http) = (&runtime, discord.http.clone());
        async move {
            let _retired = retired_runtime_for_tests::retire(&retired.0, retired.1);
            let text = "지금 상태 알려줘";
            let shared_provider = ProviderKind::Codex;
            let channel = ChannelId::new(voice);
            runtime
                .try_handle_voice_channel_text_reply(&http, channel, text, &shared_provider)
                .await
        }
    };

    assert!(reply((ProviderKind::Claude, None)).await);
    let notices = discord.requests_for(voice);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains(RetiredRuntimeError::CODE),
        "{notices:?}"
    );
    assert!(
        !calls.exists(),
        "the foreground model ran for a retired runtime"
    );

    assert!(reply((ProviderKind::Codex, None)).await);
    assert!(reply((ProviderKind::Claude, Some(background))).await);
    let posts = discord.requests_for(voice);
    assert_eq!(posts.len(), 3, "{posts:?}");
    let answered = |post: &String| post.contains("stub voice reply");
    assert!(posts[1..].iter().all(answered), "{posts:?}");
    assert!(
        calls.exists(),
        "a supported foreground runtime ran the model"
    );
}
