use super::*;
use crate::services::discord::session_runtime::reconstruct_managed_worktree_metadata;

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
#[derive(Debug, Clone)]
pub(super) struct ResolvedThreadRoleBinding {
    pub(super) role_binding: Option<settings::RoleBinding>,
    inherited_parent: Option<(ChannelId, Option<String>)>,
}
impl ResolvedThreadRoleBinding {
    pub(super) fn direct(role_binding: Option<settings::RoleBinding>) -> Self {
        Self {
            role_binding,
            inherited_parent: None,
        }
    }

    pub(super) fn memory_channel_id(&self, channel_id: ChannelId) -> ChannelId {
        self.inherited_parent
            .as_ref()
            .map(|(parent_id, _)| *parent_id)
            .unwrap_or(channel_id)
    }

    pub(super) fn memory_channel_name(&self, channel_name: Option<&str>) -> Option<String> {
        self.inherited_parent
            .as_ref()
            .and_then(|(_, parent_name)| parent_name.clone())
            .or_else(|| channel_name.map(String::from))
    }
}
fn inheritable_thread_parent(
    thread_parent: Option<&(ChannelId, Option<String>)>,
) -> Option<&(ChannelId, Option<String>)> {
    thread_parent.filter(|(parent_id, parent_name)| {
        super::super::super::role_map::thread_inheritance_enabled(
            *parent_id,
            parent_name.as_deref(),
        )
    })
}
pub(super) fn resolve_thread_role_binding(
    channel_id: ChannelId,
    channel_name: Option<&str>,
    thread_parent: Option<&(ChannelId, Option<String>)>,
) -> ResolvedThreadRoleBinding {
    let direct = settings::resolve_role_binding(channel_id, channel_name);
    if direct.is_some() {
        return ResolvedThreadRoleBinding::direct(direct);
    }
    let inherited =
        inheritable_thread_parent(thread_parent).and_then(|(parent_id, parent_name)| {
            settings::resolve_role_binding(*parent_id, parent_name.as_deref())
                .map(|binding| (binding, (*parent_id, parent_name.clone())))
        });
    ResolvedThreadRoleBinding {
        role_binding: inherited.as_ref().map(|(binding, _)| binding.clone()),
        inherited_parent: inherited.map(|(_, parent)| parent),
    }
}
pub(super) fn resolve_thread_workspace(
    channel_id: ChannelId,
    channel_name: Option<&str>,
    thread_parent: Option<&(ChannelId, Option<String>)>,
) -> Option<String> {
    settings::resolve_workspace(channel_id, channel_name).or_else(|| {
        inheritable_thread_parent(thread_parent).and_then(|(parent_id, parent_name)| {
            settings::resolve_workspace(*parent_id, parent_name.as_deref())
        })
    })
}
pub(super) fn metadata_delivery_bot(metadata: Option<&serde_json::Value>) -> Option<String> {
    metadata
        .and_then(|value| value.get("delivery_bot"))
        .and_then(|value| value.as_str())
        .and_then(normalize_delivery_bot_name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RuntimeKindEvidenceStrength {
    Weak,
    Moderate,
    Strong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ManagedRuntimeExpectation {
    pub(super) runtime_kind: RuntimeHandoffKind,
    evidence_strength: RuntimeKindEvidenceStrength,
}

#[cfg(unix)]
pub(super) fn prelaunch_runtime_kind_for_managed_session(
    provider: &ProviderKind,
    remote_profile_is_none: bool,
    has_tmux_session_name: bool,
    channel_id: Option<u64>,
) -> Option<ManagedRuntimeExpectation> {
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
    let hook_endpoint_present =
        crate::services::claude_tui::hook_server::current_hook_endpoint().is_some();
    let runtime_kind = if selection.driver
        == crate::services::provider_hosting::ProviderSessionDriver::TuiHosting
    {
        match provider {
            ProviderKind::Claude if hook_endpoint_present => RuntimeHandoffKind::ClaudeTui,
            ProviderKind::Codex => RuntimeHandoffKind::CodexTui,
            _ => RuntimeHandoffKind::LegacyTmuxWrapper,
        }
    } else {
        RuntimeHandoffKind::LegacyTmuxWrapper
    };
    let evidence_strength = if matches!(provider, ProviderKind::Claude)
        && selection.driver == crate::services::provider_hosting::ProviderSessionDriver::TuiHosting
        && !hook_endpoint_present
    {
        RuntimeKindEvidenceStrength::Weak
    } else {
        RuntimeKindEvidenceStrength::Strong
    };
    Some(ManagedRuntimeExpectation {
        runtime_kind,
        evidence_strength,
    })
}

#[cfg(not(unix))]
pub(super) fn prelaunch_runtime_kind_for_managed_session(
    _provider: &ProviderKind,
    _remote_profile_is_none: bool,
    _has_tmux_session_name: bool,
    _channel_id: Option<u64>,
) -> Option<ManagedRuntimeExpectation> {
    None
}

/// Seeds the durable inflight runtime fields before runtime handoff binds the
/// provider-owned transcript path.
fn prelaunch_inflight_runtime_seed_from_paths(
    tmux_name: &str,
    output_path: String,
    input_fifo_path: String,
    session_exists: bool,
    prelaunch_runtime_kind: Option<RuntimeHandoffKind>,
) -> (Option<String>, Option<String>, Option<String>, u64) {
    let is_claude_tui = prelaunch_runtime_kind == Some(RuntimeHandoffKind::ClaudeTui);
    let last_offset = (!is_claude_tui)
        .then(|| {
            std::fs::metadata(&output_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        })
        .unwrap_or(0);
    (
        Some(tmux_name.to_string()),
        (!is_claude_tui).then_some(output_path),
        Some(input_fifo_path),
        session_exists.then_some(last_offset).unwrap_or(0),
    )
}

pub(super) fn prelaunch_inflight_runtime_seed(
    provider: &ProviderKind,
    remote_profile_is_none: bool,
    tmux_session_name: Option<&str>,
    prelaunch_runtime_kind: Option<RuntimeHandoffKind>,
) -> (Option<String>, Option<String>, Option<String>, u64) {
    #[cfg(unix)]
    {
        if remote_profile_is_none
            && provider.uses_managed_tmux_backend()
            && claude::is_tmux_available()
            && let Some(tmux_name) = tmux_session_name
        {
            let (output_path, input_fifo_path) = tmux_runtime_paths(tmux_name);
            let session_exists =
                crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_name);
            return prelaunch_inflight_runtime_seed_from_paths(
                tmux_name,
                output_path,
                input_fifo_path,
                session_exists,
                prelaunch_runtime_kind,
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (
            provider,
            remote_profile_is_none,
            tmux_session_name,
            prelaunch_runtime_kind,
        );
    }
    (None, None, None, 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ObservedManagedRuntimeKind {
    pub(super) runtime_kind: RuntimeHandoffKind,
    evidence_strength: RuntimeKindEvidenceStrength,
}

#[cfg(unix)]
pub(super) fn observed_runtime_kind_for_managed_tmux(
    provider: &ProviderKind,
    tmux_session_name: &str,
) -> Option<ObservedManagedRuntimeKind> {
    if let Some(binding) =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
    {
        return Some(ObservedManagedRuntimeKind {
            runtime_kind: binding.runtime_kind,
            evidence_strength: RuntimeKindEvidenceStrength::Strong,
        });
    }
    if let Some(marker) =
        crate::services::tmux_common::resolve_tmux_runtime_kind_marker(tmux_session_name)
    {
        return Some(ObservedManagedRuntimeKind {
            runtime_kind: marker,
            evidence_strength: RuntimeKindEvidenceStrength::Strong,
        });
    }
    if crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "input").is_some()
    {
        return Some(ObservedManagedRuntimeKind {
            runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
            evidence_strength: RuntimeKindEvidenceStrength::Moderate,
        });
    }
    match provider {
        ProviderKind::Claude => Some(ObservedManagedRuntimeKind {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            evidence_strength: RuntimeKindEvidenceStrength::Weak,
        }),
        ProviderKind::Codex => Some(ObservedManagedRuntimeKind {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            evidence_strength: RuntimeKindEvidenceStrength::Weak,
        }),
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
    tracing::info!(
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RuntimeMismatchVerdict {
    Match,
    Recreate,
    Defer,
}

impl RuntimeMismatchVerdict {
    pub(super) fn should_defer(self) -> bool {
        self == Self::Defer
    }
}

pub(super) fn runtime_mismatch_verdict(
    expected: ManagedRuntimeExpectation,
    observed: ObservedManagedRuntimeKind,
    live_turn: bool,
) -> RuntimeMismatchVerdict {
    if expected.runtime_kind == observed.runtime_kind {
        RuntimeMismatchVerdict::Match
    } else if expected.evidence_strength != RuntimeKindEvidenceStrength::Strong
        || observed.evidence_strength != RuntimeKindEvidenceStrength::Strong
        || live_turn
    {
        RuntimeMismatchVerdict::Defer
    } else {
        RuntimeMismatchVerdict::Recreate
    }
}

#[cfg(unix)]
fn managed_runtime_transcript_state_using(
    provider: &ProviderKind,
    current_path: Option<&str>,
    session_id: Option<&str>,
    tmux_session_name: Option<&str>,
    observe_claude: fn(
        Option<&str>,
        Option<&str>,
        Option<&str>,
    ) -> crate::services::tui_turn_state::TuiTurnState,
    observe_codex: fn(
        Option<&str>,
        Option<&str>,
        Option<&str>,
    ) -> crate::services::tui_turn_state::TuiTurnState,
) -> crate::services::tui_turn_state::TuiTurnState {
    match provider {
        ProviderKind::Claude => observe_claude(current_path, session_id, tmux_session_name),
        ProviderKind::Codex => observe_codex(current_path, tmux_session_name, session_id),
        _ => crate::services::tui_turn_state::TuiTurnState::Unknown,
    }
}

#[cfg(unix)]
fn managed_runtime_transcript_state(
    provider: &ProviderKind,
    current_path: Option<&str>,
    session_id: Option<&str>,
    tmux_session_name: Option<&str>,
) -> crate::services::tui_turn_state::TuiTurnState {
    managed_runtime_transcript_state_using(
        provider,
        current_path,
        session_id,
        tmux_session_name,
        observe_claude_tui_transcript_state_for_session,
        observe_codex_tui_rollout_state_for_cwd,
    )
}

#[cfg(unix)]
#[path = "provider_isolation/runtime_mismatch.rs"]
mod runtime_mismatch;
#[cfg(unix)]
pub(super) use runtime_mismatch::reconcile_managed_tmux_runtime_kind_using_runtime;
#[cfg(unix)]
use runtime_mismatch::{
    RuntimeInflightEvidence, clear_runtime_mismatch_defer,
    reconcile_managed_tmux_runtime_kind_for_config,
};

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
    thread_parent: Option<&(ChannelId, Option<String>)>,
    metadata: Option<&serde_json::Value>,
) -> Option<String> {
    resolve_thread_workspace(channel_id, channel_name_hint, thread_parent).or_else(|| {
        thread_parent.is_none().then(|| {
            metadata_parent_channel_id(metadata)
                .and_then(|parent_id| settings::resolve_workspace(parent_id, None))
        })?
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

pub(super) fn select_final_path<'a>(
    dispatch: &'a str,
    workspace: Option<&'a str>,
    authoritative: bool,
) -> &'a str {
    workspace.filter(|_| !authoritative).unwrap_or(dispatch)
}

pub(super) async fn apply_final_thread_workspace(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    thread_parent: Option<&(ChannelId, Option<String>)>,
    selection: (&mut String, bool),
) -> bool {
    let (dispatch_path, authoritative) = selection;
    let data = shared.core.lock().await;
    let channel_name = data
        .sessions
        .get(&channel_id)
        .and_then(|session| session.channel_name.as_deref());
    let workspace = resolve_thread_workspace(channel_id, channel_name, thread_parent);
    let has_workspace = workspace.is_some();
    *dispatch_path =
        select_final_path(dispatch_path, workspace.as_deref(), authoritative).to_owned();
    has_workspace
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

fn reconstruct_unowned_managed_worktree(
    session: &mut DiscordSession,
    conflict: Option<&str>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    canonical: &str,
) -> bool {
    if conflict.is_some() {
        return false;
    }
    reconstruct_managed_worktree_metadata(session, provider, channel_id, canonical);
    session.worktree.is_some()
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

    let (already_isolated, reconstructed, session_channel_name, conflict) = {
        let mut data = shared.core.lock().await;
        let conflict = detect_worktree_conflict(&data.sessions, &canonical, channel_id);
        let already_isolated = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.worktree.as_ref())
            .is_some();
        let reconstructed = data.sessions.get_mut(&channel_id).is_some_and(|session| {
            reconstruct_unowned_managed_worktree(
                session,
                conflict.as_deref(),
                provider,
                channel_id,
                &canonical,
            )
        });
        let session_channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone());
        (
            already_isolated,
            reconstructed,
            session_channel_name,
            conflict,
        )
    };
    if already_isolated || (conflict.is_none() && reconstructed) {
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
        let _ = super::super::super::inflight::save_inflight_state_if_identity_unchanged(
            &inflight,
            "provider_worktree_isolation",
        );
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
    if let Some(key) = build_adk_session_key(shared, channel_id, provider, None).await {
        super::super::super::adk_session::clear_provider_session_id(&key, shared.api_port).await;
    }
    if let Some(stale_session_id) = outcome.stale_session_id.as_deref() {
        let _ = super::super::super::internal_api::clear_stale_session_id(stale_session_id).await;
    }
    *session_id = None;
    *memento_context_loaded = false;
    *session_strategy_reason = "provider_channel_worktree_isolated";
}
#[cfg(test)]
#[path = "provider_isolation/thread_role_inheritance_tests.rs"]
mod thread_role_inheritance_tests;
