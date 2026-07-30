use super::*;

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

pub(super) fn clear_runtime_mismatch_defer(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) {
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
pub(super) struct RuntimeInflightEvidence {
    pub(super) open: bool,
    pub(super) stale: bool,
}

#[cfg(unix)]
pub(super) fn reconcile_managed_tmux_runtime_kind_for_config(
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
pub(in crate::services::discord) fn reconcile_managed_tmux_runtime_kind_using_runtime(
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
                crate::services::discord::inflight::load_inflight_state(provider, channel_id.get());
            RuntimeInflightEvidence {
                open: state
                    .as_ref()
                    .is_some_and(|state| !state.terminal_delivery_completed()),
                stale: state.as_ref().is_some_and(|state| {
                    crate::services::discord::inflight::inflight_state_is_stale(
                        state,
                        chrono::Utc::now().timestamp(),
                        crate::services::discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS,
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
