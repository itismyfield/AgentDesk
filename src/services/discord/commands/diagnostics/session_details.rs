use super::super::super::inflight::InflightTurnState;

pub(super) async fn build_session_details(
    tmux_name: Option<&str>,
    pane_alive: bool,
    cached_cwd: Option<&str>,
    cached_session_id: Option<&str>,
    raw_session_id: Option<&str>,
    inflight: Option<&InflightTurnState>,
) -> String {
    let binding = tmux_name.and_then(crate::services::tui_prompt_dedupe::peek_tmux_runtime_binding);
    let session_id = binding
        .as_ref()
        .and_then(|binding| binding.session_id.as_deref())
        .or_else(|| inflight.and_then(|state| state.session_id.as_deref()))
        .or(raw_session_id)
        .or(cached_session_id)
        .unwrap_or("없음");
    let cwd = cached_cwd.map(str::to_owned);
    #[cfg(unix)]
    let cwd = if pane_alive && let Some(name) = tmux_name {
        let name = name.to_owned();
        let live_cwd = tokio::task::spawn_blocking(move || {
            crate::services::tmux_diagnostics::tmux_session_pane_cwd(&name)
        })
        .await
        .ok()
        .flatten();
        live_cwd.or(cwd)
    } else {
        cwd
    };
    #[cfg(not(unix))]
    let _ = pane_alive;
    let transcript = binding
        .as_ref()
        .map(|binding| binding.output_path.clone())
        .or_else(|| inflight.and_then(|state| state.output_path.clone()))
        .or_else(|| {
            tmux_name.map(crate::services::cluster::session_matcher::expected_rollout_path_for)
        });
    let transcript_state = match transcript.as_deref().filter(|path| !path.trim().is_empty()) {
        Some(path) => match tokio::fs::metadata(path).await {
            Ok(metadata) if metadata.is_file() => "있음".to_owned(),
            Ok(_) => "없음".to_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "없음".to_owned(),
            Err(error) => format!("확인 불가: {error}"),
        },
        None => "없음".to_owned(),
    };
    let mut text = format!(
        "- cwd: `{}`\n- session_id: `{session_id}`\n- transcript: `{transcript_state}`",
        cwd.as_deref().unwrap_or("없음"),
    );
    if let Some(path) = transcript {
        text.push_str(&format!("\n- transcript_path: `{path}`"));
    }
    if let Some(state) = inflight {
        text.push_str(&format!(
            "\n- inflight: saved, started `{}`, updated `{}`, offset `{}`\n- placeholder_msg: `{}`\n- user_text: `{}`",
            state.started_at,
            state.updated_at,
            state.last_offset,
            state.current_msg_id,
            super::super::super::formatting::truncate_str(&state.user_text, 80),
        ));
    } else {
        text.push_str("\n- inflight: `없음`");
    }
    text
}
