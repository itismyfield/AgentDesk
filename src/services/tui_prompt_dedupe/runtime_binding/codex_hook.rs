//! Verifies and publishes Codex hook sources under the existing pane authority.

use super::*;

/// Launch paths let the execution's context name the cause of a new source.
pub(crate) fn register_launched_tmux_runtime_binding(
    tmux_session_name: &str,
    binding: TuiRuntimeBinding,
) {
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        register_launched_tmux_runtime_binding_under_source_authority(authority, binding)
    });
}

pub(crate) fn register_launched_tmux_runtime_binding_under_source_authority(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    binding: TuiRuntimeBinding,
) -> bool {
    publish_runtime_binding(authority, binding, None, CauseSource::Launch, Record::Stat)
        .is_some_and(Persisted::published)
}

/// A Herdr Codex pane before its first prompt: the hook that records its source finds a Codex pane
/// on the channel, while the empty path gives no reader or relay and logs no binding event.
pub(crate) fn register_codex_herdr_placeholder(logical: &str, channel_id: u64) {
    super::register_tmux_channel(logical, channel_id);
    crate::services::tmux_common::with_tmux_source_authority(logical, |authority| {
        with_runtime_binding_state_under_source_authority(authority, |state| {
            let placeholder = TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: String::new(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            };
            let recorded_at = Instant::now();
            let entry = TimedValue {
                value: placeholder,
                recorded_at,
            };
            state
                .runtime_by_tmux
                .entry(logical.to_owned())
                .or_insert(entry);
        })
    });
}

/// Child rollouts cannot supply the output source of a Codex TUI pane.
pub(crate) fn codex_tui_binding_is_subagent(
    tmux_session_name: &str,
    binding: &TuiRuntimeBinding,
) -> bool {
    let child = binding.runtime_kind == RuntimeHandoffKind::CodexTui
        && crate::services::codex_tui::rollout_index::rollout_is_subagent(std::path::Path::new(
            &binding.output_path,
        ));
    if child {
        tracing::warn!(tmux_session_name, rollout_path = %binding.output_path,
            "refusing Codex TUI runtime binding to a subagent rollout");
    }
    child
}

use crate::services::claude_tui::hook_server::adoption_retry::{DurableKind, NotDurableReason};
use crate::services::claude_tui::hook_server::observation_ingress::{
    IngressOutcome, NotApplicableReason, UnavailableReason,
};
use crate::services::codex_tui::session::{
    self,
    source_observation::{CodexHookSourceClaim, CodexRolloutSource, verify_codex_hook_source},
};
use crate::services::tui_prompt_dedupe::binding_context::{
    CapturedContext, HookBindingEnvelope, SpawnNonceMarker, observe_spawn_nonce_marker,
};

#[cfg(test)]
thread_local! { pub(crate) static SHADOW_IO_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
fn shadow_mutant(name: &str) -> bool {
    std::env::var("AGENTDESK_CODEX_SHADOW_TEST_MUTATION").is_ok_and(|value| value == name)
}

/// ObservationOnly uses the existing tracing sink; never installs or publishes a source.
pub(crate) fn observe_codex_shadow(
    command: Option<&str>,
    payload_session: Option<&str>,
    payload: &serde_json::Value,
    hook: &HookSignal,
    envelope: Option<&HookBindingEnvelope>,
) {
    if !matches!(hook.event.as_str(), "session_start" | "user_prompt_submit") {
        return;
    }
    use crate::services::codex_tui::session::source_observation::{
        CodexFirstProof, CodexFirstProofRejection, codex_first_proof_candidate,
    };
    use crate::services::tui_prompt_dedupe::binding_context::execution_context;
    let context = envelope.and_then(|e| match &e.context {
        CapturedContext::Captured(c) => Some(c),
        _ => None,
    });
    let mut generic_precedent_seqs = Vec::new();
    let result = context.zip(payload_session).and_then(|(captured, id)| {
        crate::services::tmux_common::with_tmux_source_authority(&captured.tmux_session, |_| {
            #[cfg(test)]
            SHADOW_IO_CALLS.with(|calls| calls.set(calls.get() + 1));
            let prepared = match execution_context("codex", &captured.execution_nonce) {
                Ok(c) => c,
                Err(_) => return Some(Err(CodexFirstProofRejection::Context)),
            };
            let nonce = match observe_spawn_nonce_marker(&prepared.tmux_session) {
                SpawnNonceMarker::Known(n) => Some(n),
                _ => None,
            };
            let history = prepared
                .channel_id
                .and_then(|channel| binding_events::records_strict(channel).ok()?.ok());
            #[cfg(test)]
            let nonce = if shadow_mutant("current") {
                Some(prepared.execution_nonce.clone())
            } else {
                nonce
            };
            let verified = codex_first_proof_candidate(
                &CodexFirstProof {
                    captured,
                    prepared: &prepared,
                    current_nonce: nonce.as_deref(),
                    verified_fresh_spawn: nonce.as_deref() == Some(&prepared.execution_nonce),
                    no_prior_claim_or_transition: true,
                    event: match hook.event.as_str() {
                        "session_start" => "SessionStart",
                        "user_prompt_submit" => "UserPromptSubmit",
                        _ => "other",
                    },
                    source: payload["source"].as_str(),
                    prompt: &payload["prompt"],
                },
                &CodexHookSourceClaim {
                    session_id: id,
                    transcript_path: hook.transcript_path.as_deref().map(std::path::Path::new),
                    expected_source: CodexRolloutSource::Cli,
                },
            );
            Some(verified.and_then(|source| {
                #[cfg(test)]
                if shadow_mutant("before")
                    && history.as_ref().is_some_and(|rows| {
                        rows.iter().any(|r| {
                            r.provider == "codex"
                                && r.tmux_session == prepared.tmux_session
                                && r.execution_nonce.as_deref() == Some(&prepared.execution_nonce)
                                && (r.evidence.hook_event.is_some()
                                    || r.cause != binding_events::BindingCause::Startup)
                        })
                    })
                {
                    return Err(CodexFirstProofRejection::Ineligible);
                }
                #[cfg(test)]
                let history = if shadow_mutant("history_error") && history.is_none() {
                    Some(Vec::new())
                } else {
                    history
                };
                let seqs = history
                    .as_deref()
                    .and_then(|history| shadow_generic_precedents(&prepared, &source, history))
                    .ok_or(CodexFirstProofRejection::Ineligible)?;
                #[cfg(test)]
                let omit_final = shadow_mutant("final_identity");
                #[cfg(not(test))]
                let omit_final = false;
                if !omit_final {
                    let final_source = verify_codex_hook_source(
                        prepared
                            .provider_root
                            .as_deref()
                            .ok_or(CodexFirstProofRejection::Context)?,
                        &CodexHookSourceClaim {
                            session_id: id,
                            transcript_path: Some(&source.rollout_path),
                            expected_source: CodexRolloutSource::Cli,
                        },
                    )
                    .map_err(CodexFirstProofRejection::Native)?;
                    if final_source.identity != source.identity
                        || final_source.rollout_path != source.rollout_path
                        || final_source.created_at != source.created_at
                    {
                        return Err(CodexFirstProofRejection::Native(
                            session::source_observation::CodexHookSourceRejection::RolloutReplaced,
                        ));
                    }
                }
                generic_precedent_seqs = seqs;
                Ok(source)
            }))
        })
    });
    let verdict = match &result {
        Some(Ok(_)) => "candidate",
        Some(Err(error)) => error.verdict(),
        None => "ineligible",
    };
    let legacy = context.and_then(|c| super::super::peek_tmux_runtime_binding(&c.tmux_session));
    let verified = result.as_ref().and_then(|r| r.as_ref().ok());
    tracing::info!(scope = "ObservationOnly", verdict, reason = ?result.as_ref().and_then(|r| r.as_ref().err()),
        channel = ?context.and_then(|c| c.channel_id), tmux = ?context.map(|c| &c.tmux_session),
        nonce = ?context.map(|c| &c.execution_nonce), root = ?context.and_then(|c| c.provider_root.as_ref()),
        native_uuid = ?payload_session, path = ?verified.map(|v| &v.rollout_path), identity = ?verified.map(|v| &v.identity),
        event = ?hook.event, legacy_selected_id = ?legacy.as_ref().and_then(|b| b.session_id.as_deref()),
        source_less = legacy.is_none(), command_present = command.is_some(), ownership_promoted = false,
        generic_precedent_neutralized = !generic_precedent_seqs.is_empty(), generic_precedent_seqs = ?generic_precedent_seqs,
        "Codex local first-proof shadow observation");
}

fn shadow_generic_precedents(
    prepared: &crate::services::tui_prompt_dedupe::binding_context::BindingContext,
    verified: &session::source_observation::VerifiedCodexHookSource,
    history: &[binding_events::BindingEvent],
) -> Option<Vec<u64>> {
    #[cfg(unix)]
    use crate::services::cluster::stream_relay::SourceFileIdentity;
    use binding_events::{BindingCause as Cause, BindingTarget as Target};
    #[cfg(test)]
    let history = if shadow_mutant("last_only") {
        &history[history.len().saturating_sub(1)..]
    } else {
        history
    };
    let matches = |source: &binding_events::SourceId| {
        #[cfg(test)]
        if shadow_mutant("identity") {
            return true;
        }
        #[cfg(unix)]
        {
            source.session_id == verified.session_id
                && source.path.canonicalize().ok().as_ref() == Some(&verified.rollout_path)
                && verified.identity
                    == SourceFileIdentity::Unix {
                        dev: source.dev,
                        ino: source.ino,
                    }
        }
        #[cfg(not(unix))]
        {
            let _ = (source, verified);
            false
        }
    };
    let mut seqs = Vec::new();
    let mut previous = None;
    let mut current_seen = false;
    for record in history {
        if Some(record.channel_id) != prepared.channel_id {
            return None;
        }
        if record.provider != "codex" || record.tmux_session != prepared.tmux_session {
            continue;
        }
        #[cfg(test)]
        if shadow_mutant("all_unknown") && record.cause == Cause::Unknown {
            seqs.push(record.seq);
            continue;
        }
        let current = record.execution_nonce.as_deref() == Some(&prepared.execution_nonce);
        let source = match &record.new {
            Target::Source(source) | Target::Resolved { source, .. } => Some(source),
            _ => None,
        };
        if !current {
            if current_seen {
                return None;
            }
            let named = match &record.new {
                Target::Pending {
                    payload_session_id, ..
                }
                | Target::Rejected {
                    payload_session_id, ..
                } => payload_session_id == &verified.session_id,
                _ => false,
            };
            #[cfg(test)]
            if shadow_mutant("prior_native") {
                previous = source;
                continue;
            }
            if named
                || source
                    .into_iter()
                    .chain(record.old.as_ref())
                    .chain(record.parent_hint.as_ref())
                    .any(|source| source.session_id == verified.session_id || matches(source))
            {
                return None;
            }
            if let Some(source) = source {
                previous = Some(source);
            }
            continue;
        }
        let Target::Source(source) = &record.new else {
            return None;
        };
        #[cfg(test)]
        let check_old = !shadow_mutant("old_chain");
        #[cfg(not(test))]
        let check_old = true;
        if record.evidence.hook_event.is_some()
            || record.parent_hint.is_some()
            || !matches!(record.cause, Cause::Unknown | Cause::Startup)
            || !matches(source)
            || (check_old && current_seen && record.old.as_ref() != previous)
            || (check_old
                && !current_seen
                && record.old.is_some()
                && record.old.as_ref() != previous)
        {
            return None;
        }
        current_seen = true;
        previous = Some(source);
        seqs.push(record.seq);
    }
    // Creation time only excludes old native reuse; it never establishes ownership.
    #[cfg(test)]
    if shadow_mutant("timestamp") {
        return Some(seqs);
    }
    #[cfg(test)]
    if shadow_mutant("shadow_branch") && !seqs.is_empty() {
        return None;
    }
    if !seqs.is_empty()
        && (prepared.source_policy.as_deref() != Some("shadow")
            || !verified
                .created_at
                .is_some_and(|time| time >= prepared.created_at))
    {
        return None;
    }
    Some(seqs)
}

fn reject(reason: NotApplicableReason, session: &str) -> IngressOutcome {
    tracing::warn!(
        session,
        ?reason,
        "Codex hook binding observation rejected; old source retained"
    );
    IngressOutcome::NotApplicable(reason)
}

pub(crate) fn observe_codex_hook(
    command: &str,
    payload_session: &str,
    hook: &HookSignal,
    envelope: Option<&HookBindingEnvelope>,
) -> IngressOutcome {
    let Some(CapturedContext::Captured(context)) = envelope.map(|e| &e.context) else {
        return reject(
            NotApplicableReason::CodexContextUnavailable,
            payload_session,
        );
    };
    let tmux = resolve_tmux_session_name("codex", command);
    if context.schema != 1
        || context.provider != "codex"
        || tmux.as_deref() != Some(context.tmux_session.as_str())
    {
        return reject(
            NotApplicableReason::CodexContextUnavailable,
            payload_session,
        );
    }
    crate::services::tmux_common::with_tmux_source_authority(&context.tmux_session, |authority| {
        if observe_spawn_nonce_marker(authority.session())
            != SpawnNonceMarker::Known(context.execution_nonce.clone())
        {
            return reject(
                NotApplicableReason::CodexContextUnavailable,
                payload_session,
            );
        }
        let sessions_root = context.provider_root.clone();
        let Some(root) = sessions_root.as_deref().filter(|root| root.is_absolute()) else {
            return reject(
                NotApplicableReason::CodexContextUnavailable,
                payload_session,
            );
        };
        with_runtime_binding_state_under_source_authority(authority, |state| {
            let Some(old) = state
                .runtime_by_tmux
                .get(authority.session())
                .map(|b| b.value.clone())
            else {
                return IngressOutcome::Unavailable(UnavailableReason::RestoreNotReady);
            };
            if old.runtime_kind != RuntimeHandoffKind::CodexTui
                || context.channel_id.filter(|id| *id != 0)
                    != state
                        .channel_by_tmux
                        .get(authority.session())
                        .map(|c| c.value)
                || context.channel_id.is_none()
            {
                return reject(
                    NotApplicableReason::CodexContextUnavailable,
                    payload_session,
                );
            }
            match binding_events::codex::superseded(context, payload_session) {
                Ok(false) => {}
                Ok(true) => {
                    return reject(NotApplicableReason::CodexSourceRejected, payload_session);
                }
                Err(error) => {
                    tracing::error!(%error, payload_session, "Codex binding history unavailable");
                    return IngressOutcome::NotDurable(NotDurableReason::Append);
                }
            }
            let verified = match verify_codex_hook_source(
                root,
                &CodexHookSourceClaim {
                    session_id: payload_session,
                    transcript_path: hook.transcript_path.as_deref().map(std::path::Path::new),
                    expected_source: CodexRolloutSource::Cli,
                },
            ) {
                Ok(verified) => Some(verified),
                Err(error) if error.may_resolve_later() => {
                    tracing::debug!(?error, payload_session, "Codex source awaits verification");
                    None
                }
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        payload_session,
                        "Codex source verification rejected"
                    );
                    return reject(NotApplicableReason::CodexSourceRejected, payload_session);
                }
            };
            let changed = match binding_events::codex::record(
                context,
                payload_session,
                hook,
                verified.as_ref(),
            ) {
                Ok(changed) => changed,
                Err(error) => {
                    tracing::error!(%error, payload_session, "Codex binding event persistence failed");
                    return IngressOutcome::NotDurable(NotDurableReason::Append);
                }
            };
            let Some(verified) = verified else {
                return IngressOutcome::Durable(DurableKind::Pending);
            };
            let path = verified.rollout_path.to_string_lossy().into_owned();
            if !changed
                && old.output_path == path
                && old.session_id.as_deref() == Some(payload_session)
            {
                return IngressOutcome::Durable(DurableKind::AlreadyRecorded);
            }
            if let Err(error) = session::write_codex_tui_rollout_marker_under_source_authority(
                authority,
                &verified.rollout_path,
                Some(&verified.session_id),
                Some(0),
            ) {
                tracing::error!(
                    error,
                    payload_session,
                    "Codex rollout marker publication failed"
                );
                return IngressOutcome::NotDurable(NotDurableReason::Append);
            }
            state.runtime_by_tmux.insert(
                authority.session().to_owned(),
                TimedValue {
                    value: TuiRuntimeBinding {
                        output_path: path,
                        relay_output_path: None,
                        session_id: Some(verified.session_id),
                        last_offset: 0,
                        relay_last_offset: Some(0),
                        ..old
                    },
                    recorded_at: Instant::now(),
                },
            );
            IngressOutcome::Durable(DurableKind::Adopted)
        })
    })
}

/// The hook-recorded source of this execution when `binding` names one it already retired.
fn retiring_source(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    channel_id: u64,
    binding: &TuiRuntimeBinding,
) -> std::io::Result<Option<binding_events::SourceId>> {
    let SpawnNonceMarker::Known(nonce) = observe_spawn_nonce_marker(authority.session()) else {
        return Ok(None);
    };
    if binding.runtime_kind != RuntimeHandoffKind::CodexTui {
        return Ok(None);
    }
    binding_events::codex::source_ahead(
        channel_id,
        authority.session(),
        &nonce,
        binding.session_id.as_deref(),
        std::path::Path::new(&binding.output_path),
    )
}

/// A finished tail must not republish a source that a later hook already replaced,
/// nor, with hooks on, one whose hook history cannot be read.
pub(crate) fn codex_tail_source_retired(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    binding: &TuiRuntimeBinding,
) -> bool {
    let channel = with_runtime_binding_state_under_source_authority(authority, |state| {
        state
            .channel_by_tmux
            .get(authority.session())
            .map(|c| c.value)
    });
    let Some(channel) = channel else {
        return false;
    };
    match retiring_source(authority, channel, binding) {
        Ok(retired) => retired.is_some(),
        // With hooks off no hook can have moved the pane, so the tail installs as before.
        Err(error) if !crate::services::codex::codex_direct_tui_hook_overrides_enabled() => {
            tracing::warn!(%error, tmux_session = authority.session(), "Codex binding history unreadable");
            false
        }
        Err(error) => {
            tracing::error!(
                %error,
                tmux_session = authority.session(),
                "Codex binding history unreadable; the tail source is held"
            );
            true
        }
    }
}

/// Runs `publish` unless a hook replaced `binding`'s source; the check and `publish` share one authority.
pub(crate) fn publish_unless_codex_tail_retired(
    binding: &TuiRuntimeBinding,
    tmux_session_name: &str,
    publish: impl FnOnce(),
) -> bool {
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        let publishes = !codex_tail_source_retired(authority, binding);
        if publishes {
            publish();
        }
        publishes
    })
}

/// Restore completes a hook source whose event is durable but whose marker was never written.
pub(super) fn restored_source(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    provider: &str,
    channel_id: u64,
    binding: TuiRuntimeBinding,
) -> Option<TuiRuntimeBinding> {
    // An unreadable history restores the marker: dropping the binding would leave the pane unobservable.
    let Some(current) = (provider == "codex")
        .then(|| {
            retiring_source(authority, channel_id, &binding).unwrap_or_else(|error| {
                tracing::warn!(%error, tmux_session = authority.session(), "Codex binding history unreadable");
                None
            })
        })
        .flatten()
        .filter(binding_events::codex::source_file_matches)
    else {
        return Some(binding);
    };
    let session_id = Some(current.session_id.as_str()).filter(|id| !id.is_empty());
    if let Err(error) = session::write_codex_tui_rollout_marker_under_source_authority(
        authority,
        &current.path,
        session_id,
        Some(0),
    ) {
        tracing::warn!(
            error,
            tmux_session = authority.session(),
            "Codex restore deferred"
        );
        return None;
    }
    Some(TuiRuntimeBinding {
        output_path: current.path.display().to_string(),
        session_id: session_id.map(str::to_owned),
        last_offset: std::fs::metadata(&current.path).map_or(0, |meta| meta.len()),
        ..binding
    })
}
