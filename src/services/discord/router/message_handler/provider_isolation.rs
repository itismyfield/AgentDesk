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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RuntimeMismatchTarget {
    provider: String,
    channel_id: u64,
    tmux_session_name: String,
    expected_kind: RuntimeHandoffKind,
    observed_kind: RuntimeHandoffKind,
}

#[cfg(unix)]
impl RuntimeMismatchTarget {
    fn new(
        provider: &ProviderKind,
        channel_id: serenity::ChannelId,
        tmux_session_name: &str,
        expected: ManagedRuntimeExpectation,
        observed: ObservedManagedRuntimeKind,
    ) -> Self {
        Self {
            provider: provider.as_str().to_string(),
            channel_id: channel_id.get(),
            tmux_session_name: tmux_session_name.to_string(),
            expected_kind: expected.runtime_kind,
            observed_kind: observed.runtime_kind,
        }
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct RuntimeMismatchDeferState {
    count: u32,
    first_seen: std::time::Instant,
    escalated: bool,
}

#[cfg(unix)]
static RUNTIME_MISMATCH_DEFERS: std::sync::LazyLock<
    dashmap::DashMap<RuntimeMismatchTarget, RuntimeMismatchDeferState>,
> = std::sync::LazyLock::new(dashmap::DashMap::new);
#[cfg(unix)]
const RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT: u32 = 3;

fn clear_runtime_mismatch_defer(provider: &ProviderKind, channel_id: serenity::ChannelId) {
    #[cfg(unix)]
    RUNTIME_MISMATCH_DEFERS.retain(|target, _| {
        target.provider != provider.as_str() || target.channel_id != channel_id.get()
    });
    #[cfg(not(unix))]
    let _ = (provider, channel_id);
}

#[cfg(unix)]
fn record_runtime_mismatch_defer(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: &str,
    expected: ManagedRuntimeExpectation,
    observed: ObservedManagedRuntimeKind,
    open_inflight: bool,
    transcript_state: Option<crate::services::tui_turn_state::TuiTurnState>,
    defer_reason: &'static str,
) -> u32 {
    let key =
        RuntimeMismatchTarget::new(provider, channel_id, tmux_session_name, expected, observed);
    RUNTIME_MISMATCH_DEFERS.retain(|target, _| {
        target.provider != key.provider || target.channel_id != key.channel_id || target == &key
    });
    let mut state =
        RUNTIME_MISMATCH_DEFERS
            .entry(key)
            .or_insert_with(|| RuntimeMismatchDeferState {
                count: 0,
                first_seen: std::time::Instant::now(),
                escalated: false,
            });
    state.count = state.count.saturating_add(1);
    let elapsed_secs = state.first_seen.elapsed().as_secs();
    let escalating = !state.escalated && state.count >= RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT;
    if escalating {
        state.escalated = true;
    }
    let count = state.count;
    drop(state);

    crate::services::observability::emit_inflight_lifecycle_event(
        provider.as_str(),
        channel_id.get(),
        None,
        None,
        None,
        if escalating {
            "runtime_kind_mismatch_defer_escalated"
        } else {
            "runtime_kind_mismatch_deferred"
        },
        serde_json::json!({
            "consecutive_defer_count": count,
            "elapsed_secs": elapsed_secs,
            "escalation_threshold": RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT,
            "action": "continue_defer_without_kill",
            "defer_reason": defer_reason,
            "expected_runtime_kind": expected.runtime_kind.as_str(),
            "expected_evidence_strength": format!("{:?}", expected.evidence_strength),
            "observed_runtime_kind": observed.runtime_kind.as_str(),
            "observed_evidence_strength": format!("{:?}", observed.evidence_strength),
            "open_inflight": open_inflight,
            "transcript_state": transcript_state.map(|state| state.as_str()),
        }),
    );
    if count == 1 || escalating {
        tracing::warn!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            consecutive_defer_count = count,
            elapsed_secs,
            escalating,
            "managed tmux runtime mismatch deferred without killing the session"
        );
    }
    count
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct RuntimeInflightEvidence {
    open: bool,
    stale: bool,
}

#[cfg(unix)]
fn reconcile_managed_tmux_runtime_kind_for_config(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: Option<&str>,
    expected: Option<ManagedRuntimeExpectation>,
    live_pane: impl Fn(&str) -> bool,
    observe: impl Fn(&ProviderKind, &str) -> Option<ObservedManagedRuntimeKind>,
    inflight_evidence: impl Fn() -> RuntimeInflightEvidence,
    transcript_state: impl Fn() -> crate::services::tui_turn_state::TuiTurnState,
    target_still_owned: impl Fn(&str) -> bool,
    recreate: impl FnOnce(&str, ManagedRuntimeExpectation, ObservedManagedRuntimeKind),
) -> RuntimeMismatchVerdict {
    let (Some(tmux_session_name), Some(expected)) = (tmux_session_name, expected) else {
        clear_runtime_mismatch_defer(provider, channel_id);
        return RuntimeMismatchVerdict::Match;
    };
    if !provider.uses_managed_tmux_backend() || !live_pane(tmux_session_name) {
        clear_runtime_mismatch_defer(provider, channel_id);
        return RuntimeMismatchVerdict::Match;
    }
    let Some(observed) = observe(provider, tmux_session_name) else {
        clear_runtime_mismatch_defer(provider, channel_id);
        return RuntimeMismatchVerdict::Match;
    };
    if observed.runtime_kind == expected.runtime_kind {
        clear_runtime_mismatch_defer(provider, channel_id);
        return RuntimeMismatchVerdict::Match;
    }

    let inflight = inflight_evidence();
    let transcript_state = (!inflight.open || inflight.stale).then(transcript_state);
    let live_turn =
        (inflight.open && !inflight.stale) || transcript_state.is_some_and(|state| state.is_busy());
    let destructive_evidence_unknown = transcript_state
        .is_none_or(|state| state == crate::services::tui_turn_state::TuiTurnState::Unknown);
    let verdict = runtime_mismatch_verdict(
        expected,
        observed,
        live_turn || destructive_evidence_unknown,
    );
    if verdict.should_defer() {
        let defer_reason = if live_turn {
            "live_turn"
        } else if destructive_evidence_unknown {
            "transcript_state_unknown"
        } else if expected.evidence_strength != RuntimeKindEvidenceStrength::Strong {
            "weak_expected_evidence"
        } else if observed.evidence_strength != RuntimeKindEvidenceStrength::Strong {
            "weak_observed_evidence"
        } else if inflight.open {
            "stale_inflight"
        } else {
            "runtime_kind_mismatch"
        };
        record_runtime_mismatch_defer(
            provider,
            channel_id,
            tmux_session_name,
            expected,
            observed,
            inflight.open,
            transcript_state,
            defer_reason,
        );
        return verdict;
    }
    let revalidated_observed = observe(provider, tmux_session_name);
    let revalidated_inflight = inflight_evidence();
    let pane_live_at_commit = live_pane(tmux_session_name);
    let owner_matches_at_commit = target_still_owned(tmux_session_name);
    let target_unchanged = pane_live_at_commit
        && owner_matches_at_commit
        && revalidated_observed == Some(observed)
        && !revalidated_inflight.open;
    if !target_unchanged {
        let defer_reason = "destructive_target_revalidation_failed";
        record_runtime_mismatch_defer(
            provider,
            channel_id,
            tmux_session_name,
            expected,
            revalidated_observed.unwrap_or(observed),
            revalidated_inflight.open,
            None,
            defer_reason,
        );
        tracing::warn!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name,
            owner_matches = owner_matches_at_commit,
            pane_live = pane_live_at_commit,
            "managed tmux mismatch cleanup skipped because target identity changed before kill"
        );
        return RuntimeMismatchVerdict::Defer;
    }
    clear_runtime_mismatch_defer(provider, channel_id);
    recreate(tmux_session_name, expected, observed);
    RuntimeMismatchVerdict::Recreate
}

#[cfg(unix)]
pub(super) fn reconcile_managed_tmux_runtime_kind_using_runtime(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: Option<&str>,
    expected: Option<ManagedRuntimeExpectation>,
    current_path: Option<&str>,
    session_id: Option<&str>,
) -> RuntimeMismatchVerdict {
    reconcile_managed_tmux_runtime_kind_for_config(
        provider,
        channel_id,
        tmux_session_name,
        expected,
        crate::services::tmux_diagnostics::tmux_session_has_live_pane,
        observed_runtime_kind_for_managed_tmux,
        || {
            let state =
                super::super::super::inflight::load_inflight_state(provider, channel_id.get());
            RuntimeInflightEvidence {
                open: state
                    .as_ref()
                    .is_some_and(|state| !state.terminal_delivery_completed()),
                stale: state.as_ref().is_some_and(|state| {
                    super::super::super::inflight::inflight_state_is_stale(
                        state,
                        chrono::Utc::now().timestamp(),
                        super::super::super::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS,
                    )
                }),
            }
        },
        || managed_runtime_transcript_state(provider, current_path, session_id, tmux_session_name),
        |tmux_session_name| {
            shared
                .tmux_watchers
                .owner_channel_for_tmux_session(tmux_session_name)
                == Some(channel_id)
        },
        |tmux_session_name, expected, observed| {
            let reason = format!(
                "tui_hosting config changed: expected {}, found {}; recreating tmux session",
                expected.runtime_kind.as_str(),
                observed.runtime_kind.as_str()
            );
            tracing::warn!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session_name,
                expected_runtime_kind = expected.runtime_kind.as_str(),
                observed_runtime_kind = observed.runtime_kind.as_str(),
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
        },
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
mod thread_role_inheritance_tests {
    use super::*;

    fn clear_test_defer(channel_id: ChannelId) {
        clear_runtime_mismatch_defer(&ProviderKind::Claude, channel_id);
    }

    #[test]
    fn matrix_preserves_live_turns_and_only_recreates_strong_idle_mismatches() {
        let cases = [
            (
                RuntimeKindEvidenceStrength::Weak,
                RuntimeKindEvidenceStrength::Strong,
                true,
                RuntimeMismatchVerdict::Defer,
                "weak expected plus strong observed live turn",
            ),
            (
                RuntimeKindEvidenceStrength::Weak,
                RuntimeKindEvidenceStrength::Weak,
                false,
                RuntimeMismatchVerdict::Defer,
                "weak expected plus weak observed evidence",
            ),
            (
                RuntimeKindEvidenceStrength::Strong,
                RuntimeKindEvidenceStrength::Strong,
                false,
                RuntimeMismatchVerdict::Recreate,
                "strong config change with no live turn",
            ),
            (
                RuntimeKindEvidenceStrength::Strong,
                RuntimeKindEvidenceStrength::Strong,
                true,
                RuntimeMismatchVerdict::Defer,
                "strong config change with live turn",
            ),
            (
                RuntimeKindEvidenceStrength::Weak,
                RuntimeKindEvidenceStrength::Strong,
                false,
                RuntimeMismatchVerdict::Defer,
                "strong observed evidence cannot authorize dispatch under a weak expectation",
            ),
            (
                RuntimeKindEvidenceStrength::Strong,
                RuntimeKindEvidenceStrength::Moderate,
                false,
                RuntimeMismatchVerdict::Defer,
                "moderate observed evidence cannot authorize destructive cleanup",
            ),
            (
                RuntimeKindEvidenceStrength::Strong,
                RuntimeKindEvidenceStrength::Weak,
                false,
                RuntimeMismatchVerdict::Defer,
                "provider-only observed fallback cannot authorize destructive cleanup",
            ),
        ];
        for (expected_strength, observed_strength, live_turn, want, label) in cases {
            let expected = ManagedRuntimeExpectation {
                runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
                evidence_strength: expected_strength,
            };
            let observed = ObservedManagedRuntimeKind {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                evidence_strength: observed_strength,
            };
            assert_eq!(
                runtime_mismatch_verdict(expected, observed, live_turn),
                want,
                "{label}"
            );
        }
    }

    #[test]
    fn matching_runtime_never_recreates_regardless_of_liveness() {
        for live_turn in [false, true] {
            assert_eq!(
                runtime_mismatch_verdict(
                    ManagedRuntimeExpectation {
                        runtime_kind: RuntimeHandoffKind::ClaudeTui,
                        evidence_strength: RuntimeKindEvidenceStrength::Strong,
                    },
                    ObservedManagedRuntimeKind {
                        runtime_kind: RuntimeHandoffKind::ClaudeTui,
                        evidence_strength: RuntimeKindEvidenceStrength::Strong,
                    },
                    live_turn,
                ),
                RuntimeMismatchVerdict::Match
            );
        }
    }

    #[cfg(unix)]
    fn reconcile_with_observation(
        expected: ManagedRuntimeExpectation,
        observed: ObservedManagedRuntimeKind,
        live_turn: bool,
        sink: &mut Vec<String>,
    ) -> RuntimeMismatchVerdict {
        reconcile_managed_tmux_runtime_kind_for_config(
            &ProviderKind::Claude,
            ChannelId::new(50_150_001),
            Some("AgentDesk-5015-runtime"),
            Some(expected),
            |_| true,
            |_, _| Some(observed),
            || RuntimeInflightEvidence {
                open: live_turn,
                stale: false,
            },
            || crate::services::tui_turn_state::TuiTurnState::Idle,
            |_| true,
            |name, _, _| sink.push(name.to_string()),
        )
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_match_does_not_call_recreate_sink() {
        let runtime = ManagedRuntimeExpectation {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            evidence_strength: RuntimeKindEvidenceStrength::Strong,
        };
        let mut calls = Vec::new();
        assert_eq!(
            reconcile_with_observation(
                runtime,
                ObservedManagedRuntimeKind {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    evidence_strength: RuntimeKindEvidenceStrength::Strong,
                },
                false,
                &mut calls,
            ),
            RuntimeMismatchVerdict::Match
        );
        assert!(calls.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_defer_does_not_call_recreate_sink() {
        let mut calls = Vec::new();
        assert_eq!(
            reconcile_with_observation(
                ManagedRuntimeExpectation {
                    runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
                    evidence_strength: RuntimeKindEvidenceStrength::Strong,
                },
                ObservedManagedRuntimeKind {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    evidence_strength: RuntimeKindEvidenceStrength::Strong,
                },
                true,
                &mut calls,
            ),
            RuntimeMismatchVerdict::Defer
        );
        assert!(
            calls.is_empty(),
            "Defer must return before destructive wiring"
        );
    }

    #[cfg(unix)]
    #[test]
    fn weak_idle_mismatch_never_escalates_to_cleanup() {
        let channel_id = ChannelId::new(50_150_010);
        clear_test_defer(channel_id);
        let mut calls = Vec::new();
        for _ in 0..(RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT + 3) {
            let verdict = reconcile_managed_tmux_runtime_kind_for_config(
                &ProviderKind::Claude,
                channel_id,
                Some("AgentDesk-5015-weak-idle"),
                Some(ManagedRuntimeExpectation {
                    runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
                    evidence_strength: RuntimeKindEvidenceStrength::Weak,
                }),
                |_| true,
                |_, _| {
                    Some(ObservedManagedRuntimeKind {
                        runtime_kind: RuntimeHandoffKind::ClaudeTui,
                        evidence_strength: RuntimeKindEvidenceStrength::Strong,
                    })
                },
                || RuntimeInflightEvidence {
                    open: false,
                    stale: false,
                },
                || crate::services::tui_turn_state::TuiTurnState::Idle,
                |_| true,
                |name, _, _| calls.push(name.to_string()),
            );
            assert_eq!(verdict, RuntimeMismatchVerdict::Defer);
        }
        assert!(calls.is_empty());
        clear_test_defer(channel_id);
    }

    #[cfg(unix)]
    #[test]
    fn stale_inflight_with_busy_transcript_never_cleans_up() {
        let channel_id = ChannelId::new(50_150_011);
        clear_test_defer(channel_id);
        let mut calls = Vec::new();
        for _ in 0..(RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT + 3) {
            let verdict = reconcile_managed_tmux_runtime_kind_for_config(
                &ProviderKind::Claude,
                channel_id,
                Some("AgentDesk-5015-stale-busy"),
                Some(ManagedRuntimeExpectation {
                    runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
                    evidence_strength: RuntimeKindEvidenceStrength::Strong,
                }),
                |_| true,
                |_, _| {
                    Some(ObservedManagedRuntimeKind {
                        runtime_kind: RuntimeHandoffKind::ClaudeTui,
                        evidence_strength: RuntimeKindEvidenceStrength::Strong,
                    })
                },
                || RuntimeInflightEvidence {
                    open: true,
                    stale: true,
                },
                || crate::services::tui_turn_state::TuiTurnState::Streaming,
                |_| true,
                |name, _, _| calls.push(name.to_string()),
            );
            assert_eq!(verdict, RuntimeMismatchVerdict::Defer);
        }
        assert!(
            calls.is_empty(),
            "busy transcript must veto destructive cleanup"
        );
        clear_test_defer(channel_id);
    }

    #[cfg(unix)]
    #[test]
    fn stale_inflight_with_unknown_transcript_fails_closed() {
        let channel_id = ChannelId::new(50_150_012);
        clear_test_defer(channel_id);
        let mut calls = Vec::new();
        let verdict = reconcile_managed_tmux_runtime_kind_for_config(
            &ProviderKind::Claude,
            channel_id,
            Some("AgentDesk-5015-stale-unknown"),
            Some(ManagedRuntimeExpectation {
                runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
                evidence_strength: RuntimeKindEvidenceStrength::Strong,
            }),
            |_| true,
            |_, _| {
                Some(ObservedManagedRuntimeKind {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    evidence_strength: RuntimeKindEvidenceStrength::Strong,
                })
            },
            || RuntimeInflightEvidence {
                open: true,
                stale: true,
            },
            || crate::services::tui_turn_state::TuiTurnState::Unknown,
            |_| true,
            |name, _, _| calls.push(name.to_string()),
        );
        assert_eq!(verdict, RuntimeMismatchVerdict::Defer);
        assert!(
            calls.is_empty(),
            "unknown transcript evidence must fail closed"
        );
        clear_test_defer(channel_id);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_revalidation_rejects_owner_change() {
        let channel_id = ChannelId::new(50_150_013);
        clear_test_defer(channel_id);
        let mut calls = Vec::new();
        let verdict = reconcile_managed_tmux_runtime_kind_for_config(
            &ProviderKind::Claude,
            channel_id,
            Some("AgentDesk-5015-owner-change"),
            Some(ManagedRuntimeExpectation {
                runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
                evidence_strength: RuntimeKindEvidenceStrength::Strong,
            }),
            |_| true,
            |_, _| {
                Some(ObservedManagedRuntimeKind {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    evidence_strength: RuntimeKindEvidenceStrength::Strong,
                })
            },
            || RuntimeInflightEvidence {
                open: false,
                stale: false,
            },
            || crate::services::tui_turn_state::TuiTurnState::Idle,
            |_| false,
            |name, _, _| calls.push(name.to_string()),
        );
        assert_eq!(verdict, RuntimeMismatchVerdict::Defer);
        assert!(calls.is_empty());
        clear_test_defer(channel_id);
    }

    #[cfg(unix)]
    #[test]
    fn transcript_state_probe_preserves_provider_argument_order() {
        fn claude(
            current_path: Option<&str>,
            session_id: Option<&str>,
            tmux_session_name: Option<&str>,
        ) -> crate::services::tui_turn_state::TuiTurnState {
            assert_eq!(current_path, Some("/worktree/claude"));
            assert_eq!(session_id, Some("claude-session"));
            assert_eq!(tmux_session_name, Some("AgentDesk-claude"));
            crate::services::tui_turn_state::TuiTurnState::Streaming
        }
        fn codex(
            current_path: Option<&str>,
            tmux_session_name: Option<&str>,
            provider_session_id: Option<&str>,
        ) -> crate::services::tui_turn_state::TuiTurnState {
            assert_eq!(current_path, Some("/worktree/codex"));
            assert_eq!(tmux_session_name, Some("AgentDesk-codex"));
            assert_eq!(provider_session_id, Some("codex-session"));
            crate::services::tui_turn_state::TuiTurnState::Streaming
        }
        assert_eq!(
            managed_runtime_transcript_state_using(
                &ProviderKind::Claude,
                Some("/worktree/claude"),
                Some("claude-session"),
                Some("AgentDesk-claude"),
                claude,
                codex,
            ),
            crate::services::tui_turn_state::TuiTurnState::Streaming
        );
        assert_eq!(
            managed_runtime_transcript_state_using(
                &ProviderKind::Codex,
                Some("/worktree/codex"),
                Some("codex-session"),
                Some("AgentDesk-codex"),
                claude,
                codex,
            ),
            crate::services::tui_turn_state::TuiTurnState::Streaming
        );
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_recreate_calls_sink_once_for_exact_tmux_session() {
        let mut calls = Vec::new();
        assert_eq!(
            reconcile_with_observation(
                ManagedRuntimeExpectation {
                    runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
                    evidence_strength: RuntimeKindEvidenceStrength::Strong,
                },
                ObservedManagedRuntimeKind {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    evidence_strength: RuntimeKindEvidenceStrength::Strong,
                },
                false,
                &mut calls,
            ),
            RuntimeMismatchVerdict::Recreate
        );
        assert_eq!(calls, ["AgentDesk-5015-runtime"]);
    }

    #[test]
    fn prelaunch_claude_tui_seed_omits_wrapper_output_but_preserves_fifo() {
        let seed = prelaunch_inflight_runtime_seed_from_paths(
            "AgentDesk-claude-seed",
            "/runtime/wrapper-stream.log".to_string(),
            "/runtime/input.fifo".to_string(),
            true,
            Some(RuntimeHandoffKind::ClaudeTui),
        );
        assert_eq!(seed.0.as_deref(), Some("AgentDesk-claude-seed"));
        assert_eq!(
            seed.1, None,
            "ClaudeTui must wait for RuntimeReady transcript binding"
        );
        assert_eq!(seed.2.as_deref(), Some("/runtime/input.fifo"));
        assert_eq!(seed.3, 0);
    }

    #[test]
    fn prelaunch_seed_is_identical_for_intake_and_headless_callers() {
        let intake = prelaunch_inflight_runtime_seed_from_paths(
            "AgentDesk-claude-symmetric",
            "/runtime/wrapper-stream.log".to_string(),
            "/runtime/input.fifo".to_string(),
            true,
            Some(RuntimeHandoffKind::ClaudeTui),
        );
        let headless = prelaunch_inflight_runtime_seed_from_paths(
            "AgentDesk-claude-symmetric",
            "/runtime/wrapper-stream.log".to_string(),
            "/runtime/input.fifo".to_string(),
            true,
            Some(RuntimeHandoffKind::ClaudeTui),
        );
        assert_eq!(intake, headless);
    }

    #[test]
    fn managed_linked_worktree_skips_provider_reisolation_without_session_state() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();

        let git = |args: &[&str]| {
            crate::services::git::GitCommand::new()
                .repo(&repo)
                .args(args)
                .run_output()
                .unwrap();
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "provider-isolation@test.invalid"]);
        git(&["config", "user.name", "Provider Isolation Test"]);
        std::fs::write(repo.join("README"), "test").unwrap();
        git(&["add", "README"]);
        git(&["commit", "-m", "initial"]);

        let (worktree_path, _) = create_git_worktree(
            repo.to_str().unwrap(),
            "restart-reisolation",
            "test-provider",
        )
        .unwrap();

        let mut session = DiscordSession {
            session_id: Some("preserved-session".to_string()),
            memento_context_loaded: true,
            memento_reflected: false,
            current_path: Some(worktree_path.clone()),
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(43_170_001),
            channel_name: Some("restart-reisolation".to_string()),
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
        };
        reconstruct_managed_worktree_metadata(
            &mut session,
            &ProviderKind::Claude,
            ChannelId::new(43_170_001),
            &worktree_path,
        );

        assert!(session.worktree.is_some());
        assert_eq!(session.session_id.as_deref(), Some("preserved-session"));

        let mut conflicted_session = DiscordSession {
            worktree: None,
            ..session.clone()
        };
        assert!(!reconstruct_unowned_managed_worktree(
            &mut conflicted_session,
            Some("owner-channel"),
            &ProviderKind::Claude,
            ChannelId::new(43_170_003),
            &worktree_path,
        ));
        assert!(conflicted_session.worktree.is_none());

        git(&["-C", &worktree_path, "checkout", "--detach"]);
        let mut detached_session = DiscordSession {
            worktree: None,
            ..session.clone()
        };
        reconstruct_managed_worktree_metadata(
            &mut detached_session,
            &ProviderKind::Claude,
            ChannelId::new(43_170_002),
            &worktree_path,
        );
        assert!(detached_session.worktree.is_none());
    }

    fn bind_parent(
        root: &std::path::Path,
        id: ChannelId,
        prompt: &std::path::Path,
        workspace: &std::path::Path,
        thread_inherit: Option<bool>,
    ) {
        let path = crate::runtime_layout::role_map_path(root);
        std::fs::create_dir_all(path.parent().expect("role-map parent")).unwrap();
        let mut entry = serde_json::json!({
            "roleId": "parent-role",
            "promptFile": prompt,
            "workspace": workspace,
        });
        if let Some(enabled) = thread_inherit {
            entry["threadInherit"] = serde_json::Value::Bool(enabled);
        }
        let json = serde_json::json!({ "byChannelId": { (id.get().to_string()): entry } });
        std::fs::write(path, json.to_string()).unwrap();
    }
    #[test]
    fn thread_inherits_parent_role_workspace_and_memory_scope_by_default() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let prompt = root.path().join("parent-role.md");
        let workspace = root.path().join("parent-memory-workspace");
        std::fs::write(&prompt, "PARENT ROLE PROMPT").unwrap();
        std::fs::create_dir(&workspace).unwrap();
        let child = ChannelId::new(43_170_101);
        let parent = (ChannelId::new(43_170_102), Some("parent".to_string()));
        bind_parent(root.path(), parent.0, &prompt, &workspace, None);
        let resolved = resolve_thread_role_binding(child, Some("thread"), Some(&parent));
        let binding = resolved.role_binding.as_ref().expect("parent role");
        assert_eq!(binding.role_id, "parent-role");
        assert_eq!(
            resolve_thread_workspace(child, Some("thread"), Some(&parent)).as_deref(),
            workspace.to_str()
        );
        assert_eq!(resolved.memory_channel_id(child), parent.0);
        assert_eq!(resolved.memory_channel_name(None), parent.1);
        let memory = settings::ResolvedMemorySettings {
            backend: settings::MemoryBackendKind::Memento,
            ..Default::default()
        };
        let built = super::super::super::super::prompt_builder::build_system_prompt_with_manifest(
            "discord",
            &[],
            workspace.to_str().unwrap(),
            child,
            parent.0,
            "token",
            Some(binding),
            false,
            super::super::super::super::prompt_builder::PromptProfiles::foreground(
                DispatchProfile::Full,
            ),
            None,
            None,
            None,
            None,
            Some(&memory),
            true,
            true,
            None,
            None,
            None,
            None,
        );
        assert!(built.system_prompt.contains("PARENT ROLE PROMPT"));
        assert!(
            built
                .system_prompt
                .contains("workspace=agentdesk-parent-memory-workspace")
        );
        let unbound_parent = (ChannelId::new(43_170_103), Some("unbound".to_string()));
        let unbound = resolve_thread_role_binding(child, Some("thread"), Some(&unbound_parent));
        assert!(unbound.role_binding.is_none());
        assert_eq!(unbound.memory_channel_id(child), child);
    }
    #[test]
    fn thread_inherit_false_opts_out() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let prompt = std::path::Path::new("/tmp/parent-role.md");
        let workspace = std::path::Path::new("/tmp/parent-workspace");
        let child = ChannelId::new(43_170_201);
        let parent = (ChannelId::new(43_170_202), Some("parent".to_string()));
        bind_parent(root.path(), parent.0, prompt, workspace, Some(false));

        let resolved = resolve_thread_role_binding(child, Some("thread"), Some(&parent));
        assert!(resolved.role_binding.is_none());
        assert!(resolve_thread_workspace(child, Some("thread"), Some(&parent)).is_none());
        assert_eq!(resolved.memory_channel_id(child), child);
        assert_eq!(
            resolved.memory_channel_name(Some("t")).as_deref(),
            Some("t")
        );
    }

    #[test]
    fn non_thread_resolution_is_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let prompt = std::path::Path::new("/tmp/child-role.md");
        let workspace = std::path::Path::new("/tmp/child-workspace");
        let child = ChannelId::new(43_170_301);
        bind_parent(root.path(), child, prompt, workspace, Some(false));

        let resolved = resolve_thread_role_binding(child, Some("channel"), None);
        let binding = resolved.role_binding.as_ref().expect("direct child role");
        assert_eq!(binding.role_id, "parent-role");
        assert_eq!(
            resolve_thread_workspace(child, Some("channel"), None).as_deref(),
            workspace.to_str()
        );
        assert_eq!(resolved.memory_channel_id(child), child);
    }

    #[test]
    fn redirect_uses_final_parent_for_inheritance_and_fast_mode_key() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let prompt = std::path::Path::new("/tmp/final-parent-role.md");
        let workspace = std::path::Path::new("/tmp/final-parent-workspace");
        let incoming_channel = ChannelId::new(43_170_401);
        let final_thread = ChannelId::new(43_170_402);
        let final_parent = (ChannelId::new(43_170_403), Some("final-parent".to_string()));
        bind_parent(root.path(), final_parent.0, prompt, workspace, None);

        let resolved =
            resolve_thread_role_binding(final_thread, Some("dispatch-thread"), Some(&final_parent));
        assert_eq!(
            resolved
                .role_binding
                .as_ref()
                .map(|binding| binding.role_id.as_str()),
            Some("parent-role")
        );
        assert_eq!(resolved.memory_channel_id(final_thread), final_parent.0);
        assert_eq!(
            resolve_thread_workspace(final_thread, Some("dispatch-thread"), Some(&final_parent))
                .as_deref(),
            workspace.to_str()
        );
        let fast_mode_key =
            effective_fast_mode_channel_id(final_thread, Some(final_parent.clone()));
        assert_eq!(fast_mode_key, final_parent.0);
        assert_ne!(fast_mode_key, incoming_channel);
        let workspace = workspace.to_str();
        let inherited = select_final_path("/default", workspace, false);
        assert_eq!(inherited, workspace.unwrap());
        let should_update = dispatch_session_path_should_update;
        assert!(should_update(true, None, false, false, "/in", inherited));
        assert_eq!(select_final_path("/explicit", workspace, true), "/explicit");
    }
}
