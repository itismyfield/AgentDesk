//! Fresh input placement and scoped source restoration share identity, never consumption grants.

use super::*;
use crate::services::discord::codex_absence::InflightObs;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::{file_identity, same_file};
use crate::services::tui_prompt_dedupe::codex_verified::VerifiedSourceSnapshot;

#[cfg(test)]
static OWNERSHIP_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn fresh_scope_ownership_calls_for_tests() -> usize {
    OWNERSHIP_CALLS.swap(0, std::sync::atomic::Ordering::SeqCst)
}

#[derive(Debug)]
pub(crate) enum FreshReason {
    ExplicitClear(Captured),
    ExactAbsence(Captured),
}

impl FreshReason {
    fn captured(&self) -> &Captured {
        match self {
            Self::ExplicitClear(pin) | Self::ExactAbsence(pin) => pin,
        }
    }
}

fn input_target(channel: u64) -> Option<String> {
    if channel != CANARY_CHANNEL {
        return None;
    }
    let panes = super::super::verified_panes(channel);
    (panes.len() == 1 && panes[0] == CANARY_TMUX).then(|| panes[0].clone())
}

/// A cheap rejection only; full proof and identity checks stay under source authority.
pub(crate) fn fresh_scope_pending(channel: u64, last_nonce: Option<&str>) -> bool {
    let Some(tmux) = input_target(channel) else {
        return false;
    };
    matches!(absent_open::read_nonce(&tmux), SpawnNonceMarker::Known(nonce)
        if last_nonce != Some(nonce.as_str()))
}

/// Selection restoration observes its exact marker while the existing source authority is held.
pub(crate) fn with_current_scope_nonce<T>(
    channel: u64,
    inspect: impl FnOnce(Option<&str>) -> T,
) -> T {
    let Some(tmux) = input_target(channel) else {
        return inspect(None);
    };
    tc::with_tmux_source_authority(&tmux, |_| {
        let SpawnNonceMarker::Known(nonce) = absent_open::read_nonce(&tmux) else {
            return inspect(None);
        };
        let scoped = binding_context::execution_context("codex", &nonce)
            .is_ok_and(|context| absent_open::scoped_context(&context, &tmux));
        inspect(scoped.then_some(nonce.as_str()))
    })
}

pub(crate) fn input_reason_for_channel(channel: u64) -> Option<FreshReason> {
    let tmux = input_target(channel)?;
    let observation = crate::services::discord::codex_absence::readiness(channel, &tmux)?;
    if !crate::services::codex::successor_hooks_ready() {
        return None;
    }
    let reason =
        tc::with_tmux_source_authority(&tmux, |authority| input_reason(authority, &observation))?;
    (reason.captured().context.channel_id == Some(channel)).then_some(reason)
}

pub(crate) fn input_hold_reason_for_channel(channel: u64) -> &'static str {
    let Some(tmux) = input_target(channel) else {
        return "fresh_evidence_missing";
    };
    tc::with_tmux_source_authority(&tmux, |authority| {
        if absent_open::custody_pending_under(authority) {
            "absence_custody_pending"
        } else {
            "fresh_evidence_missing"
        }
    })
}

fn input_reason(
    authority: &TmuxSourceAuthority<'_>,
    observation: &InflightObs,
) -> Option<FreshReason> {
    #[cfg(test)]
    if crate::services::claude_tui::source_verify::n2b_mutant("b1b-pure") {
        let _ = open_under(authority);
    }
    if !observation.recheck(CANARY_CHANNEL, authority.session()) {
        return None;
    }
    let captured = absent_open::candidate_under(authority)?;
    if absent_open::read_nonce(authority.session()) == SpawnNonceMarker::Absent
        && read().ok().flatten().is_some_and(|mark| {
            let explicit = mark.origin == "explicit_clear";
            #[cfg(test)]
            let explicit =
                explicit || crate::services::claude_tui::source_verify::n2b_mutant("b1b-origin");
            explicit && captured.seq <= mark.retired_through_seq && captured.nonce == mark.nonce
        })
    {
        return Some(FreshReason::ExplicitClear(captured));
    }
    absent_open::capture_under(authority, observation).map(FreshReason::ExactAbsence)
}

pub(crate) fn successor_unproved_for_channel(channel: u64) -> bool {
    let Some(tmux) = input_target(channel) else {
        return false;
    };
    tc::with_tmux_source_authority(&tmux, |authority| {
        let SpawnNonceMarker::Known(nonce) = absent_open::read_nonce(&tmux) else {
            return false;
        };
        binding_context::execution_context("codex", &nonce).is_ok_and(|context| {
            absent_open::scoped_context(&context, &tmux)
                && successor_unproved_under(authority, &nonce, &context)
        })
    })
}

pub(crate) fn fresh_scope_eligible(channel: u64) -> bool {
    let Some(tmux) = input_target(channel) else {
        return false;
    };
    tc::with_tmux_source_authority(&tmux, |authority| {
        let context = match absent_open::read_nonce(&tmux) {
            SpawnNonceMarker::Known(nonce) => {
                binding_context::execution_context("codex", &nonce).ok()
            }
            SpawnNonceMarker::Absent => capture_under(authority)
                .ok()
                .flatten()
                .map(|pin| pin.context),
            SpawnNonceMarker::Unreadable => None,
        };
        context.is_some_and(|context| absent_open::scoped_context(&context, &tmux))
    })
}

fn root_source(context: &BindingContext) -> Option<(SourceId, Vec<SourceId>)> {
    let events = binding_events::binding_events_since(CANARY_CHANNEL, 0).ok()?;
    let root = events.iter().find(|event| {
        event.provider == "codex"
            && event.tmux_session == context.tmux_session
            && event.execution_nonce.as_deref() == Some(&context.execution_nonce)
            && matches!(
                event.new,
                binding_events::BindingTarget::Source(_)
                    | binding_events::BindingTarget::Pending { .. }
            )
    })?;
    if root.cause != binding_events::BindingCause::Startup
        || root.old.is_some()
        || root.parent_hint.is_some()
    {
        return None;
    }
    let proof_event = match &root.new {
        binding_events::BindingTarget::Source(_) => root,
        binding_events::BindingTarget::Pending { payload_session_id, .. } => events.iter().find(|event| {
            event.execution_nonce.as_deref() == Some(&context.execution_nonce)
                && matches!(&event.new, binding_events::BindingTarget::Resolved {pending_seq, source}
                    if *pending_seq == root.seq && source.session_id == *payload_session_id)
        })?,
        _ => return None,
    };
    let source = binding_events::codex::proof_at_seq(context, proof_event.seq)
        .ok()??
        .source;
    let prior = events
        .iter()
        .filter(|event| {
            event.seq < root.seq
                && event.provider == "codex"
                && event.execution_nonce.as_deref() != Some(&context.execution_nonce)
        })
        .filter_map(|event| match &event.new {
            binding_events::BindingTarget::Source(source)
            | binding_events::BindingTarget::Resolved { source, .. } => Some(source.clone()),
            _ => None,
        })
        .collect();
    Some((source, prior))
}

fn no_alias(denied: &[SourceId], current: &SourceId) -> bool {
    denied.iter().all(|old| {
        if old.path == current.path || old.ino == current.ino {
            return false;
        }
        match fs::metadata(&old.path) {
            Ok(meta) => file_identity(&meta) != (current.dev, current.ino),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        }
    })
}

/// Installs only selection data under the existing authority; resume keeps its normal gates.
pub(crate) fn with_fresh_scope(
    channel: u64,
    last_nonce: Option<&str>,
    sources: &[SourceId],
    quiescent: bool,
    install: impl FnOnce(String, Vec<SourceId>),
) -> Result<bool, &'static str> {
    let Some(tmux) = input_target(channel) else {
        return Ok(false);
    };
    tc::with_tmux_source_authority(&tmux, |authority| {
        let SpawnNonceMarker::Known(nonce) = absent_open::read_nonce(&tmux) else {
            return Ok(false);
        };
        if last_nonce == Some(nonce.as_str()) {
            return Ok(false);
        }
        let context = binding_context::execution_context("codex", &nonce)
            .map_err(|_| "fresh_evidence_missing")?;
        if !absent_open::scoped_context(&context, &tmux)
            || context.launch_mode != "fresh"
            || context.expected_native_session_id.is_some()
        {
            return Ok(false);
        }
        #[cfg(test)]
        OWNERSHIP_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if !binding_events::codex::latest_ownership(channel, &tmux)
            .is_ok_and(|pin| pin.is_some_and(|(_, current, _)| current == nonce))
        {
            return Err("successor_unproved");
        }
        let allowed =
            |source: &SourceId| super::super::owned_source_allowed(authority, source, same_file);
        let denied: Vec<_> = sources
            .iter()
            .filter(|source| !allowed(source))
            .cloned()
            .collect();
        #[cfg(test)]
        if crate::services::claude_tui::source_verify::n2b_mutant("b1b-proof") {
            install(nonce, denied);
            return Ok(true);
        }
        let fold = binding_events::codex::read_ownership(&context)
            .map_err(|_| "fresh_evidence_missing")?;
        let proof = fold
            .verified
            .filter(|_| !fold.conflicted && fold.pending.is_empty())
            .ok_or("successor_unproved")?;
        let (root, prior) = root_source(&context).ok_or("fresh_evidence_missing")?;
        // Reject aliases before descriptor verification can read a retained source's bytes.
        if !no_alias(&denied, &proof.source)
            || !no_alias(&denied, &root)
            || !no_alias(&prior, &proof.source)
            || !no_alias(&prior, &root)
        {
            return Err("source_alias");
        }
        let snapshot =
            VerifiedSourceSnapshot::capture(authority).map_err(|_| "fresh_evidence_missing")?;
        let binding =
            super::super::runtime_binding_for_tmux_session_under_source_authority(authority)
                .ok_or("fresh_evidence_missing")?;
        if snapshot.context() != &context
            || binding.runtime_kind != crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
            || !super::super::codex_verified_source_allowed_under_source_authority(
                authority,
                &binding.output_path,
                binding.session_id.as_deref(),
            )
            || !super::super::codex_verified::channel_allowed_under_source_authority(
                authority, channel,
            )
        {
            return Err("fresh_evidence_missing");
        }
        snapshot
            .recheck(authority)
            .map_err(|_| "fresh_evidence_missing")?;
        let changed = !denied.is_empty();
        if changed && !quiescent {
            return Err("new_root_waiting");
        }
        #[cfg(test)]
        if !changed && crate::services::claude_tui::source_verify::n2b_mutant("b1b-empty-nonce") {
            return Ok(false);
        }
        install(nonce, denied);
        Ok(changed)
    })
}
