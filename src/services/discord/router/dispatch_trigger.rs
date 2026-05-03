#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DispatchContextHints {
    pub(super) worktree_path: Option<String>,
    pub(super) stale_worktree_path: Option<String>,
    /// #762: when the dispatch context explicitly pins a `target_repo` (e.g. an
    /// external-repo review), propagate it so bootstrap fallbacks can resolve
    /// to the correct repo instead of the default AgentDesk workspace.
    pub(super) target_repo: Option<String>,
    pub(super) reset_provider_state: bool,
    pub(super) recreate_tmux: bool,
}

pub(super) fn parse_dispatch_context_hints(
    dispatch_context: Option<&str>,
    dispatch_type: Option<&str>,
) -> DispatchContextHints {
    let parsed =
        dispatch_context.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
    let requested_worktree_path = parsed
        .as_ref()
        .and_then(|v| v.get("worktree_path"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let target_repo = parsed
        .as_ref()
        .and_then(|v| v.get("target_repo"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from);
    let strategy =
        crate::dispatch::dispatch_session_strategy_from_context(parsed.as_ref(), dispatch_type);
    DispatchContextHints {
        worktree_path: requested_worktree_path
            .as_deref()
            .filter(|p| std::path::Path::new(p).exists())
            .map(str::to_string),
        stale_worktree_path: requested_worktree_path.filter(|p| !std::path::Path::new(p).exists()),
        target_repo,
        reset_provider_state: strategy.reset_provider_state,
        recreate_tmux: strategy.recreate_tmux,
    }
}

/// #762: Resolve a bootstrap fallback path for a dispatch without a usable
/// `worktree_path`. When the context pins an external `target_repo`, the
/// dispatch must land in that repo's configured directory rather than the
/// default AgentDesk workspace — otherwise external-repo reviews silently
/// review this repo's default HEAD.
///
/// Returns `None` when `target_repo` is unset or cannot be resolved; callers
/// fall back to `resolve_repo_dir()` / session CWD as before.
pub(super) fn resolve_dispatch_target_repo_dir(target_repo: Option<&str>) -> Option<String> {
    let target_repo = target_repo
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    match crate::services::platform::shell::resolve_repo_dir_for_target(Some(target_repo)) {
        Ok(Some(path)) => std::path::Path::new(&path).is_dir().then_some(path),
        Ok(None) => None,
        Err(err) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Dispatch target_repo '{}' could not be resolved: {}",
                target_repo,
                err
            );
            None
        }
    }
}

/// #762 (B): Decide whether a dispatch's `dispatch_effective_path` should
/// overwrite the active session's current_path.
///
/// Triggers when any of the following holds:
/// - The dispatch emitted a concrete `worktree_path` (classic #259 path —
///   review/rework sessions must execute inside the checked-out worktree).
/// - The dispatch pinned a `target_repo` whose resolved directory differs
///   from the session's current path. This covers reused threads where
///   `bootstrap_thread_session` returned early because the thread already
///   had a session: without this branch the session keeps its stale
///   `current_path` and an external-repo review quietly executes inside
///   the previous repo.
///
/// Returns `true` when the effective path should overwrite the session path.
pub(super) fn dispatch_session_path_should_update(
    has_dispatch: bool,
    dispatch_type: Option<&str>,
    has_worktree_path: bool,
    bootstrapped_fresh_thread_session: bool,
    current_path: &str,
    dispatch_effective_path: &str,
) -> bool {
    if !has_dispatch {
        return false;
    }
    if crate::dispatch::dispatch_type_requires_fresh_worktree(dispatch_type)
        && bootstrapped_fresh_thread_session
    {
        return false;
    }
    if has_worktree_path {
        return true;
    }
    dispatch_effective_path != current_path
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DispatchCwdPolicyDecision {
    pub(super) log_main_workspace_error: bool,
    pub(super) reject_for_missing_fresh_worktree: bool,
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn evaluate_dispatch_cwd_policy(
    dispatch_type: Option<&str>,
    current_path: &str,
    main_workspace: Option<&std::path::Path>,
    worktrees_root: Option<&std::path::Path>,
) -> DispatchCwdPolicyDecision {
    let requires_fresh_worktree =
        crate::dispatch::dispatch_type_requires_fresh_worktree(dispatch_type);
    let current_path = std::path::Path::new(current_path);
    let is_main_workspace = main_workspace.is_some_and(|workspace| current_path == workspace);
    let is_managed_worktree = worktrees_root.is_some_and(|root| current_path.starts_with(root));

    DispatchCwdPolicyDecision {
        log_main_workspace_error: requires_fresh_worktree && is_main_workspace,
        reject_for_missing_fresh_worktree: requires_fresh_worktree
            && worktrees_root.is_some()
            && !is_managed_worktree,
    }
}
