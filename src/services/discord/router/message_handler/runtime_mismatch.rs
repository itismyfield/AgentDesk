use super::*;

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
    } else if expected.evidence_strength == RuntimeKindEvidenceStrength::Weak || live_turn {
        RuntimeMismatchVerdict::Defer
    } else {
        RuntimeMismatchVerdict::Recreate
    }
}

#[cfg(unix)]
fn managed_runtime_transcript_busy_using(
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
) -> bool {
    match provider {
        ProviderKind::Claude => {
            observe_claude(current_path, session_id, tmux_session_name).is_busy()
        }
        ProviderKind::Codex => observe_codex(current_path, tmux_session_name, session_id).is_busy(),
        _ => false,
    }
}

#[cfg(unix)]
fn managed_runtime_transcript_busy(
    provider: &ProviderKind,
    current_path: Option<&str>,
    session_id: Option<&str>,
    tmux_session_name: Option<&str>,
) -> bool {
    managed_runtime_transcript_busy_using(
        provider,
        current_path,
        session_id,
        tmux_session_name,
        observe_claude_tui_transcript_state_for_session,
        observe_codex_tui_rollout_state_for_cwd,
    )
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
    dashmap::DashMap<(String, u64), RuntimeMismatchDeferState>,
> = std::sync::LazyLock::new(dashmap::DashMap::new);
#[cfg(unix)]
const RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT: u32 = 3;

#[cfg(unix)]
fn clear_runtime_mismatch_defer(provider: &ProviderKind, channel_id: serenity::ChannelId) {
    RUNTIME_MISMATCH_DEFERS.remove(&(provider.as_str().to_string(), channel_id.get()));
}

#[cfg(unix)]
fn record_runtime_mismatch_defer(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    expected: ManagedRuntimeExpectation,
    observed: ObservedManagedRuntimeKind,
    open_inflight: bool,
    transcript_busy: Option<bool>,
    defer_reason: &'static str,
) -> u32 {
    let key = (provider.as_str().to_string(), channel_id.get());
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
            "action": if defer_reason != "live_turn"
                && count >= RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT
            {
                "fallback_recreate"
            } else {
                "continue_defer_without_kill"
            },
            "defer_reason": defer_reason,
            "expected_runtime_kind": expected.runtime_kind.as_str(),
            "observed_runtime_kind": observed.runtime_kind.as_str(),
            "open_inflight": open_inflight,
            "transcript_busy": transcript_busy,
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
    live_pane: impl FnOnce(&str) -> bool,
    observe: impl FnOnce(&ProviderKind, &str) -> Option<ObservedManagedRuntimeKind>,
    inflight_evidence: impl FnOnce() -> RuntimeInflightEvidence,
    transcript_busy: impl FnOnce() -> bool,
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
    let transcript_busy = (!inflight.open).then(transcript_busy);
    let live_turn = (inflight.open && !inflight.stale) || transcript_busy.unwrap_or(false);
    let verdict = runtime_mismatch_verdict(
        expected,
        observed,
        inflight.open || transcript_busy.unwrap_or(false),
    );
    if verdict.should_defer() {
        let defer_reason = if live_turn {
            "live_turn"
        } else if inflight.open {
            "stale_inflight"
        } else {
            "weak_expected_evidence"
        };
        let count = record_runtime_mismatch_defer(
            provider,
            channel_id,
            expected,
            observed,
            inflight.open,
            transcript_busy,
            defer_reason,
        );
        if live_turn || count < RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT {
            return verdict;
        }
        crate::services::observability::emit_inflight_lifecycle_event(
            provider.as_str(),
            channel_id.get(),
            None,
            None,
            None,
            "runtime_kind_mismatch_defer_fallback_recreate",
            serde_json::json!({
                "consecutive_defer_count": count,
                "defer_reason": defer_reason,
                "expected_runtime_kind": expected.runtime_kind.as_str(),
                "observed_runtime_kind": observed.runtime_kind.as_str(),
            }),
        );
        tracing::warn!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            consecutive_defer_count = count,
            defer_reason,
            "bounded runtime mismatch defer fell back to recreation"
        );
    }
    clear_runtime_mismatch_defer(provider, channel_id);
    recreate(tmux_session_name, expected, observed);
    RuntimeMismatchVerdict::Recreate
}

#[cfg(unix)]
pub(super) fn reconcile_managed_tmux_runtime_kind_using_runtime(
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
        || managed_runtime_transcript_busy(provider, current_path, session_id, tmux_session_name),
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

#[cfg(test)]
mod tests {
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
                RuntimeMismatchVerdict::Recreate,
                "moderate observed evidence preserves strong idle config transition",
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
            || false,
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
    fn weak_idle_mismatch_falls_back_at_escalation_threshold() {
        let channel_id = ChannelId::new(50_150_010);
        clear_test_defer(channel_id);
        let mut calls = Vec::new();
        for attempt in 1..=RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT {
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
                || false,
                |name, _, _| calls.push(name.to_string()),
            );
            let expected = if attempt < RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT {
                RuntimeMismatchVerdict::Defer
            } else {
                RuntimeMismatchVerdict::Recreate
            };
            assert_eq!(verdict, expected);
        }
        assert_eq!(calls, ["AgentDesk-5015-weak-idle"]);
        clear_test_defer(channel_id);
    }

    #[cfg(unix)]
    #[test]
    fn stale_inflight_falls_back_but_live_turn_never_recreates() {
        let expected = ManagedRuntimeExpectation {
            runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
            evidence_strength: RuntimeKindEvidenceStrength::Strong,
        };
        let observed = ObservedManagedRuntimeKind {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            evidence_strength: RuntimeKindEvidenceStrength::Strong,
        };
        for (channel_raw, evidence, should_recreate) in [
            (
                50_150_011,
                RuntimeInflightEvidence {
                    open: true,
                    stale: true,
                },
                true,
            ),
            (
                50_150_012,
                RuntimeInflightEvidence {
                    open: true,
                    stale: false,
                },
                false,
            ),
        ] {
            let channel_id = ChannelId::new(channel_raw);
            clear_test_defer(channel_id);
            let mut calls = Vec::new();
            for _ in 0..(RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT + 3) {
                let verdict = reconcile_managed_tmux_runtime_kind_for_config(
                    &ProviderKind::Claude,
                    channel_id,
                    Some("AgentDesk-5015-inflight"),
                    Some(expected),
                    |_| true,
                    |_, _| Some(observed),
                    || evidence,
                    || false,
                    |name, _, _| calls.push(name.to_string()),
                );
                if !should_recreate {
                    assert_eq!(verdict, RuntimeMismatchVerdict::Defer);
                }
            }
            if should_recreate {
                assert_eq!(calls.len(), 2, "fallback resets the consecutive counter");
            } else {
                assert!(calls.is_empty(), "live turns must defer without a bound");
            }
            clear_test_defer(channel_id);
        }
    }

    #[cfg(unix)]
    #[test]
    fn transcript_busy_probe_preserves_provider_argument_order() {
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
        assert!(managed_runtime_transcript_busy_using(
            &ProviderKind::Claude,
            Some("/worktree/claude"),
            Some("claude-session"),
            Some("AgentDesk-claude"),
            claude,
            codex,
        ));
        assert!(managed_runtime_transcript_busy_using(
            &ProviderKind::Codex,
            Some("/worktree/codex"),
            Some("codex-session"),
            Some("AgentDesk-codex"),
            claude,
            codex,
        ));
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
}
