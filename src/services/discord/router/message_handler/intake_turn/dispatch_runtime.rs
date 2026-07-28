use super::*;

pub(super) async fn prepare_post_redirect_dispatch_runtime(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    dispatch_id_for_thread: Option<&String>,
    dispatch_info_cached: Option<super::super::super::thread_binding::DispatchInfo>,
    dispatch_paths: (Option<&String>, Option<&String>),
    dispatch_runtime: (&mut Option<String>, &mut String),
) -> (
    Option<(ChannelId, Option<String>)>,
    bool,
    Option<super::super::super::thread_binding::DispatchInfo>,
    Option<String>,
) {
    let (dispatch_worktree_path, dispatch_target_repo_path) = dispatch_paths;
    let (dispatch_type_str, dispatch_effective_path) = dispatch_runtime;
    let final_thread_parent =
        crate::services::discord::resolve_thread_parent(http, channel_id).await;
    let mut authoritative = dispatch_worktree_path.is_some() || dispatch_target_repo_path.is_some();
    if dispatch_should_recover_session_worktree(
        dispatch_id_for_thread.is_some(),
        dispatch_type_str.as_deref(),
        dispatch_worktree_path.is_some(),
    ) {
        let session_worktree_path = {
            let data = shared.core.lock().await;
            data.sessions
                .get(&channel_id)
                .and_then(|session| session.worktree.as_ref())
                .map(|worktree| worktree.worktree_path.clone())
                .filter(|path| std::path::Path::new(path).is_dir())
        };
        if let Some(worktree_path) = session_worktree_path {
            authoritative = true;
            if *dispatch_effective_path != worktree_path {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🌿 Dispatch recovered thread worktree CWD: {} → {}",
                    dispatch_effective_path,
                    worktree_path
                );
                *dispatch_effective_path = worktree_path;
            }
        }
    }
    let active_dispatch_id_for_prompt =
        crate::services::discord::adk_session::lookup_pending_dispatch_for_thread(
            shared.api_port,
            channel_id.get(),
        )
        .await
        .or_else(|| dispatch_id_for_thread.cloned());
    let active_dispatch_info = match active_dispatch_id_for_prompt.as_deref() {
        Some(did) if dispatch_id_for_thread.map(String::as_str) == Some(did) => {
            dispatch_info_cached
        }
        Some(did) => super::super::super::lookup_dispatch_info(shared.api_port, did).await,
        None => None,
    };
    if let Some(active_dispatch_type) = active_dispatch_info
        .as_ref()
        .and_then(|info| info.dispatch_type.clone())
    {
        *dispatch_type_str = Some(active_dispatch_type);
    }
    (
        final_thread_parent,
        authoritative,
        active_dispatch_info,
        active_dispatch_id_for_prompt,
    )
}
