//! Verifies and publishes Codex hook sources under the existing pane authority.

use super::*;
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
