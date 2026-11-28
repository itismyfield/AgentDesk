use crate::services::discord::{DiscordSession, session_runtime::rebind_channel_session};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;

#[tokio::test]
async fn rebound_session_binding_reaches_claude_resume_launch_args() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = ChannelId::new(4_794_001);
    let cwd_dir = tempfile::tempdir().expect("resume cwd");
    let cwd = cwd_dir.path().to_str().expect("utf8 cwd");
    let session_id = "01234567-89ab-cdef-0123-456789abcdef";

    rebind_channel_session(&shared, &ProviderKind::Claude, channel_id, cwd, session_id).await;

    let (launch_session_id, launch_cwd) = {
        let mut data = shared.core.lock().await;
        let (session_id, _, current_path) =
            crate::services::discord::router::load_session_runtime_state(
                &mut data.sessions,
                channel_id,
            )
            .expect("rebound runtime state");
        (session_id.expect("provider session id"), current_path)
    };
    let config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
        tmux_session_name: "AgentDesk-claude-resume-binding".to_string(),
        working_dir: std::path::PathBuf::from(&launch_cwd),
        claude_bin: crate::services::claude_command::ClaudeBinary::from_tmux_wrapper_argv("claude"),
        agentdesk_exe: std::path::PathBuf::from("agentdesk"),
        hook_endpoint: "http://127.0.0.1:49152".to_string(),
        session_id: launch_session_id,
        system_prompt: None,
        model: None,
        resume: true,
    };
    let args = crate::services::claude_tui::session::build_claude_tui_args(
        &config,
        std::path::Path::new("/runtime/settings.json"),
    );

    assert_eq!(config.working_dir, std::path::Path::new(cwd));
    assert!(args.windows(2).any(|pair| pair == ["--resume", session_id]));
}

#[test]
fn absent_resume_binding_keeps_existing_launch_selection() {
    let cwd_dir = tempfile::tempdir().expect("current cwd");
    let expected_cwd = cwd_dir.path().display().to_string();
    let mut sessions = std::collections::HashMap::new();
    sessions.insert(
        ChannelId::new(4_794_002),
        DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: Some(expected_cwd.clone()),
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            channel_id: Some(4_794_002),
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
        },
    );

    let (session_id, _, cwd) = crate::services::discord::router::load_session_runtime_state(
        &mut sessions,
        ChannelId::new(4_794_002),
    )
    .expect("existing runtime state");
    assert_eq!(session_id, None);
    assert_eq!(cwd, expected_cwd);
}
