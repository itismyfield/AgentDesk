//! Codex observations carry the verifier's identity, never a fresh pathname stat.

use super::*;
use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::codex_tui::session::source_observation::VerifiedCodexHookSource;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;

pub(crate) fn superseded(context: &BindingContext, session: &str) -> io::Result<bool> {
    let events = binding_events_since(context.channel_id.unwrap_or_default(), 0)?;
    let events: Vec<_> = events
        .iter()
        .filter(|e| {
            e.provider == "codex"
                && e.tmux_session == context.tmux_session
                && e.execution_nonce.as_deref() == Some(context.execution_nonce.as_str())
        })
        .collect();
    let latest = events.iter().rev().find_map(|e| match &e.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
            Some((e.seq, source))
        }
        _ => None,
    });
    let Some((seq, current)) = latest else {
        return Ok(false);
    };
    if current.session_id == session {
        return Ok(false);
    }
    Ok(events.iter().any(|e| {
        e.seq <= seq
            && (e.old.as_ref().is_some_and(|old| old.session_id == session)
                || match &e.new {
                    BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
                        source.session_id == session
                    }
                    BindingTarget::Pending {
                        payload_session_id, ..
                    } => payload_session_id == session,
                    BindingTarget::Rejected { .. } => false,
                })
    }))
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
