use super::*;

#[derive(Clone, Copy, Debug)]
pub(in crate::services::discord) enum ThreadBootstrapPathSource<'a> {
    ParentDerived(ChannelId, Option<&'a str>),
    ExplicitDispatch,
}

#[derive(Debug, PartialEq, Eq)]
enum ThreadBootstrapPlan<'a> {
    PreserveExisting,
    SkipInherited,
    Bootstrap(&'a str),
}

fn plan_thread_bootstrap<'a>(
    child_session_exists: bool,
    source: ThreadBootstrapPathSource<'_>,
    path: &'a str,
) -> ThreadBootstrapPlan<'a> {
    if child_session_exists {
        return ThreadBootstrapPlan::PreserveExisting;
    }
    let allowed = match source {
        ThreadBootstrapPathSource::ParentDerived(parent_id, parent_name) => {
            settings::thread_inheritance_enabled(parent_id, parent_name)
        }
        ThreadBootstrapPathSource::ExplicitDispatch => true,
    };
    if allowed {
        ThreadBootstrapPlan::Bootstrap(path)
    } else {
        ThreadBootstrapPlan::SkipInherited
    }
}

/// Create a lightweight thread session from an allowed parent or dispatch path.
pub(in crate::services::discord) async fn bootstrap_thread_session(
    shared: &Arc<SharedData>,
    thread_channel_id: ChannelId,
    parent_path: &str,
    path_source: ThreadBootstrapPathSource<'_>,
    http: &Arc<serenity::http::Http>,
    cache: Option<&Arc<serenity::cache::Cache>>,
) -> bool {
    let (thread_title, cat_name) = resolve_channel_category(http, cache, thread_channel_id).await;
    let provider_kind = shared.settings.read().await.provider.clone();
    let parent_info = resolve_thread_parent(http, thread_channel_id).await;
    let ch_name = if let Some((parent_id, parent_name)) = parent_info {
        let parent = parent_name.unwrap_or_else(|| format!("{parent_id}"));
        Some(synthetic_thread_channel_name(&parent, thread_channel_id))
    } else {
        thread_title
    };
    let mut data = shared.core.lock().await;
    let parent_path = match plan_thread_bootstrap(
        data.sessions.contains_key(&thread_channel_id),
        path_source,
        parent_path,
    ) {
        ThreadBootstrapPlan::Bootstrap(path) => path,
        ThreadBootstrapPlan::PreserveExisting | ThreadBootstrapPlan::SkipInherited => return false,
    };

    let session = data
        .sessions
        .entry(thread_channel_id)
        .or_insert_with(|| DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            channel_id: Some(thread_channel_id.get()),
            channel_name: ch_name,
            category_name: cat_name,
            remote_profile_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: runtime_store::load_generation(),
        });
    let ch = session
        .channel_name
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let restored_worktree = resolve_reusable_worktree(
        shared.pg_pool.as_ref(),
        &shared.token_hash,
        &provider_kind,
        &ch,
        thread_channel_id.get(),
        parent_path,
    );
    if let Some(wt_info) = restored_worktree {
        let base_commit = crate::services::platform::git_head_commit(&wt_info.original_path);
        let restored_path = wt_info.worktree_path.clone();
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ Restored thread worktree: {} (branch: {})",
            wt_info.worktree_path,
            wt_info.branch_name
        );
        sync_inflight_worktree_context(
            &provider_kind,
            thread_channel_id.get(),
            Some(wt_info.worktree_path.clone()),
            Some(wt_info.branch_name.clone()),
            base_commit,
        );
        session.worktree = Some(wt_info);
        session.current_path = Some(restored_path.clone());
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] ↻ Bootstrapped thread session: {restored_path}");
        return true;
    }

    let effective_path = {
        let provider_str = shared.settings.read().await.provider.as_str().to_string();
        match create_git_worktree(parent_path, &ch, &provider_str) {
            Ok((wt_path, branch)) => {
                let base_commit = crate::services::platform::git_head_commit(parent_path);
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🌿 Thread worktree created: {} (branch: {})",
                    wt_path,
                    branch
                );
                session.worktree = Some(WorktreeInfo {
                    original_path: parent_path.to_string(),
                    worktree_path: wt_path.clone(),
                    branch_name: branch.clone(),
                });
                sync_inflight_worktree_context(
                    &provider_kind,
                    thread_channel_id.get(),
                    Some(wt_path.clone()),
                    Some(branch),
                    base_commit,
                );
                wt_path
            }
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ Thread worktree creation failed: {e}, falling back to parent path"
                );
                parent_path.to_string()
            }
        }
    };
    session.current_path = Some(effective_path.clone());
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ↻ Bootstrapped thread session: {effective_path}");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT_ID: u64 = 1479671301387059200;

    fn with_thread_inherit_disabled(test: impl FnOnce()) {
        let root = tempfile::tempdir().expect("temp AgentDesk root");
        let config_dir = root.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::write(
            config_dir.join("agentdesk.yaml"),
            format!(
                r#"server:
  port: 8791
agents:
  - id: project-agentdesk
    name: AgentDesk
    provider: codex
    channels:
      codex:
        id: "{PARENT_ID}"
        name: adk-cdx
        workspace: /tmp/parent-workspace
        threadInherit: false
"#
            ),
        )
        .expect("write AgentDesk config");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        test();
    }

    #[test]
    fn fresh_child_respects_parent_opt_out_but_explicit_dispatch_path_is_authoritative() {
        with_thread_inherit_disabled(|| {
            let parent = ChannelId::new(PARENT_ID);
            assert_eq!(
                plan_thread_bootstrap(
                    false,
                    ThreadBootstrapPathSource::ParentDerived(parent, Some("adk-cdx")),
                    "/tmp/parent-workspace",
                ),
                ThreadBootstrapPlan::SkipInherited,
                "fresh unbound child must not receive the opted-out parent path"
            );
            assert_eq!(
                plan_thread_bootstrap(
                    false,
                    ThreadBootstrapPathSource::ExplicitDispatch,
                    "/tmp/dispatch-worktree",
                ),
                ThreadBootstrapPlan::Bootstrap("/tmp/dispatch-worktree"),
                "explicit dispatch worktree remains authoritative"
            );
            assert_eq!(
                plan_thread_bootstrap(
                    true,
                    ThreadBootstrapPathSource::ParentDerived(parent, Some("adk-cdx")),
                    "/tmp/parent-workspace",
                ),
                ThreadBootstrapPlan::PreserveExisting,
                "an existing direct child session must never be replaced"
            );
        });
    }

    #[test]
    fn all_live_parent_bootstrap_callsites_supply_authority_source() {
        let gate = include_str!("../router/intake_gate.rs");
        let turn = include_str!("../router/message_handler/intake_turn.rs");
        assert_eq!(gate.matches("bootstrap_thread_session(").count(), 2);
        assert_eq!(turn.matches("bootstrap_thread_session(").count(), 2);
        assert_eq!(
            gate.matches("thread_bootstrap_path_source,").count(),
            2,
            "attachment and normal intake bootstraps must share the parent-derived source"
        );
        assert_eq!(
            turn.matches("dispatch_bootstrap_path_source,").count(),
            2,
            "dispatch reuse and create bootstraps must share dispatch path authority"
        );
    }
}
