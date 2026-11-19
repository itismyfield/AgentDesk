//! The channel selector follows a Claude session rotation: the cached provider session and the
//! stored selector move off the session the pane left onto the one it runs now.

use super::*;

/// Moves the channel's cached session onto `rotation`'s new session while it still names a
/// session this pane left. A cleared, empty or unrelated cache stays as it is. Returns the
/// sessions the stored selector may move off when the cache now names the new session.
async fn follow_cached_selector(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    rotation: &crate::services::tui_prompt_dedupe::ClaudeSessionRotation,
) -> Option<Vec<String>> {
    let new = rotation.new_session_id.as_str();
    let tmux = rotation.tmux_session_name.as_str();
    let mut data = shared.core.lock().await;
    let session = data.sessions.get_mut(&channel_id)?;
    let cached = session.session_id.clone().filter(|_| !session.cleared)?;
    let mut left: Vec<String> = rotation.old_session_id.iter().cloned().collect();
    if cached == new {
        return Some(left);
    }
    let resolve = crate::services::tui_prompt_dedupe::resolve_tmux_session_name;
    let left_by_pane =
        left.contains(&cached) || resolve("claude", &cached).as_deref() == Some(tmux);
    if !left_by_pane {
        return None;
    }
    session.restore_provider_session(Some(new.to_string()));
    tracing::info!(
        provider = "claude",
        channel_id = channel_id.get(),
        tmux_session_name = tmux,
        old_session_id = %cached,
        new_session_id = new,
        "moved the channel's cached provider session to the rotated Claude session"
    );
    left.push(cached);
    Some(left)
}

/// Moves the cache and then the stored selector. Returns false only when the stored selector
/// could not be written, so the caller keeps the rotation and tries again on the next pass.
pub(super) async fn follow_rotation_selector(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    rotation: &crate::services::tui_prompt_dedupe::ClaudeSessionRotation,
) -> bool {
    let Some(left) = follow_cached_selector(shared, channel_id, rotation).await else {
        return true;
    };
    let provider = ProviderKind::Claude;
    let session_key = super::super::super::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        &provider,
        &rotation.tmux_session_name,
    );
    let new = rotation.new_session_id.as_str();
    let Some(pool) = shared.pg_pool.as_ref() else {
        let save = super::super::super::adk_session::save_provider_session_id;
        save(
            &session_key,
            new,
            Some(new),
            &provider,
            channel_id,
            shared.api_port,
        )
        .await;
        return true;
    };
    let moved = crate::db::dispatched_sessions::rotation_selector::move_provider_selector_pg(
        pool,
        &session_key,
        &left,
        new,
    )
    .await;
    match moved {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(
                provider = "claude",
                channel_id = channel_id.get(),
                tmux_session_name = %rotation.tmux_session_name,
                new_session_id = new,
                error,
                "stored provider selector did not move to the rotated Claude session; retrying"
            );
            false
        }
    }
}

/// Applies a rotation still waiting for its settle pass to the channel's cache, so a turn that
/// starts before that pass resumes the session the pane runs now. Returns the cached session.
pub(in crate::services::discord) async fn follow_pending_rotation_selector(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    tmux_session_name: &str,
) -> Option<String> {
    let rotation =
        crate::services::tui_prompt_dedupe::claude_session_rotation_for_tmux(tmux_session_name)?;
    follow_cached_selector(shared, channel_id, &rotation).await?;
    Some(rotation.new_session_id)
}
