use super::*;

pub(super) async fn register_restored_core(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    owned_sessions: std::collections::HashMap<ChannelId, String>,
) {
    if !owned_sessions.is_empty() {
        let mut data = shared.core.lock().await;
        for (channel_id, channel_name) in &owned_sessions {
            let persisted_path = load_last_session_path(
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                channel_id.get(),
            );
            let persisted_session_id = load_restored_provider_session_id(
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                provider,
                channel_name,
            );
            let configured_path = crate::services::discord::settings::resolve_workspace(
                *channel_id,
                Some(channel_name.as_str()),
            );
            let tmux_name = provider.build_tmux_session_name(channel_name);
            let session_keys = crate::services::discord::adk_session::build_session_key_candidates(
                &shared.token_hash,
                provider,
                &tmux_name,
            );
            let db_cwd =
                load_restored_session_cwd(shared.pg_pool.as_ref(), &session_keys, channel_id.get());

            let session = data.sessions.entry(*channel_id).or_insert_with(|| {
                crate::services::discord::DiscordSession {
                    session_id: persisted_session_id.clone(),
                    memento_context_loaded:
                        crate::services::discord::session_runtime::restored_memento_context_loaded(
                            false,
                            None,
                            persisted_session_id.as_deref(),
                        ),
                    memento_reflected: false,
                    current_path: None,
                    history: Vec::new(),
                    pending_uploads: Vec::new(),
                    cleared: false,
                    channel_name: Some(channel_name.clone()),
                    category_name: None,
                    channel_id: Some(channel_id.get()),

                    last_active: tokio::time::Instant::now(),
                    worktree: None,

                    born_generation: crate::services::discord::runtime_store::process_generation(),
                }
            });

            if session.session_id.is_none() && persisted_session_id.is_some() {
                session.restore_provider_session(persisted_session_id.clone());
            }

            // Restore current_path: DB cwd (worktree-aware) > last_sessions (yaml, main workspace)
            if session.current_path.is_none() {
                // Prefer the channel's reusable worktree over its configured workspace;
                // a mismatch is only logged when that worktree is not reused.
                let reusable_worktree =
                    crate::services::discord::session_runtime::db_cwd_is_reusable_worktree(
                        configured_path.as_deref(),
                        db_cwd.as_deref(),
                    );
                if let (Some(configured), Some(restored)) =
                    (configured_path.as_ref(), db_cwd.as_ref())
                {
                    if configured != restored && !reusable_worktree {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⚠ Ignoring restored DB cwd for channel {}: {} (configured workspace: {})",
                            channel_id,
                            restored,
                            configured
                        );
                    }
                }
                let effective_path = crate::services::discord::select_restored_session_path(
                    configured_path,
                    db_cwd,
                    persisted_path,
                    reusable_worktree,
                );
                if let Some(path) = effective_path {
                    session.current_path = Some(path);
                }
            }
        }
    }
}
