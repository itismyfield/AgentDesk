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
    incident_identity: Option<u64>,
}

#[cfg(unix)]
static RUNTIME_MISMATCH_DEFERS: std::sync::LazyLock<
    dashmap::DashMap<RuntimeMismatchTarget, RuntimeMismatchDeferState>,
> = std::sync::LazyLock::new(dashmap::DashMap::new);
#[cfg(unix)]
static RUNTIME_MISMATCH_DEFERS_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));
#[cfg(unix)]
const RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT: u32 = 3;

pub(super) fn clear_runtime_mismatch_defer(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) {
    #[cfg(unix)]
    {
        let _state_lock = RUNTIME_MISMATCH_DEFERS_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        RUNTIME_MISMATCH_DEFERS.retain(|target, _| {
            target.provider != provider.as_str() || target.channel_id != channel_id.get()
        });
    }
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
    incident_identity: Option<u64>,
    target_still_owned: impl FnOnce() -> bool,
) -> (u32, bool, bool) {
    let _state_lock = RUNTIME_MISMATCH_DEFERS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key =
        RuntimeMismatchTarget::new(provider, channel_id, tmux_session_name, expected, observed);
    RUNTIME_MISMATCH_DEFERS.retain(|target, _| {
        target.provider != key.provider || target.channel_id != key.channel_id || target == &key
    });
    let mut state = RUNTIME_MISMATCH_DEFERS
        .entry(key.clone())
        .or_insert_with(|| RuntimeMismatchDeferState {
            count: 0,
            first_seen: std::time::Instant::now(),
            escalated: false,
            incident_identity,
        });
    // A changed open-turn identity proves the previous mismatch observation
    // belonged to a completed incident; the count remains cumulative so busy
    // follow-up turns cannot indefinitely postpone the threshold.
    if incident_identity.is_some()
        && state.incident_identity.is_some()
        && state.incident_identity != incident_identity
    {
        state.escalated = false;
        state.incident_identity = incident_identity;
    }
    state.count = state.count.saturating_add(1);
    let elapsed_secs = state.first_seen.elapsed().as_secs();
    let escalating = !state.escalated && state.count >= RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT;
    let count = state.count;
    drop(state);
    let owner_matches = !escalating || target_still_owned();
    if escalating && owner_matches {
        if let Some(mut state) = RUNTIME_MISMATCH_DEFERS.get_mut(&key) {
            state.escalated = true;
        }
    }

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
    (count, escalating, owner_matches)
}

#[cfg(unix)]
pub(super) fn build_runtime_mismatch_escalation_notice(
    channel_id: serenity::ChannelId,
    tmux_session_name: &str,
    expected: ManagedRuntimeExpectation,
    observed: ObservedManagedRuntimeKind,
) -> String {
    format!(
        "⚠️ Runtime mismatch deferred\nchannel_id: {}\nsession: {}\nobserved runtime: {}\nexpected runtime: {}\nThe session continues without kill/recreate.",
        channel_id.get(),
        tmux_session_name,
        observed.runtime_kind.as_str(),
        expected.runtime_kind.as_str(),
    )
}

#[cfg(unix)]
#[derive(Clone, Copy)]
pub(super) struct RuntimeInflightEvidence {
    pub(super) open: bool,
    pub(super) stale: bool,
    pub(super) incident_identity: Option<u64>,
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
    on_escalation: impl Fn(
        &ProviderKind,
        serenity::ChannelId,
        &str,
        ManagedRuntimeExpectation,
        ObservedManagedRuntimeKind,
    ),
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
        // Moderate observed evidence is intentionally a permanent defer here:
        // no exact-identity repair/reattach path can safely promote this legacy
        // session, and the downstream relay watcher does not revisit this
        // verdict. Exact-identity kill/recreate remains a separate follow-up (#5062).
        let defer_reason = if live_turn {
            "live_turn"
        } else if destructive_evidence_unknown {
            "transcript_state_unknown"
        } else if expected.evidence_strength != RuntimeKindEvidenceStrength::Strong {
            "weak_expected_evidence"
        } else if observed.evidence_strength != RuntimeKindEvidenceStrength::Strong {
            "weak_observed_evidence"
        } else {
            "runtime_kind_mismatch"
        };
        let (_defer_count, escalated, owner_matches) = record_runtime_mismatch_defer(
            provider,
            channel_id,
            tmux_session_name,
            expected,
            observed,
            inflight.open,
            transcript_state,
            defer_reason,
            inflight.incident_identity,
            || target_still_owned(tmux_session_name),
        );
        if escalated && owner_matches {
            on_escalation(provider, channel_id, tmux_session_name, expected, observed);
        } else if escalated {
            tracing::warn!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session_name,
                "runtime mismatch escalation skipped because tmux ownership was lost"
            );
        }
        return verdict;
    }
    let revalidated_observed = observe(provider, tmux_session_name);
    let revalidated_inflight = inflight_evidence();
    let pane_live_at_commit = live_pane(tmux_session_name);
    let owner_matches_at_commit = target_still_owned(tmux_session_name);
    // A stale inflight row may classify the transcript as idle, but it remains
    // durable ownership evidence until an existing identity-guarded recovery
    // path clears it. Never kill while any open row remains; repeated calls will
    // re-probe and proceed once that owner is actually gone.
    let target_unchanged = pane_live_at_commit
        && owner_matches_at_commit
        && revalidated_observed == Some(observed)
        && !revalidated_inflight.open;
    if !target_unchanged {
        let defer_reason = "destructive_target_revalidation_failed";
        let (_defer_count, escalated, owner_matches) = record_runtime_mismatch_defer(
            provider,
            channel_id,
            tmux_session_name,
            expected,
            revalidated_observed.unwrap_or(observed),
            revalidated_inflight.open,
            None,
            defer_reason,
            revalidated_inflight.incident_identity,
            || owner_matches_at_commit,
        );
        if escalated && owner_matches {
            on_escalation(
                provider,
                channel_id,
                tmux_session_name,
                expected,
                revalidated_observed.unwrap_or(observed),
            );
        } else if escalated {
            tracing::warn!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session_name,
                "runtime mismatch escalation skipped because tmux ownership was lost"
            );
        }
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
                incident_identity: state.as_ref().map(|state| state.user_msg_id),
            }
        },
        || managed_runtime_transcript_state(provider, current_path, session_id, tmux_session_name),
        |tmux_session_name| {
            shared
                .tmux_watchers
                .owner_channel_for_tmux_session(tmux_session_name)
                == Some(channel_id)
        },
        |provider, channel_id, tmux_session_name, expected, observed| {
            let Some(http) = shared.serenity_http_or_token_fallback() else {
                tracing::warn!(
                    provider = provider.as_str(),
                    channel_id = channel_id.get(),
                    tmux_session_name,
                    "skipping runtime mismatch escalation notice; provider serenity http unavailable"
                );
                return;
            };
            let session_name = tmux_session_name.to_string();
            let message = build_runtime_mismatch_escalation_notice(
                channel_id,
                &session_name,
                expected,
                observed,
            );
            let provider_name = provider.as_str().to_string();
            tokio::spawn(async move {
                if let Err(error) = channel_id.say(&*http, message).await {
                    tracing::warn!(
                        provider = %provider_name,
                        channel_id = channel_id.get(),
                        tmux_session_name = %session_name,
                        error = %error,
                        "failed to send runtime mismatch escalation notice"
                    );
                }
            });
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
