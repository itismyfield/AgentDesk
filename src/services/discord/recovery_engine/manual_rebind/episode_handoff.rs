//! Exact-episode authority commit for automatic watcher reattach.

use super::*;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct EpisodeSideEffectOutcome {
    pub finish_mailbox_on_completion: bool,
    pub repaired_state: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn commit_episode_side_effects(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    discord_channel_id: ChannelId,
    recovered_state: &super::inflight::InflightTurnState,
    mut locked_episode: Option<super::inflight::LockedInflightEpisode>,
    existing_inflight_present: bool,
    existing_session_id: &Option<String>,
    channel_name: &Option<String>,
    session_id_for_state: &Option<String>,
    runtime_kind_for_state: Option<RuntimeHandoffKind>,
    output_path: &str,
    tmux_session_name: &str,
    initial_offset: u64,
) -> Result<
    (
        Option<super::inflight::LockedInflightEpisode>,
        EpisodeSideEffectOutcome,
    ),
    RebindError,
> {
    // A terminal commit is a lifecycle-authority transition, not a reattachable
    // episode. The reservation pin normally rejects a commit that wins before
    // adoption; keep this lock-held check as defense in depth so the guarded
    // path can never reach a helper that tries to clear (and recursively lock)
    // the same inflight sidecar.
    if locked_episode
        .as_ref()
        .is_some_and(|guard| guard.state().terminal_delivery_committed)
    {
        return Err(RebindError::InflightEpisodeChanged);
    }

    // Automatic adoption already wrote under, and retained, the canonical
    // inflight flock. Never synchronously wait for that flock while holding the
    // async core mutex. The returned guard remains live through watcher spawn.
    let mut core = shared.core.lock().await;
    let authoritative_owned = locked_episode.as_ref().map(|guard| guard.state().clone());
    let authoritative_state = authoritative_owned.as_ref().unwrap_or(recovered_state);

    if let Some(current_msg_id) = optional_message_id(authoritative_state.current_msg_id) {
        footer_view_reconciler::note_footer_suppressed_for_message_takeover(
            discord_channel_id,
            current_msg_id,
        );
    }

    let previous_session = core.sessions.get(&discord_channel_id).cloned();
    let session = core
        .sessions
        .entry(discord_channel_id)
        .or_insert_with(|| DiscordSession {
            session_id: locked_episode
                .as_ref()
                .and_then(|_| authoritative_state.session_id.clone())
                .or_else(|| existing_session_id.clone()),
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(channel_id),
            channel_name: authoritative_state
                .channel_name
                .clone()
                .or_else(|| channel_name.clone()),
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: super::runtime_store::process_generation(),
        });
    session.channel_id = Some(channel_id);
    session.last_active = tokio::time::Instant::now();
    if session.channel_name.is_none() {
        session.channel_name = authoritative_state
            .channel_name
            .clone()
            .or_else(|| channel_name.clone());
    }
    let authoritative_session_id = locked_episode
        .as_ref()
        .map(|_| authoritative_state.session_id.clone())
        .unwrap_or_else(|| session_id_for_state.clone());
    if authoritative_session_id.is_some() {
        session.session_id = authoritative_session_id.clone();
    }
    restore_recovered_session_worktree(session, authoritative_state);
    let session_binding_changed = previous_session.as_ref().is_none_or(|previous| {
        previous.session_id != session.session_id
            || previous.channel_id != session.channel_id
            || previous.channel_name != session.channel_name
            || previous.current_path != session.current_path
            || previous.worktree.as_ref().map(|worktree| {
                (
                    worktree.original_path.as_str(),
                    worktree.worktree_path.as_str(),
                    worktree.branch_name.as_str(),
                )
            }) != session.worktree.as_ref().map(|worktree| {
                (
                    worktree.original_path.as_str(),
                    worktree.worktree_path.as_str(),
                    worktree.branch_name.as_str(),
                )
            })
    });
    drop(core);

    #[cfg(test)]
    if locked_episode.is_some() {
        super::test_barriers::await_episode_authority_held_barrier().await;
    }

    let reregister_outcome = if existing_inflight_present {
        if locked_episode.is_some() {
            super::reregister_active_turn_from_inflight_under_episode_guard(
                shared,
                authoritative_state,
            )
            .await
        } else {
            let finish_mailbox_on_completion =
                reregister_active_turn_from_inflight(shared, authoritative_state).await;
            super::ActiveTurnReregisterOutcome {
                finish_mailbox_on_completion,
                mailbox_binding_changed: finish_mailbox_on_completion,
            }
        }
    } else {
        super::ActiveTurnReregisterOutcome::default()
    };
    let finish_mailbox_on_completion = reregister_outcome.finish_mailbox_on_completion;
    let mut readoption_marker_changed = false;

    if finish_mailbox_on_completion && let Some(guard) = locked_episode.as_mut() {
        readoption_marker_changed = !guard.state().readopted_from_inflight;
        let outcome = guard.mark_readopted_under_guard();
        if !matches!(outcome, super::inflight::GuardedSaveOutcome::Saved) {
            shared.evict_readopted_mailbox_owner(provider, channel_id);
            return Err(RebindError::Internal(format!(
                "persist exact-episode readoption marker for channel {channel_id}: {outcome:?}"
            )));
        }
    }

    let authoritative_runtime_kind = locked_episode
        .as_ref()
        .map(|_| authoritative_state.runtime_kind)
        .unwrap_or(runtime_kind_for_state);
    let authoritative_output_path = locked_episode
        .as_ref()
        .and_then(|_| authoritative_state.output_path.as_deref())
        .unwrap_or(output_path);
    let authoritative_tmux_session = locked_episode
        .as_ref()
        .and_then(|_| authoritative_state.tmux_session_name.as_deref())
        .unwrap_or(tmux_session_name);
    let runtime_binding_changed = if claude_tui_rebind_should_reregister_runtime_binding(
        authoritative_runtime_kind,
        authoritative_output_path,
    ) {
        let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: authoritative_output_path.to_string(),
            relay_output_path: None,
            input_fifo_path: authoritative_state.input_fifo_path.clone(),
            session_id: authoritative_session_id,
            last_offset: initial_offset.max(authoritative_state.last_offset),
            relay_last_offset: None,
        };
        let changed = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
            authoritative_tmux_session,
        )
        .as_ref()
            != Some(&binding)
            || crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(
                authoritative_tmux_session,
            ) != Some(channel_id);
        crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
            provider.as_str(),
            authoritative_tmux_session,
            channel_id,
            binding,
        );
        changed
    } else {
        false
    };

    Ok((
        locked_episode,
        EpisodeSideEffectOutcome {
            finish_mailbox_on_completion,
            repaired_state: session_binding_changed
                || reregister_outcome.mailbox_binding_changed
                || readoption_marker_changed
                || runtime_binding_changed,
        },
    ))
}
