//! An exact resumed execution's start proof in the existing binding log.

use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{self, BindingContext};

fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}

pub(crate) fn context_identity(context: &BindingContext) -> io::Result<&SourceId> {
    let resume = context
        .claude_resume
        .as_ref()
        .ok_or_else(|| invalid("no exact resume source"))?;
    let source = &resume.source;
    let immutable = binding_context::execution_context("claude", &context.execution_nonce)
        .map_err(|_| invalid("resume context unreadable"))?;
    if immutable != *context
        || context.schema != 1
        || context.provider != "claude"
        || context.launch_mode != "resume"
        || context.owner_runtime_root != crate::services::tmux_common::current_tmux_owner_marker()
        || context.channel_id.is_none_or(|channel| channel == 0)
        || context.tmux_session.is_empty()
        || context.expected_native_session_id.as_deref() != Some(source.session_id.as_str())
        || uuid::Uuid::parse_str(&context.execution_nonce).ok()
            == uuid::Uuid::parse_str(&resume.previous_nonce).ok()
        || resume.previous_nonce.len() != 32
        || !resume
            .previous_nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || !matches!(observe_spawn_nonce_marker(&context.tmux_session), SpawnNonceMarker::Known(nonce) if nonce == context.execution_nonce)
    {
        return Err(invalid("resume context/source identity mismatch"));
    }
    if context.provider_root.is_none() {
        return Err(invalid("resume provider root missing"));
    }
    Ok(source)
}

pub(crate) fn context_source(context: &BindingContext) -> io::Result<&SourceId> {
    let source = context_identity(context)?;
    if !codex::source_file_matches(source) {
        return Err(invalid("resume source identity changed"));
    }
    let root = context
        .provider_root
        .as_ref()
        .ok_or_else(|| invalid("resume provider root missing"))?;
    let root = root.canonicalize()?;
    let canonical = source.path.canonicalize()?;
    if !canonical.starts_with(&root) || canonical == root {
        return Err(invalid("resume transcript escaped provider root"));
    }
    Ok(source)
}

fn snapshot(context: &BindingContext) -> io::Result<Vec<StrictBindingRecord>> {
    records_strict_verified(
        context
            .channel_id
            .ok_or_else(|| invalid("no resume channel"))?,
    )?
    .map_err(|_| invalid("resume history is incomplete"))
}

fn own<'a>(
    context: &BindingContext,
    records: &'a [StrictBindingRecord],
) -> Vec<&'a StrictBindingRecord> {
    records
        .iter()
        .filter(|record| {
            record.event.provider == "claude"
                && record.event.channel_id == context.channel_id.unwrap_or(0)
                && record.event.tmux_session == context.tmux_session
        })
        .collect()
}

fn start_is(context: &BindingContext, record: &StrictBindingRecord) -> bool {
    record.verified
        && record.event.execution_nonce.as_deref() == Some(context.execution_nonce.as_str())
        && record.event.cause == BindingCause::Resume
        && record.event.evidence.hook_event.as_deref() == Some("session_start")
        && matches!(&record.event.new, BindingTarget::Source(source)
            if Some(source) == context.claude_resume.as_ref().map(|resume| &resume.source))
}

fn durable_snapshot(
    context: &BindingContext,
    records: &[StrictBindingRecord],
    exact: bool,
) -> io::Result<()> {
    let sequence = records.last().map_or(0, |record| record.event.seq);
    let mut healthy = false;
    commit_with(
        context
            .channel_id
            .ok_or_else(|| invalid("no resume channel"))?,
        |writer| {
            healthy = !writer.poisoned
                && !writer.tainted
                && writer.last_seq == sequence
                && writer.panes.get(&context.tmux_session).is_some_and(|pane| {
                    pane.nonce.as_deref() == Some(context.execution_nonce.as_str())
                        && (!exact
                            || (pane.verified
                                && pane.pending.is_none()
                                && pane.current.as_ref()
                                    == context.claude_resume.as_ref().map(|resume| &resume.source)))
                });
            Planned::Keep(Committed::Unchanged)
        },
    )?;
    if healthy {
        Ok(())
    } else {
        Err(invalid("resume history changed or is not durable"))
    }
}

pub(crate) fn record_start(context: &BindingContext, hook: &HookSignal) -> io::Result<Committed> {
    let source = context_source(context)?;
    if hook.event != "session_start"
        || hook.source.as_deref() != Some("resume")
        || hook.transcript_path.as_deref().map(Path::new) != Some(source.path.as_path())
    {
        return Err(invalid("resume start hook mismatches its source"));
    }
    let records = snapshot(context)?;
    let own = own(context, &records);
    let latest = own
        .iter()
        .rev()
        .copied()
        .find(|record| !matches!(record.event.new, BindingTarget::Rejected { .. }))
        .ok_or_else(|| invalid("resume has no old source baseline"))?;
    let repeated = start_is(context, latest);
    let resume = context
        .claude_resume
        .as_ref()
        .ok_or_else(|| invalid("resume context missing"))?;
    let first_new = own.iter().position(|record| {
        record.event.execution_nonce.as_deref() == Some(context.execution_nonce.as_str())
    });
    if first_new.is_some_and(|first| {
        !repeated
            || !start_is(context, own[first])
            || own[first..].iter().any(|record| {
                record.event.execution_nonce.as_deref() != Some(context.execution_nonce.as_str())
            })
    }) {
        return Err(invalid("resume nonce history collides"));
    }
    if !repeated
        && !(latest.verified
            && latest.event.execution_nonce.as_deref() == Some(resume.previous_nonce.as_str())
            && matches!(&latest.event.new, BindingTarget::Source(pin) | BindingTarget::Resolved { source: pin, .. } if pin == source))
    {
        return Err(invalid("resume old nonce baseline changed"));
    }
    let sequence = records.last().map_or(0, |record| record.event.seq);
    let mut healthy = false;
    let committed = commit_with(
        context
            .channel_id
            .ok_or_else(|| invalid("no resume channel"))?,
        |writer| {
            let pane = writer.panes.get(&context.tmux_session);
            healthy = !writer.poisoned
                && !writer.tainted
                && writer.last_seq == sequence
                && pane.is_some_and(|pane| {
                    pane.verified
                        && pane.current.as_ref() == Some(source)
                        && pane.pending.is_none()
                        && pane.nonce.as_deref() == latest.event.execution_nonce.as_deref()
                });
            if !healthy {
                return Planned::Keep(Committed::Recheck);
            }
            if repeated {
                return Planned::Keep(Committed::Unchanged);
            }
            Planned::Append(
                BindingEvent {
                    seq: writer.last_seq + 1,
                    channel_id: context.channel_id.unwrap_or(0),
                    provider: "claude".into(),
                    tmux_session: context.tmux_session.clone(),
                    execution_nonce: Some(context.execution_nonce.clone()),
                    old: Some(source.clone()),
                    new: BindingTarget::Source(source.clone()),
                    cause: BindingCause::Resume,
                    parent_hint: None,
                    evidence: BindingEvidence {
                        hook_event: Some("session_start".into()),
                        received_at: hook.received_at,
                    },
                    committed_at: Utc::now(),
                },
                true,
                hook.published_at,
            )
        },
    )?;
    if healthy {
        Ok(committed)
    } else {
        Err(invalid("resume log or pin changed before append"))
    }
}

pub(crate) fn start_recorded(context: &BindingContext) -> io::Result<bool> {
    context_identity(context)?;
    let records = snapshot(context)?;
    durable_snapshot(context, &records, false)?;
    let own = own(context, &records);
    let Some(start) = own.iter().position(|record| {
        record.event.execution_nonce.as_deref() == Some(context.execution_nonce.as_str())
    }) else {
        return Ok(false);
    };
    Ok(start_is(context, own[start])
        && own[start..].iter().all(|record| {
            record.event.execution_nonce.as_deref() == Some(context.execution_nonce.as_str())
        }))
}

pub(crate) fn started_source(context: &BindingContext) -> io::Result<Option<SourceId>> {
    let source = context_source(context)?;
    let records = snapshot(context)?;
    durable_snapshot(context, &records, true)?;
    let own = own(context, &records);
    if !own
        .iter()
        .rev()
        .copied()
        .find(|record| !matches!(record.event.new, BindingTarget::Rejected { .. }))
        .is_some_and(|record| start_is(context, record))
    {
        return Ok(None);
    }
    let Some(first) = own.iter().position(|record| {
        record.event.execution_nonce.as_deref() == Some(context.execution_nonce.as_str())
    }) else {
        return Ok(None);
    };
    if !start_is(context, own[first])
        || own[first..].iter().any(|record| {
            record.event.execution_nonce.as_deref() != Some(context.execution_nonce.as_str())
        })
    {
        return Err(invalid("resume nonce history collides"));
    }
    let history = strict_claude_history(
        records.iter(),
        &context.tmux_session,
        &context.execution_nonce,
    );
    Ok((history.complete
        && history.awaiting.is_none()
        && history
            .current
            .as_ref()
            .and_then(|current| current.pin.as_ref())
            == Some(source))
    .then(|| source.clone()))
}

pub(crate) fn matches_start(
    context: &BindingContext,
    event: &crate::services::claude_tui::hook_server::HookEvent,
) -> bool {
    event.provider == "claude"
        && event.kind == crate::services::claude_tui::hook_server::HookEventKind::SessionStart
        && event.original_context.as_ref() == Some(context)
        && Some(event.session_id.as_str()) == context.expected_native_session_id.as_deref()
        && event
            .payload
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            == context.expected_native_session_id.as_deref()
        && event.payload["source"] == "resume"
        && event
            .payload
            .get("transcript_path")
            .and_then(serde_json::Value::as_str)
            .map(Path::new)
            == context
                .claude_resume
                .as_ref()
                .map(|resume| resume.source.path.as_path())
        && matches!(started_source(context), Ok(Some(source)) if Some(&source) == context.claude_resume.as_ref().map(|resume| &resume.source))
}
