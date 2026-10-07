//! Pinned source policy separates legacy use from unavailable verified evidence.

use super::codex_verified::DeliveryPermission;
use super::*;
use crate::services::tmux_common::{self as tc, TmuxSourceAuthority};
use crate::services::tui_prompt_dedupe::binding_context::{self, BindingContext, SpawnNonceMarker};

pub(super) enum SourcePolicyState {
    Legacy,
    VerifiedCurrent(BindingContext),
    VerifiedUnavailable,
}

pub(super) fn disposition(authority: &TmuxSourceAuthority<'_>) -> SourcePolicyState {
    match codex_verified::current_context(authority) {
        Ok(Some(context)) => return SourcePolicyState::VerifiedCurrent(context),
        Err(_) => return SourcePolicyState::VerifiedUnavailable,
        Ok(None) => {}
    }
    let nonce = match binding_context::observe_spawn_nonce_marker(authority.session()) {
        SpawnNonceMarker::Known(nonce) => nonce,
        _ => {
            // Missing launch files cannot erase an already durable canary pin.
            return if authority.session() == crate::services::codex_tui::canary::CANARY_TMUX
                && match binding_events::codex::context_for_nonce(
                    crate::services::codex_tui::canary::CANARY_CHANNEL,
                    authority.session(),
                    None,
                ) {
                    Ok(Some(context)) => context.source_policy.as_deref() == Some("verified"),
                    Err(_) => true,
                    Ok(None) => false,
                } {
                SourcePolicyState::VerifiedUnavailable
            } else {
                SourcePolicyState::Legacy
            };
        }
    };
    if let Ok(context) = binding_context::execution_context("codex", &nonce) {
        if context.source_policy.as_deref() != Some("verified") {
            return SourcePolicyState::Legacy;
        }
    }
    // The fixed canary log is launch evidence even before its marker was published.
    if authority.session() == crate::services::codex_tui::canary::CANARY_TMUX {
        match binding_events::codex::context_for_nonce(
            crate::services::codex_tui::canary::CANARY_CHANNEL,
            authority.session(),
            None,
        ) {
            Ok(Some(context)) if context.source_policy.as_deref() == Some("verified") => {
                return SourcePolicyState::VerifiedUnavailable;
            }
            Err(_) => return SourcePolicyState::VerifiedUnavailable,
            _ => {}
        }
        if crate::services::codex_tui::canary::enabled_for(authority.session()) {
            return SourcePolicyState::VerifiedUnavailable;
        }
    }
    SourcePolicyState::Legacy
}

pub(crate) fn codex_verified_requires_proof_under_source_authority(
    authority: &TmuxSourceAuthority<'_>,
) -> bool {
    !matches!(disposition(authority), SourcePolicyState::Legacy)
}

pub(crate) fn codex_verified_requires_proof(tmux: &str) -> bool {
    tc::with_tmux_source_authority(tmux, codex_verified_requires_proof_under_source_authority)
}

pub(crate) fn codex_verified_source_allowed_under_source_authority(
    authority: &TmuxSourceAuthority<'_>,
    path: &str,
    session_id: Option<&str>,
) -> bool {
    let context = match disposition(authority) {
        SourcePolicyState::Legacy => return true,
        SourcePolicyState::VerifiedUnavailable => return false,
        SourcePolicyState::VerifiedCurrent(context) => context,
    };
    let Ok(fold) = binding_events::codex::read_ownership(&context) else {
        return false;
    };
    let Some(proof) = fold.verified else {
        return false;
    };
    let binding = TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::CodexTui,
        output_path: path.to_owned(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session_id.unwrap_or(&proof.source.session_id).to_owned()),
        last_offset: 0,
        relay_last_offset: Some(0),
    };
    codex_verified::consumer_allowed(authority, &binding)
}

pub(crate) fn codex_verified_source_allowed(
    tmux: &str,
    path: &str,
    session_id: Option<&str>,
) -> bool {
    tc::with_tmux_source_authority(tmux, |a| {
        codex_verified_source_allowed_under_source_authority(a, path, session_id)
    })
}

pub(crate) fn codex_verified_input_blocked(tmux: &str) -> bool {
    tc::with_tmux_source_authority(tmux, |authority| match disposition(authority) {
        SourcePolicyState::Legacy => false,
        SourcePolicyState::VerifiedUnavailable => true,
        SourcePolicyState::VerifiedCurrent(context) => {
            let binding = with_runtime_binding_state_under_source_authority(authority, |state| {
                state
                    .runtime_by_tmux
                    .get(authority.session())
                    .map(|entry| entry.value.clone())
            });
            !binding.is_some_and(|binding| codex_verified::consumer_allowed(authority, &binding))
                || codex_verified::permission(&context) != DeliveryPermission::Allowed
        }
    })
}

pub(crate) fn codex_verified_channel_delivery_allowed(channel: u64) -> bool {
    let panes = verified_panes(channel);
    panes
        .into_iter()
        .all(|tmux| !codex_verified_input_blocked(&tmux))
}

pub(crate) fn codex_verified_event_allowed(event: &binding_events::BindingEvent) -> bool {
    if event.provider != "codex" {
        return true;
    }
    tc::with_tmux_source_authority(&event.tmux_session, |authority| {
        let context = match disposition(authority) {
            SourcePolicyState::Legacy => return true,
            SourcePolicyState::VerifiedUnavailable => return false,
            SourcePolicyState::VerifiedCurrent(context) => context,
        };
        if event.execution_nonce.as_deref() != Some(&context.execution_nonce)
            || event.channel_id != context.channel_id.unwrap_or(0)
        {
            return false;
        }
        let source = match &event.new {
            binding_events::BindingTarget::Source(source)
            | binding_events::BindingTarget::Resolved { source, .. } => source,
            _ => return true,
        };
        if !binding_events::codex::proof_at_seq(&context, event.seq)
            .is_ok_and(|proof| proof.is_some_and(|proof| &proof.source == source))
        {
            return false;
        }
        owned_source_allowed(authority, source)
    })
}

pub(crate) fn codex_verified_o_source_allowed(
    channel: u64,
    source: &binding_events::SourceId,
) -> bool {
    verified_panes(channel).into_iter().all(|tmux| {
        tc::with_tmux_source_authority(&tmux, |authority| owned_source_allowed(authority, source))
    })
}

fn owned_source_allowed(
    authority: &TmuxSourceAuthority<'_>,
    source: &binding_events::SourceId,
) -> bool {
    let context = match disposition(authority) {
        SourcePolicyState::Legacy => return true,
        SourcePolicyState::VerifiedUnavailable => return false,
        SourcePolicyState::VerifiedCurrent(context) => context,
    };
    let Ok(fold) = binding_events::codex::read_ownership(&context) else {
        return false;
    };
    fold.verified
        .as_ref()
        .is_some_and(|proof| &proof.source == source)
        && codex_verified_source_allowed_under_source_authority(
            authority,
            &source.path.display().to_string(),
            Some(&source.session_id),
        )
}

fn verified_panes(channel: u64) -> Vec<String> {
    let mut panes = {
        let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state
            .channel_by_tmux
            .iter()
            .filter(|(_, entry)| entry.value == channel)
            .map(|(tmux, _)| tmux.clone())
            .collect::<Vec<_>>()
    };
    if channel == crate::services::codex_tui::canary::CANARY_CHANNEL {
        panes.push(crate::services::codex_tui::canary::CANARY_TMUX.to_owned());
    }
    panes
}
