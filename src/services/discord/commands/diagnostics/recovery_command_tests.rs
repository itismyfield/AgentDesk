use super::*;
use crate::services::discord::DiscordSession;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;

#[test]
fn clear_acknowledges_before_auth_and_cleanup() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/services/discord/commands/control.rs"
    ))
    .unwrap();
    let body = source
        .split("async fn cmd_clear(")
        .nth(1)
        .unwrap()
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    let defer = body.find("ctx.defer().await?").expect("clear must defer");
    assert!(defer < body.find("check_auth(").unwrap());
    assert!(defer < body.find("clear_channel_session_state(").unwrap());
    assert!(body.contains("ctx.say(report.reply()).await?"));
}

#[test]
fn skill_stop_delegates_to_the_same_stop_response() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/services/discord/commands/skill.rs"
    ))
    .unwrap();
    let body = source
        .split("\"stop\" => {")
        .nth(1)
        .unwrap()
        .split("\"pwd\" =>")
        .next()
        .unwrap();
    assert!(
        body.contains("super::control::run_stop(ctx).await"),
        "stop aliases must use the actual stop result and acknowledgement"
    );
}

#[tokio::test]
async fn status_shows_complete_session_cwd_and_missing_transcript() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(648_501);
    let sid = "session-with-more-than-twenty-four-characters";
    let cwd = dirs::home_dir()
        .unwrap()
        .join("status-test-workspace")
        .display()
        .to_string();
    shared.core.lock().await.sessions.insert(
        channel,
        DiscordSession {
            session_id: Some(sid.into()),
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: Some(cwd.clone()),
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(channel.get()),
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: shared.restart.current_generation,
        },
    );
    let text = build_status_report(&shared, &ProviderKind::Claude, channel).await;
    assert!(text.contains(&format!("session_id: `{sid}`")), "{text}");
    assert!(text.contains(&format!("cwd: `{cwd}`")), "{text}");
    assert!(text.contains("transcript: `없음`"), "{text}");
}

#[tokio::test]
async fn status_transcript_probe_checks_metadata_for_large_missing_and_directory_paths() {
    let root = tempfile::tempdir().unwrap();
    let transcript = root.path().join("large.jsonl");
    let file = std::fs::File::create(&transcript).unwrap();
    // Windows allocates set_len's full size unless the file is marked sparse first.
    #[cfg(windows)]
    {
        let result = std::process::Command::new("fsutil")
            .args(["sparse", "setflag"])
            .arg(&transcript)
            .output()
            .expect("mark the test transcript sparse");
        assert!(result.status.success(), "sparse flag failed: {result:?}");
    }
    file.set_len(32 * 1024 * 1024 * 1024).unwrap();
    drop(file);
    let mut state = crate::services::discord::inflight::InflightTurnState::new(
        ProviderKind::Claude,
        648_503,
        None,
        1,
        2,
        3,
        "pending prompt".into(),
        Some("inflight-session-with-a-complete-identifier".into()),
        None,
        Some(transcript.display().to_string()),
        None,
        0,
    );
    let details = super::session_details::build_session_details;
    let text = details(
        None,
        false,
        Some("/workspace"),
        Some("cached"),
        None,
        Some(&state),
    )
    .await;
    assert!(
        text.contains("session_id: `inflight-session-with-a-complete-identifier`"),
        "{text}"
    );
    assert!(text.contains("transcript: `있음`"), "{text}");
    assert!(text.contains(&transcript.display().to_string()));
    assert!(text.contains("inflight: saved"));
    assert!(text.contains("pending prompt"));
    std::fs::remove_file(&transcript).unwrap();
    let text = details(None, false, None, None, None, Some(&state)).await;
    assert!(text.contains("transcript: `없음`"), "{text}");
    state.output_path = Some(root.path().display().to_string());
    let text = details(None, false, None, None, None, Some(&state)).await;
    assert!(text.contains("transcript: `없음`"), "{text}");
}

#[cfg(unix)]
#[tokio::test]
async fn status_uses_the_bound_runtime_session_and_transcript() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::tui_prompt_dedupe::{
        TuiRuntimeBinding, register_launched_tmux_runtime_binding,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(648_504);
    let name = "status-runtime-binding";
    crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, name).await;
    shared
        .core
        .lock()
        .await
        .sessions
        .get_mut(&channel)
        .unwrap()
        .session_id = Some("old-cache".into());
    let tmux = ProviderKind::Claude.build_tmux_session_name(name);
    let root = tempfile::tempdir().unwrap();
    let transcript = root.path().join("bound.jsonl");
    std::fs::write(&transcript, "").unwrap();
    register_launched_tmux_runtime_binding(
        &tmux,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: transcript.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("live-bound-session-with-a-complete-identifier".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let text = build_status_report(&shared, &ProviderKind::Claude, channel).await;
    assert!(
        text.contains("session_id: `live-bound-session-with-a-complete-identifier`"),
        "{text}"
    );
    assert!(text.contains("transcript: `있음`"), "{text}");
    assert!(text.contains(&transcript.display().to_string()));
}
