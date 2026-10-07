//! Typed Codex claims are durable ownership evidence; delivery permission is separate.

use super::super::binding_context::{
    self, BindingContext, CapturedContext, HookBindingEnvelope, SpawnNonceMarker,
};
use super::*;
use crate::services::claude_tui::hook_server::{
    adoption_retry::{DurableKind, NotDurableReason},
    observation_ingress::{IngressOutcome, NotApplicableReason, ProceedReason, UnavailableReason},
};
use crate::services::codex_tui::session::{
    self,
    source_observation::{
        CodexHookSourceClaim, CodexRolloutSource, VerifiedCodexHookSource, first_prompt_matches,
        verify_codex_hook_source,
    },
};
use crate::services::tmux_common::{self as tc, TmuxSourceAuthority};
use binding_events::codex::{Claim, ClaimEvidence, Decision, Fold, VerifiedProof};
use std::{io, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeliveryPermission {
    Allowed,
    Cancelled,
    Unknown,
}

#[cfg(test)]
thread_local! {
    static BEFORE_COMMIT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
    static PERMISSIONS: std::cell::RefCell<std::collections::HashMap<String, DeliveryPermission>> = Default::default();
}
#[cfg(test)]
pub(super) fn set_permission_for_tests(context: &BindingContext, permission: DeliveryPermission) {
    PERMISSIONS.with(|values| {
        values
            .borrow_mut()
            .insert(context.execution_nonce.clone(), permission);
    });
}
#[cfg(test)]
pub(super) fn clear_permissions_for_tests() {
    PERMISSIONS.with(|values| values.borrow_mut().clear());
    BEFORE_COMMIT.with(|action| action.borrow_mut().take());
}

#[cfg(test)]
pub(super) fn before_commit_for_tests(action: impl FnOnce() + 'static) {
    BEFORE_COMMIT.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

fn permission(context: &BindingContext) -> DeliveryPermission {
    #[cfg(test)]
    if let Some(value) =
        PERMISSIONS.with(|values| values.borrow().get(&context.execution_nonce).copied())
    {
        return value;
    }
    // The cancellation owner must supply exact episode disposition before delivery can proceed.
    let _ = context;
    DeliveryPermission::Unknown
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn validate_context(
    authority: &TmuxSourceAuthority<'_>,
    context: &BindingContext,
) -> io::Result<()> {
    let current = binding_context::observe_spawn_nonce_marker(authority.session());
    let canonical = binding_context::execution_context("codex", &context.execution_nonce)
        .map_err(|_| invalid("Codex canonical context unavailable"))?;
    if current != SpawnNonceMarker::Known(context.execution_nonce.clone())
        || canonical != *context
        || context.tmux_session != authority.session()
        || context.owner_runtime_root != tc::current_tmux_owner_marker()
        || context.source_policy.as_deref() != Some("verified")
    {
        return Err(invalid("Codex execution changed"));
    }
    Ok(())
}

pub(super) fn current_context(
    authority: &TmuxSourceAuthority<'_>,
) -> io::Result<Option<BindingContext>> {
    let nonce = match binding_context::observe_spawn_nonce_marker(authority.session()) {
        SpawnNonceMarker::Known(nonce) => Some(nonce),
        _ => None,
    };
    let canonical = nonce
        .as_deref()
        .and_then(|nonce| binding_context::execution_context("codex", nonce).ok());
    let context = if canonical.is_some() {
        canonical
    } else {
        let channel = with_runtime_binding_state_under_source_authority(authority, |state| {
            state
                .channel_by_tmux
                .get(authority.session())
                .map(|entry| entry.value)
        });
        let recovered = channel
            .map(|channel| {
                binding_events::codex::context_for_nonce(
                    channel,
                    authority.session(),
                    nonce.as_deref(),
                )
            })
            .transpose()?
            .flatten();
        if recovered.is_none() {
            let marker =
                tc::session_temp_path(authority.session(), tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT);
            if std::fs::read(marker)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|marker| marker.get("codex_ownership").is_some())
            {
                return Err(invalid("Codex proof has no current canonical execution"));
            }
        }
        recovered
    };
    let Some(context) =
        context.filter(|context| context.source_policy.as_deref() == Some("verified"))
    else {
        return Ok(None);
    };
    validate_context(authority, &context)?;
    Ok(Some(context))
}

fn native(
    context: &BindingContext,
    claim: &Claim,
) -> Result<VerifiedCodexHookSource, session::source_observation::CodexHookSourceRejection> {
    verify_codex_hook_source(
        context.provider_root.as_deref().unwrap_or(Path::new("")),
        &CodexHookSourceClaim {
            session_id: &claim.session_id,
            transcript_path: claim.path.as_deref(),
            expected_source: CodexRolloutSource::Cli,
        },
    )
}

fn source_id(source: &VerifiedCodexHookSource) -> io::Result<binding_events::SourceId> {
    #[cfg(unix)]
    if let crate::services::cluster::stream_relay::SourceFileIdentity::Unix { dev, ino } =
        source.identity
    {
        return Ok(binding_events::SourceId {
            session_id: source.session_id.clone(),
            path: source.rollout_path.clone(),
            dev,
            ino,
        });
    }
    Err(invalid("Codex descriptor identity unavailable"))
}

fn claim_eligible(context: &BindingContext, claim: &Claim) -> bool {
    match &claim.evidence {
        ClaimEvidence::NativeHook {
            event,
            source,
            first_prompt_digest,
        } => {
            context.launch_mode == "fresh"
                && context.expected_native_session_id.is_none()
                && match event.as_str() {
                    "session_start" => source.as_deref() == Some("startup"),
                    "user_prompt_submit" => {
                        first_prompt_digest.is_some()
                            && first_prompt_digest == &context.first_prompt_digest
                    }
                    _ => false,
                }
        }
        ClaimEvidence::ExplicitResume {
            executed_session_id,
        } => {
            context.launch_mode == "resume"
                && context.expected_native_session_id.as_ref() == Some(executed_session_id)
                && executed_session_id == &claim.session_id
        }
    }
}

fn commit(
    authority: &TmuxSourceAuthority<'_>,
    context: &BindingContext,
    claim: &Claim,
) -> io::Result<Fold> {
    let previous = binding_events::codex::read_ownership(context)?.verified;
    let retry = matches!(&claim.evidence, ClaimEvidence::NativeHook { event, .. } if event == "user_prompt_submit")
        && previous
            .as_ref()
            .is_some_and(|proof| proof.source.session_id == claim.session_id);
    let mut verified = None;
    let decision = if !retry && !claim_eligible(context, claim) {
        Decision::Pending
    } else {
        match native(context, claim) {
            Ok(source)
                if context.launch_mode != "fresh"
                    || source
                        .created_at
                        .is_some_and(|time| time >= context.created_at) =>
            {
                let source_id = source_id(&source)?;
                verified = Some(source);
                Decision::Verified(source_id)
            }
            Ok(_) => Decision::Pending,
            Err(error) if error.may_resolve_later() => Decision::Pending,
            Err(error) => Decision::Rejected(format!("{error:?}")),
        }
    };
    #[cfg(test)]
    BEFORE_COMMIT.with(|action| {
        if let Some(action) = action.borrow_mut().take() {
            action();
        }
    });
    // A path hint cannot split a retry of the same opened native descriptor.
    let persisted_claim = match (&previous, &decision) {
        (Some(proof), Decision::Verified(source)) if &proof.source == source => {
            &proof.ownership.claim
        }
        _ => claim,
    };
    binding_events::codex::commit_claim(context, persisted_claim, decision, || {
        validate_context(authority, context)?;
        if let Some(before) = &verified {
            let after = native(context, claim)
                .map_err(|_| invalid("Codex descriptor changed before commit"))?;
            if after != *before {
                return Err(invalid("Codex descriptor changed before commit"));
            }
        }
        Ok(())
    })
}

pub(super) fn proof_for_binding(
    authority: &TmuxSourceAuthority<'_>,
    context: &BindingContext,
    binding: &TuiRuntimeBinding,
) -> io::Result<VerifiedProof> {
    validate_context(authority, context)?;
    let fold = binding_events::codex::read_ownership(context)?;
    let proof = fold
        .verified
        .filter(|_| !fold.conflicted && fold.pending.is_empty())
        .ok_or_else(|| invalid("Codex ownership Pending or conflicted"))?;
    if binding.runtime_kind != RuntimeHandoffKind::CodexTui
        || binding.session_id.as_deref() != Some(&proof.source.session_id)
        || Path::new(&binding.output_path).canonicalize().ok().as_ref() != Some(&proof.source.path)
    {
        return Err(invalid("Codex binding does not name its proof"));
    }
    let source = native(context, &proof.ownership.claim)
        .map_err(|_| invalid("Codex proof descriptor unavailable"))?;
    if source_id(&source)? != proof.source {
        return Err(invalid("Codex proof descriptor changed"));
    }
    Ok(proof)
}

pub(super) fn publication_allowed(
    authority: &TmuxSourceAuthority<'_>,
    binding: &TuiRuntimeBinding,
) -> bool {
    if binding.runtime_kind != RuntimeHandoffKind::CodexTui {
        return true;
    }
    match current_context(authority) {
        Ok(None) => true,
        Ok(Some(context)) => {
            proof_for_binding(authority, &context, binding).is_ok()
                && permission(&context) == DeliveryPermission::Allowed
        }
        Err(_) => false,
    }
}

pub(super) fn marker_proof(context: &BindingContext, proof: &VerifiedProof) -> serde_json::Value {
    serde_json::json!({"execution_nonce": context.execution_nonce, "proof_seq": proof.seq,
        "dev": proof.source.dev, "ino": proof.source.ino})
}

pub(super) fn marker_metadata(
    authority: &TmuxSourceAuthority<'_>,
    path: &Path,
    id: Option<&str>,
) -> Result<Option<serde_json::Value>, String> {
    let Some(context) = current_context(authority).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let binding = TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::CodexTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: id.map(str::to_owned),
        last_offset: 0,
        relay_last_offset: Some(0),
    };
    let proof = proof_for_binding(authority, &context, &binding).map_err(|e| e.to_string())?;
    if permission(&context) != DeliveryPermission::Allowed {
        return Err("Codex delivery permission held".into());
    }
    Ok(Some(marker_proof(&context, &proof)))
}

pub(super) fn consumer_allowed(
    authority: &TmuxSourceAuthority<'_>,
    binding: &TuiRuntimeBinding,
) -> bool {
    if binding.runtime_kind != RuntimeHandoffKind::CodexTui {
        return true;
    }
    let context = match current_context(authority) {
        Ok(None) => return true,
        Ok(Some(context)) => context,
        Err(_) => return false,
    };
    let Ok(proof) = proof_for_binding(authority, &context, binding) else {
        return false;
    };
    if permission(&context) != DeliveryPermission::Allowed {
        return false;
    }
    let path = tc::session_temp_path(authority.session(), tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT);
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|marker| {
            marker["codex_ownership"] == marker_proof(&context, &proof)
                && marker["rollout_path"].as_str() == proof.source.path.to_str()
                && marker["session_id"].as_str() == Some(&proof.source.session_id)
        })
}

pub(super) fn publish_proof(
    authority: &TmuxSourceAuthority<'_>,
    context: &BindingContext,
    fold: &Fold,
) -> io::Result<bool> {
    let Some(proof) = fold
        .verified
        .as_ref()
        .filter(|_| !fold.conflicted && fold.pending.is_empty())
    else {
        return Ok(false);
    };
    if permission(context) != DeliveryPermission::Allowed {
        return Ok(false);
    }
    let binding = TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::CodexTui,
        output_path: proof.source.path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(proof.source.session_id.clone()),
        last_offset: 0,
        relay_last_offset: Some(0),
    };
    proof_for_binding(authority, context, &binding)?;
    let binding = super::codex_cursor::restore(authority, binding)?;
    session::write_codex_tui_rollout_marker_under_source_authority(
        authority,
        &proof.source.path,
        Some(&proof.source.session_id),
        Some(0),
    )
    .map_err(io::Error::other)?;
    super::codex_cursor::persist(authority, &binding)?;
    with_runtime_binding_state_under_source_authority(authority, |state| {
        state.runtime_by_tmux.insert(
            authority.session().to_owned(),
            TimedValue {
                value: binding,
                recorded_at: Instant::now(),
            },
        );
        state.tmux_by_provider_session.insert(
            PromptKey::new("codex", &proof.source.session_id),
            TimedValue {
                value: authority.session().to_owned(),
                recorded_at: Instant::now(),
            },
        );
    });
    Ok(true)
}

pub(crate) fn observe_verified_codex_hook(
    payload_session: Option<&str>,
    payload: &serde_json::Value,
    hook: &HookSignal,
    envelope: Option<&HookBindingEnvelope>,
) -> Option<IngressOutcome> {
    let CapturedContext::Captured(context) = &envelope?.context else {
        return None;
    };
    if context.source_policy.as_deref() != Some("verified") {
        return None;
    }
    if !matches!(hook.event.as_str(), "session_start" | "user_prompt_submit") {
        return Some(IngressOutcome::Proceed(ProceedReason::NoSessionSwitch));
    }
    Some(tc::with_tmux_source_authority(
        &context.tmux_session,
        |authority| {
            if validate_context(authority, context).is_err() {
                return IngressOutcome::Unavailable(UnavailableReason::RestoreNotReady);
            }
            let channel = with_runtime_binding_state_under_source_authority(authority, |state| {
                state
                    .channel_by_tmux
                    .get(authority.session())
                    .map(|entry| entry.value)
            });
            if channel != context.channel_id || channel.is_none() {
                return IngressOutcome::Unavailable(UnavailableReason::ChannelNotRestored);
            }
            let Some(id) = payload_session.filter(|id| uuid::Uuid::parse_str(id).is_ok()) else {
                return IngressOutcome::NotApplicable(NotApplicableReason::PayloadNotUuid);
            };
            use sha2::{Digest, Sha256};
            let claim = Claim {
                session_id: id.to_owned(),
                path: hook.transcript_path.as_ref().map(std::path::PathBuf::from),
                evidence: ClaimEvidence::NativeHook {
                    event: hook.event.clone(),
                    source: payload["source"].as_str().map(str::to_owned),
                    first_prompt_digest: payload["prompt"]
                        .as_str()
                        .map(|prompt| format!("sha256:{:x}", Sha256::digest(prompt.as_bytes()))),
                },
            };
            let qualified = hook.event != "user_prompt_submit"
                || (context.launch_mode == "fresh"
                    && context.expected_native_session_id.is_none()
                    && first_prompt_matches(
                        context.first_prompt_digest.as_deref(),
                        &payload["prompt"],
                    ));
            let retry = binding_events::codex::read_ownership(context).is_ok_and(|fold| {
                fold.verified
                    .is_some_and(|proof| proof.source.session_id == id)
            });
            let result = if qualified || retry {
                commit(authority, context, &claim)
            } else {
                binding_events::codex::commit_claim(context, &claim, Decision::Pending, || {
                    validate_context(authority, context)
                })
            };
            match result {
                Err(_) => IngressOutcome::NotDurable(NotDurableReason::Append),
                Ok(fold) => match publish_proof(authority, context, &fold) {
                    Err(_) => IngressOutcome::NotDurable(NotDurableReason::Append),
                    Ok(true) => IngressOutcome::Durable(DurableKind::Adopted),
                    Ok(false) => IngressOutcome::Durable(DurableKind::Pending),
                },
            }
        },
    ))
}

pub(crate) fn resolve_under_authority(authority: &TmuxSourceAuthority<'_>) -> IngressOutcome {
    let context = match current_context(authority) {
        Ok(Some(context)) => context,
        Ok(None) => return IngressOutcome::NotApplicable(NotApplicableReason::OtherProvider),
        Err(_) => return IngressOutcome::Unavailable(UnavailableReason::HistoryUnreadable),
    };
    let result = (|| -> io::Result<bool> {
        let mut fold = binding_events::codex::read_ownership(&context)?;
        for _ in 0..=fold.pending.len() {
            let before = fold.clone();
            for pending in before.pending.iter() {
                fold = commit(authority, &context, &pending.claim)?;
            }
            if fold == before {
                break;
            }
        }
        publish_proof(authority, &context, &fold)
    })();
    match result {
        Ok(true) => IngressOutcome::Durable(DurableKind::Adopted),
        Ok(false) => IngressOutcome::Durable(DurableKind::Pending),
        Err(_) => IngressOutcome::NotDurable(NotDurableReason::Append),
    }
}

pub(crate) fn resolve_registered_claims() {
    let sessions = {
        let state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        state.channel_by_tmux.keys().cloned().collect::<Vec<_>>()
    };
    for tmux in sessions {
        tc::with_tmux_source_authority(&tmux, |authority| {
            let _ = resolve_under_authority(authority);
        });
    }
}

pub(crate) fn codex_verified_discovered_channel(tmux: &str) -> Option<u64> {
    tc::with_tmux_source_authority(tmux, |authority| {
        current_context(authority).ok().flatten()?.channel_id
    })
}

pub(crate) fn recover_discovered_codex_binding(tmux: &str, channel: u64) -> bool {
    tc::with_tmux_source_authority(tmux, |authority| {
        recover_discovered_under_authority(authority, channel)
    })
}

fn recover_discovered_under_authority(authority: &TmuxSourceAuthority<'_>, channel: u64) -> bool {
    let context = match current_context(authority) {
        Ok(None) => {
            let nonce = match binding_context::observe_spawn_nonce_marker(authority.session()) {
                SpawnNonceMarker::Known(nonce) => Some(nonce),
                _ => None,
            };
            return binding_events::codex::context_for_nonce(
                channel,
                authority.session(),
                nonce.as_deref(),
            )
            .is_ok_and(|context| context.is_none());
        }
        Ok(Some(context)) if context.channel_id == Some(channel) && channel != 0 => context,
        _ => return false,
    };
    let registered = with_runtime_binding_state_under_source_authority(authority, |state| {
        if state
            .channel_by_tmux
            .get(authority.session())
            .is_some_and(|entry| entry.value != channel)
        {
            return false;
        }
        state.channel_by_tmux.insert(
            authority.session().to_owned(),
            TimedValue {
                value: channel,
                recorded_at: Instant::now(),
            },
        );
        true
    });
    registered
        && validate_context(authority, &context).is_ok()
        && matches!(
            resolve_under_authority(authority),
            IngressOutcome::Durable(DurableKind::Adopted)
        )
}
