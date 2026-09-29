//! Codex observations carry the verifier's identity, never a fresh pathname stat.

use super::*;
use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::codex_tui::session::codex_tui_rollout_paths_same;
use crate::services::codex_tui::session::source_observation::VerifiedCodexHookSource;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;

fn history(channel: u64, tmux: &str, nonce: &str) -> io::Result<Vec<BindingEvent>> {
    let mut events = binding_events_since(channel, 0)?;
    events.retain(|e| {
        e.provider == "codex"
            && e.tmux_session == tmux
            && e.execution_nonce.as_deref() == Some(nonce)
    });
    Ok(events)
}

fn current(events: &[BindingEvent]) -> Option<(&BindingEvent, &SourceId)> {
    events.iter().rev().find_map(|e| match &e.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => Some((e, source)),
        _ => None,
    })
}

/// Whether an event up to `seq` named the claim as its old, new, or pending source.
fn named_before(
    events: &[BindingEvent],
    seq: u64,
    claim: impl Fn(&str, Option<&Path>) -> bool,
) -> bool {
    events.iter().any(|e| {
        e.seq <= seq
            && (e
                .old
                .as_ref()
                .is_some_and(|old| claim(&old.session_id, Some(&old.path)))
                || match &e.new {
                    BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
                        claim(&source.session_id, Some(&source.path))
                    }
                    BindingTarget::Pending {
                        payload_session_id,
                        payload_transcript_path,
                    } => claim(
                        payload_session_id,
                        payload_transcript_path.as_deref().map(Path::new),
                    ),
                    BindingTarget::Rejected { .. } => false,
                })
    })
}

pub(crate) fn superseded(context: &BindingContext, session: &str) -> io::Result<bool> {
    let events = history(
        context.channel_id.unwrap_or_default(),
        &context.tmux_session,
        &context.execution_nonce,
    )?;
    let Some((latest, current)) = current(&events) else {
        return Ok(false);
    };
    if current.session_id == session {
        return Ok(false);
    }
    Ok(named_before(&events, latest.seq, |id, _| id == session))
}

/// The hook-recorded current source when the claimed source is one it already retired.
pub(crate) fn source_ahead(
    channel: u64,
    tmux: &str,
    nonce: &str,
    session: Option<&str>,
    path: &Path,
) -> io::Result<Option<SourceId>> {
    let events = history(channel, tmux, nonce)?;
    let Some((latest, current)) = current(&events) else {
        return Ok(None);
    };
    let claim = |id: &str, other: Option<&Path>| {
        session.is_some_and(|session| session == id)
            || other.is_some_and(|other| codex_tui_rollout_paths_same(other, path))
    };
    let retired = latest.evidence.hook_event.is_some()
        && !claim(&current.session_id, Some(&current.path))
        && named_before(&events, latest.seq, claim);
    Ok(retired.then(|| current.clone()))
}

/// Whether the recorded source's file is still the one the hook verified.
pub(crate) fn source_file_matches(source: &SourceId) -> bool {
    fs::metadata(&source.path).is_ok_and(|meta| file_identity(&meta) == (source.dev, source.ino))
}

pub(crate) fn record(
    context: &BindingContext,
    session: &str,
    hook: &HookSignal,
    verified: Option<&VerifiedCodexHookSource>,
) -> io::Result<bool> {
    let channel = context
        .channel_id
        .filter(|id| *id != 0)
        .ok_or_else(|| io::Error::other("Codex binding has no channel log"))?;
    if log_path(channel)?.is_none() {
        return Err(io::Error::other("Codex binding log unavailable"));
    }
    let source = verified
        .map(|v| {
            let (dev, ino) = match v.identity {
                #[cfg(unix)]
                SourceFileIdentity::Unix { dev, ino } => (dev, ino),
                SourceFileIdentity::Unavailable => {
                    return Err(io::Error::other("unverified identity"));
                }
            };
            Ok(SourceId {
                session_id: v.session_id.clone(),
                path: v.rollout_path.clone(),
                dev,
                ino,
            })
        })
        .transpose()?;
    commit_with(channel, |writer| {
        writer.plan_codex(context, session, hook, source)
    })
}

impl Writer {
    fn plan_codex(
        &self,
        context: &BindingContext,
        session: &str,
        hook: &HookSignal,
        source: Option<SourceId>,
    ) -> Option<BindingEvent> {
        let pane = self.panes.get(&context.tmux_session);
        let old = pane.and_then(|pane| pane.current.clone());
        let pending = pane.and_then(|pane| pane.pending.as_ref()).filter(|pending| {
            pending.execution_nonce.as_deref() == Some(&context.execution_nonce)
                && matches!(&pending.new, BindingTarget::Pending { payload_session_id, .. } if payload_session_id == session)
        });
        if source
            .as_ref()
            .is_some_and(|source| Some(source) == old.as_ref())
            || (source.is_none() && pending.is_some())
        {
            return None;
        }
        let new = match (source, pending) {
            (Some(source), Some(pending)) => BindingTarget::Resolved {
                pending_seq: pending.seq,
                source,
            },
            (Some(source), None) => BindingTarget::Source(source),
            (None, _) => BindingTarget::Pending {
                payload_session_id: session.to_owned(),
                payload_transcript_path: hook.transcript_path.clone(),
            },
        };
        Some(BindingEvent {
            seq: self.last_seq + 1,
            channel_id: context.channel_id.unwrap_or_default(),
            provider: "codex".to_owned(),
            tmux_session: context.tmux_session.clone(),
            execution_nonce: Some(context.execution_nonce.clone()),
            old,
            new,
            cause: pending.map_or_else(|| hook.cause(), |p| p.cause),
            parent_hint: pending.and_then(|p| p.parent_hint.clone()),
            evidence: BindingEvidence {
                hook_event: Some(hook.event.clone()),
                received_at: hook.received_at,
            },
            committed_at: Utc::now(),
        })
    }
}
