use super::*;

pub(super) fn metadata_parent_channel_id(
    metadata: Option<&serde_json::Value>,
) -> Option<serenity::ChannelId> {
    metadata
        .and_then(|value| value.get("parent_channel_id"))
        .and_then(|value| value.as_str())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|id| *id > 0)
        .map(serenity::ChannelId::new)
}

pub(super) fn metadata_delivery_bot(metadata: Option<&serde_json::Value>) -> Option<String> {
    metadata
        .and_then(|value| value.get("delivery_bot"))
        .and_then(|value| value.as_str())
        .and_then(normalize_delivery_bot_name)
}

#[cfg(unix)]
pub(super) fn prelaunch_runtime_kind_for_managed_session(
    provider: &ProviderKind,
    remote_profile_is_none: bool,
    has_tmux_session_name: bool,
    channel_id: Option<u64>,
) -> Option<RuntimeHandoffKind> {
    if !remote_profile_is_none
        || !has_tmux_session_name
        || !provider.uses_managed_tmux_backend()
        || !claude::is_tmux_available()
    {
        return None;
    }
    let selection =
        crate::services::provider_hosting::resolve_provider_session_selection_with_channel(
            provider, true, channel_id,
        );
    if selection.driver == crate::services::provider_hosting::ProviderSessionDriver::TuiHosting {
        return match provider {
            ProviderKind::Claude
                if crate::services::claude_tui::hook_server::current_hook_endpoint().is_some() =>
            {
                Some(RuntimeHandoffKind::ClaudeTui)
            }
            ProviderKind::Codex => Some(RuntimeHandoffKind::CodexTui),
            _ => Some(RuntimeHandoffKind::LegacyTmuxWrapper),
        };
    }
    Some(RuntimeHandoffKind::LegacyTmuxWrapper)
}

#[cfg(not(unix))]
pub(super) fn prelaunch_runtime_kind_for_managed_session(
    _provider: &ProviderKind,
    _remote_profile_is_none: bool,
    _has_tmux_session_name: bool,
    _channel_id: Option<u64>,
) -> Option<RuntimeHandoffKind> {
    None
}

#[cfg(unix)]
pub(super) fn observed_runtime_kind_for_managed_tmux(
    provider: &ProviderKind,
    tmux_session_name: &str,
) -> Option<RuntimeHandoffKind> {
    if let Some(binding) =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
    {
        return Some(binding.runtime_kind);
    }
    if let Some(marker) =
        crate::services::tmux_common::resolve_tmux_runtime_kind_marker(tmux_session_name)
    {
        return Some(marker);
    }
    if crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "input").is_some()
    {
        return Some(RuntimeHandoffKind::LegacyTmuxWrapper);
    }
    match provider {
        ProviderKind::Claude => Some(RuntimeHandoffKind::ClaudeTui),
        ProviderKind::Codex => Some(RuntimeHandoffKind::CodexTui),
        _ => None,
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LiveTuiProviderSessionRecovery {
    session_id: String,
    output_path: String,
}

#[cfg(unix)]
pub(super) fn live_tui_provider_session_recovery(
    provider: &ProviderKind,
    tmux_session_name: Option<&str>,
) -> Option<LiveTuiProviderSessionRecovery> {
    if !matches!(provider, ProviderKind::Claude) {
        return None;
    }
    let tmux_session_name = tmux_session_name
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    if !crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name) {
        return None;
    }
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)?;
    if binding.runtime_kind != RuntimeHandoffKind::ClaudeTui {
        return None;
    }
    let session_id = binding
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    if !std::path::Path::new(&binding.output_path).exists() {
        return None;
    }
    Some(LiveTuiProviderSessionRecovery {
        session_id: session_id.to_string(),
        output_path: binding.output_path,
    })
}

#[cfg(unix)]
pub(super) async fn restore_live_tui_provider_session_from_binding(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    tmux_session_name: Option<&str>,
    adk_session_key: Option<&str>,
) -> Option<(String, bool)> {
    let recovery = live_tui_provider_session_recovery(provider, tmux_session_name)?;
    let memento_context_loaded = {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.restore_provider_session(Some(recovery.session_id.clone()));
            session.memento_context_loaded
        } else {
            false
        }
    };
    if let Some(session_key) = adk_session_key {
        super::super::super::adk_session::save_provider_session_id(
            session_key,
            &recovery.session_id,
            Some(&recovery.session_id),
            provider,
            channel_id,
            shared.api_port,
        )
        .await;
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ↻ Recovered provider session_id from live TUI runtime binding for channel {}: tmux={} transcript={}",
        channel_id.get(),
        tmux_session_name.unwrap_or("(none)"),
        recovery.output_path
    );
    Some((recovery.session_id, memento_context_loaded))
}

#[cfg(not(unix))]
pub(super) async fn restore_live_tui_provider_session_from_binding(
    _shared: &Arc<SharedData>,
    _channel_id: serenity::ChannelId,
    _provider: &ProviderKind,
    _tmux_session_name: Option<&str>,
    _adk_session_key: Option<&str>,
) -> Option<(String, bool)> {
    None
}

#[cfg(unix)]
pub(super) fn reconcile_managed_tmux_runtime_kind_for_config(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: Option<&str>,
    expected_runtime_kind: Option<RuntimeHandoffKind>,
) {
    let (Some(tmux_session_name), Some(expected_runtime_kind)) =
        (tmux_session_name, expected_runtime_kind)
    else {
        return;
    };
    if !provider.uses_managed_tmux_backend()
        || !crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name)
    {
        return;
    }
    let Some(observed_runtime_kind) =
        observed_runtime_kind_for_managed_tmux(provider, tmux_session_name)
    else {
        return;
    };
    if observed_runtime_kind == expected_runtime_kind {
        return;
    }

    let reason = format!(
        "tui_hosting config changed: expected {}, found {}; recreating tmux session",
        expected_runtime_kind.as_str(),
        observed_runtime_kind.as_str()
    );
    tracing::warn!(
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name,
        expected_runtime_kind = expected_runtime_kind.as_str(),
        observed_runtime_kind = observed_runtime_kind.as_str(),
        "managed tmux runtime kind mismatch detected; killing stale session before dispatch"
    );
    crate::services::termination_audit::record_termination_for_tmux(
        tmux_session_name,
        None,
        "discord_dispatch",
        "runtime_kind_mismatch_recreate",
        Some(&reason),
        None,
    );
    crate::services::tmux_diagnostics::record_tmux_exit_reason(tmux_session_name, &reason);
    crate::services::platform::tmux::kill_session(tmux_session_name, &reason);
    crate::services::tmux_common::cleanup_session_temp_files(tmux_session_name);
    let cleared_runtime_binding =
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(tmux_session_name);
    tracing::debug!(
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name,
        cleared_runtime_binding,
        "cleared stale tmux runtime binding after runtime kind mismatch"
    );
}

#[cfg(test)]
#[allow(dead_code)] // #3034: test-only runtime-kind mismatch classifier (no live caller).
pub(super) fn runtime_kind_mismatch_requires_recreate(
    observed_runtime_kind: Option<RuntimeHandoffKind>,
    expected_runtime_kind: Option<RuntimeHandoffKind>,
) -> bool {
    matches!(
        (observed_runtime_kind, expected_runtime_kind),
        (Some(observed), Some(expected)) if observed != expected
    )
}

pub(super) fn apply_prelaunch_runtime_kind(
    state: &mut InflightTurnState,
    runtime_kind: Option<RuntimeHandoffKind>,
) {
    if let Some(kind) = runtime_kind {
        state.runtime_kind = Some(kind);
        // #2235 compat window (one release): keep the synthesized
        // `input_fifo_path` populated when stamping ClaudeTui so that an old
        // (pre-#2213) binary rolling back over inflight rows written by this
        // binary can still satisfy its FIFO-required recovery branch. The new
        // recovery path treats the FIFO as optional for ClaudeTui, so leaving
        // it set has no behavioural cost on the new code. For CodexTui and
        // ProcessBackend we still clear, since neither legacy nor current
        // recovery uses a FIFO for those backends.
        match kind {
            RuntimeHandoffKind::ClaudeTui | RuntimeHandoffKind::LegacyTmuxWrapper => {}
            RuntimeHandoffKind::CodexTui
            | RuntimeHandoffKind::ProcessBackend
            | RuntimeHandoffKind::ClaudeEAdapter => {
                state.input_fifo_path = None;
            }
        }
    }
}

pub(super) fn metadata_silent_flag(metadata: Option<&serde_json::Value>) -> bool {
    metadata
        .and_then(|value| value.get("silent"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

pub(super) fn metadata_turn_source(
    source: Option<&str>,
    metadata: Option<&serde_json::Value>,
) -> crate::dispatch::Source {
    source
        .and_then(crate::dispatch::Source::from_label)
        .or_else(|| {
            metadata
                .and_then(|value| value.get("source").or_else(|| value.get("turn_source")))
                .and_then(serde_json::Value::as_str)
                .and_then(crate::dispatch::Source::from_label)
        })
        .unwrap_or_default()
}

pub(super) fn normalize_delivery_bot_name(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 64
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return None;
    }
    Some(value.to_string())
}

pub(super) fn resolve_headless_workspace(
    channel_id: serenity::ChannelId,
    channel_name_hint: Option<&str>,
    metadata: Option<&serde_json::Value>,
) -> Option<String> {
    settings::resolve_workspace(channel_id, channel_name_hint).or_else(|| {
        metadata_parent_channel_id(metadata)
            .and_then(|parent_channel_id| settings::resolve_workspace(parent_channel_id, None))
    })
}
pub(super) fn native_fast_mode_override_for_turn(
    provider: &ProviderKind,
    channel_fast_mode_setting: Option<bool>,
) -> Option<bool> {
    if matches!(provider, ProviderKind::Claude | ProviderKind::Codex) {
        channel_fast_mode_setting
    } else {
        None
    }
}

pub(super) fn codex_goals_override_for_turn(
    provider: &ProviderKind,
    channel_codex_goals_setting: Option<bool>,
) -> Option<bool> {
    if matches!(provider, ProviderKind::Codex) {
        channel_codex_goals_setting
    } else {
        None
    }
}
pub(super) fn effective_fast_mode_channel_id(
    channel_id: ChannelId,
    thread_parent: Option<(ChannelId, Option<String>)>,
) -> ChannelId {
    thread_parent
        .map(|(parent_channel_id, _)| parent_channel_id)
        .unwrap_or(channel_id)
}

pub(super) fn dispatch_type_bypasses_provider_worktree_isolation(
    dispatch_type: Option<&str>,
) -> bool {
    dispatch_type
        .map(str::trim)
        .map(|value| value.to_ascii_lowercase())
        .is_some_and(|value| matches!(value.as_str(), "review" | "e2e-test" | "consultation"))
}

pub(super) fn should_force_provider_worktree_isolation(
    non_main_provider_channel: bool,
    isolate_override: Option<bool>,
    dispatch_type: Option<&str>,
) -> bool {
    if dispatch_type_bypasses_provider_worktree_isolation(dispatch_type) {
        return false;
    }
    isolate_override.unwrap_or(non_main_provider_channel)
}

#[derive(Debug, Default)]
pub(super) struct ProviderWorktreeIsolationOutcome {
    applied: bool,
    stale_session_id: Option<String>,
}

pub(super) async fn ensure_provider_worktree_isolation(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    current_path: &mut String,
    provider: &ProviderKind,
    channel_name: Option<&str>,
    dispatch_type: Option<&str>,
) -> ProviderWorktreeIsolationOutcome {
    let Some(policy) = super::super::super::agentdesk_config::resolve_worktree_isolation_policy(
        channel_id,
        channel_name,
    ) else {
        return ProviderWorktreeIsolationOutcome::default();
    };
    if !should_force_provider_worktree_isolation(
        policy.non_main_provider_channel,
        policy.isolate_override,
        dispatch_type,
    ) {
        return ProviderWorktreeIsolationOutcome::default();
    }

    let path = std::path::Path::new(current_path);
    if !path.is_dir() {
        return ProviderWorktreeIsolationOutcome::default();
    }
    let canonical = path
        .canonicalize()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| current_path.clone());

    let (already_isolated, session_channel_name, conflict) = {
        let data = shared.core.lock().await;
        let already_isolated = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.worktree.as_ref())
            .is_some();
        let session_channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone());
        let conflict = detect_worktree_conflict(&data.sessions, &canonical, channel_id);
        (already_isolated, session_channel_name, conflict)
    };
    if already_isolated {
        return ProviderWorktreeIsolationOutcome::default();
    }

    let worktree_channel_name = session_channel_name
        .as_deref()
        .or(channel_name)
        .unwrap_or("unknown");
    let Ok((worktree_path, branch_name)) =
        create_git_worktree(&canonical, worktree_channel_name, provider.as_str())
    else {
        return ProviderWorktreeIsolationOutcome::default();
    };

    let base_commit = crate::services::platform::git_head_commit(&canonical);
    let mut stale_session_id = None;
    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            stale_session_id = session.session_id.clone();
            session.clear_provider_session();
            session.current_path = Some(worktree_path.clone());
            session.worktree = Some(WorktreeInfo {
                original_path: canonical.clone(),
                worktree_path: worktree_path.clone(),
                branch_name: branch_name.clone(),
            });
        }
    }
    if let Some(mut inflight) =
        super::super::super::inflight::load_inflight_state(provider, channel_id.get())
    {
        inflight.set_worktree_context(
            Some(worktree_path.clone()),
            Some(branch_name.clone()),
            base_commit,
        );
        let _ = super::super::super::inflight::save_inflight_state(&inflight);
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    if let Some(conflict) = conflict {
        tracing::info!(
            "  [{ts}] 🌿 Provider-channel worktree isolation (also conflicted with {conflict}): {} → {}",
            canonical,
            worktree_path
        );
    } else {
        tracing::info!(
            "  [{ts}] 🌿 Provider-channel worktree isolation: {} → {}",
            canonical,
            worktree_path
        );
    }
    *current_path = worktree_path;
    ProviderWorktreeIsolationOutcome {
        applied: true,
        stale_session_id,
    }
}

pub(super) async fn reset_provider_session_after_worktree_isolation(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    outcome: ProviderWorktreeIsolationOutcome,
    session_id: &mut Option<String>,
    memento_context_loaded: &mut bool,
    session_strategy_reason: &mut &'static str,
) {
    if !outcome.applied {
        return;
    }
    if let Some(key) = build_adk_session_key(shared, channel_id, provider).await {
        super::super::super::adk_session::clear_provider_session_id(&key, shared.api_port).await;
    }
    if let Some(stale_session_id) = outcome.stale_session_id.as_deref() {
        let _ = super::super::super::internal_api::clear_stale_session_id(stale_session_id).await;
    }
    *session_id = None;
    *memento_context_loaded = false;
    *session_strategy_reason = "provider_channel_worktree_isolated";
}
