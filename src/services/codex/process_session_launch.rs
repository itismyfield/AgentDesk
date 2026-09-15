//! Direct-process Codex session launch (Windows and tmux-less hosts).

use super::*;

/// Execute Codex via ProcessBackend (direct child process, no tmux).
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_streaming_local_process_codex(
    prompt: &str,
    model: Option<&str>,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    session_name: &str,
    developer_instructions: Option<&str>,
    compact_token_limit: Option<u64>,
    force_fresh_provider_session: bool,
) -> Result<(), String> {
    use crate::services::session_backend::{ProcessBackend, SessionBackend, SessionConfig};

    let output_path = format!(
        "{}/agentdesk-{}.jsonl",
        std::env::temp_dir().display(),
        session_name
    );
    let prompt_path = format!(
        "{}/agentdesk-{}.prompt",
        std::env::temp_dir().display(),
        session_name
    );

    let overlay = crate::services::discord::org_schema::overlay_from_tmux_session(
        ProviderKind::Codex,
        session_name,
    )?;
    let profile_matches =
        crate::services::session_backend::auth_profiles::prepare(session_name, &overlay.profile_id);
    let session_id = profile_matches.then_some(session_id).flatten();
    let force_fresh_provider_session = force_fresh_provider_session || !profile_matches;

    // Check for existing process session
    let process_session_alive = process_session_is_alive(session_name);
    if should_reuse_existing_provider_session(process_session_alive, force_fresh_provider_session) {
        // Snapshot file length BEFORE sending input to avoid race:
        // Codex wrapper appends JSONL immediately on stdin, so a fast
        // response could be written before we read the offset.
        let start_offset = std::fs::metadata(&output_path)
            .map(|m| m.len())
            .unwrap_or(0);

        let encoded = format!(
            "{}{}",
            TMUX_PROMPT_B64_PREFIX,
            BASE64_STANDARD.encode(prompt.as_bytes())
        );
        send_process_session_input(session_name, &encoded)?;
        let read_result = read_output_file_until_result(
            &output_path,
            start_offset,
            sender.clone(),
            cancel_token,
            process_session_probe(session_name),
        )?;

        fold_read_output_result(
            read_result,
            |offset| {
                let _ = sender.send(StreamMessage::ProcessReady {
                    output_path: output_path.to_string(),
                    session_name: session_name.to_string(),
                    last_offset: offset,
                });
            },
            |_| {
                let _ = sender.send(StreamMessage::Done {
                    result: "⚠ 세션이 종료되었습니다.".to_string(),
                    session_id: None,
                });
                remove_process_session(session_name);
            },
        );
        return Ok(());
    }

    if force_fresh_provider_session && process_session_alive {
        if let Some(handle) = remove_process_session(session_name) {
            terminate_process_handle(handle);
        }
    }

    // Clean up and create new session
    let _ = std::fs::remove_file(&output_path);
    let _ = std::fs::remove_file(&prompt_path);
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("Failed to write prompt file: {}", e))?;

    let resolution = resolve_codex_binary();
    let codex_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Codex CLI not found".to_string())?;
    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;
    let launch_options = CodexLaunchOptions::new(prompt)
        .with_resume_session_id(session_id)
        .with_developer_instructions(developer_instructions)
        .with_model(model)
        .with_reasoning_effort(codex_reasoning_effort_from_env().as_deref())
        .with_compact_token_limit(compact_token_limit)
        .with_readonly_mode(false)
        .with_fast_mode_enabled(fast_mode_enabled)
        .with_goals_enabled(goals_enabled)
        .with_cwd(Some(working_dir));

    let config = SessionConfig {
        session_name: session_name.to_string(),
        working_dir: working_dir.to_string(),
        agentdesk_exe: exe.display().to_string(),
        output_path: output_path.clone(),
        prompt_path: prompt_path.clone(),
        wrapper_subcommand: "codex-tmux-wrapper".to_string(),
        wrapper_args: build_codex_wrapper_cli_args(&launch_options, &codex_bin),
        env_vars: crate::services::provider_auth_profile::merge_overlay_env(
            resolution
                .exec_path
                .clone()
                .map(|path| vec![("PATH".to_string(), path)])
                .unwrap_or_default(),
            &overlay,
        ),
        unset_env: crate::services::provider_auth_profile::overlay_unset_keys(&overlay),
    };

    let backend = ProcessBackend::new();
    let handle = backend.create_session(&config)?;
    let handle = crate::services::session_backend::auth_profiles::record_launch(
        session_name,
        &overlay.profile_id,
        handle,
    )?;

    register_child_pid(cancel_token.as_deref(), handle.pid());

    insert_process_session(session_name.to_string(), handle);

    let read_result = read_output_file_until_result(
        &output_path,
        0,
        sender.clone(),
        cancel_token,
        process_session_probe(session_name),
    )?;

    fold_read_output_result(
        read_result,
        |offset| {
            let _ = sender.send(StreamMessage::ProcessReady {
                output_path,
                session_name: session_name.to_string(),
                last_offset: offset,
            });
        },
        |_| {
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 프로세스가 종료되었습니다.".to_string(),
                session_id: None,
            });
            remove_process_session(session_name);
        },
    );

    Ok(())
}
