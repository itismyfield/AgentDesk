//! Volatile fresh successors are sealed only after their exact first durable binding.

use super::identity::SourceFailure as Failure;
use super::{attempt, capture, confirm};
use crate::services::tmux_common::{self as tc, TmuxSourceAuthority};
use crate::services::tui_o::store::codex_end::{
    self, Appended, CodexEnd, ContextSnapshot, SuccessorSeal,
};
use crate::services::tui_prompt_dedupe::{
    self as dedupe, binding_context as contexts, binding_events as events,
};
use contexts::{BindingContext, SpawnNonceMarker};
use events::{BindingEvent, BindingTarget, SourceId};

enum OldEnd {
    Stored(String),
    Replacement(Box<capture::CapturedOldExecution>),
    Confirmed(Box<CodexEnd>),
}

pub(super) struct Candidate {
    context: BindingContext,
    old: OldEnd,
    first: Option<BindingEvent>,
}

/// This token is inherited from the synchronous dispatch, never looked up by latest scope.
pub(crate) struct LaunchObservation {
    id: Option<attempt::AttemptId>,
    old: Option<OldEnd>,
    capture_failure: Option<(String, Option<u32>)>,
}

impl LaunchObservation {
    pub(crate) fn new(channel: Option<u64>, pane: &str) -> Self {
        let id = channel.and_then(|channel| attempt::current(channel, pane));
        let mut capture_failure = None;
        let old = id.as_ref().and_then(|id| {
            if let Some(end) = attempt::take_ticket(id) {
                return Some(OldEnd::Stored(end));
            }
            if crate::services::platform::tmux::session_presence(pane)
                != crate::services::platform::tmux::SessionPresence::Present
            {
                return None;
            }
            capture::capture(id.channel(), pane)
                .map(|capture| OldEnd::Replacement(Box::new(capture)))
                .inspect_err(|reason| {
                    capture_failure = Some((
                        reason.clone(),
                        crate::services::platform::tmux::pane_pid(pane),
                    ));
                })
                .ok()
        });
        Self {
            id,
            old,
            capture_failure,
        }
    }

    pub(crate) fn prepare(&mut self, context: &BindingContext) {
        let Some(id) = &self.id else {
            return;
        };
        if context.launch_mode == "fresh"
            && context.expected_native_session_id.is_none()
            && context.source_policy.as_deref() == Some("shadow")
            && let Some((reason, pid)) = self.capture_failure.take()
            && !super::mutant("capture_reason_removed")
        {
            tracing::warn!(channel = id.channel(), pane = id.pane(), ?pid,
                pgid = ?None::<u32>, attempt_state = ?id, reason,
                "Codex replacement End not captured");
        }
        let Some(old) = self.old.take() else {
            return;
        };
        if context.channel_id != Some(id.channel())
            || context.tmux_session != id.pane()
            || crate::config::runtime_root().as_deref() != Some(id.root())
            || context.launch_mode != "fresh"
            || context.expected_native_session_id.is_some()
            || context.source_policy.as_deref() != Some("shadow")
        {
            diagnostic(
                false,
                id.channel(),
                id.pane(),
                &old,
                "successor is not fresh shadow",
                "MaySpawn",
            );
            return;
        }
        let candidate = Candidate {
            context: context.clone(),
            old,
            first: None,
        };
        tc::with_tmux_source_authority(id.pane(), |authority| {
            let phase = attempt::phase_under(id, authority);
            if !attempt::put_candidate_under(id, authority, candidate) {
                tracing::debug!(channel = id.channel(), pane = id.pane(), pid = ?None::<u32>,
                    pgid = ?None::<u32>, attempt_state = ?phase, reason = "attempt superseded",
                    "Codex successor Seal not recorded");
            }
        });
    }

    /// Called only after the existing create and marker publication both succeeded.
    pub(crate) fn spawned(&self, context: &BindingContext) {
        let Some(id) = &self.id else {
            return;
        };
        tc::with_tmux_source_authority(id.pane(), |authority| {
            if attempt::bound_under(id, authority, &context.execution_nonce) {
                retry_bound_candidate(id.channel(), authority, &context.execution_nonce);
            } else {
                tracing::debug!(channel = id.channel(), pane = id.pane(), pid = ?None::<u32>,
                    pgid = ?None::<u32>, attempt_state = ?attempt::phase_under(id, authority),
                    reason = "spawn attempt superseded", "Codex successor Seal not recorded");
            }
        });
    }
}

/// A successful warm submit closes every spawn branch, without creating fresh evidence.
pub(crate) fn warm_bound(channel: Option<u64>, pane: &str) {
    let Some(id) = channel.and_then(|channel| attempt::current(channel, pane)) else {
        return;
    };
    tc::with_tmux_source_authority(pane, |authority| {
        if let SpawnNonceMarker::Known(nonce) = contexts::observe_spawn_nonce_marker(pane) {
            attempt::bound_under(&id, authority, &nonce);
        }
    });
}

/// The publisher already holds this authority; never reacquire the pane lock here.
pub(crate) fn binding_published_under(authority: &TmuxSourceAuthority<'_>) {
    if super::mutant("seal_callback_removed") {
        return;
    }
    let SpawnNonceMarker::Known(nonce) = contexts::observe_spawn_nonce_marker(authority.session())
    else {
        return;
    };
    let Ok(context) = contexts::execution_context("codex", &nonce) else {
        return;
    };
    if let Some(channel) = context.channel_id {
        retry_bound_candidate(channel, authority, &nonce);
    }
}

/// A2b may call this on idle polls. It only observes the current binding and writes End/Seal.
pub(crate) fn retry_bound_candidate(
    channel: u64,
    authority: &TmuxSourceAuthority<'_>,
    nonce: &str,
) {
    attempt::with_candidate_under(channel, authority, nonce, |mut candidate| {
        if candidate.first.is_some() && super::mutant("retry_removed") {
            return Some(candidate);
        }
        match candidate.seal(authority) {
            Ok(true) => None,
            Ok(false) => Some(candidate),
            Err(failure) => {
                let (pending, reason) = match failure {
                    Failure::Pending(reason) => (true, reason),
                    Failure::Rejected(reason) => (false, reason),
                };
                diagnostic(
                    !pending,
                    channel,
                    authority.session(),
                    &candidate.old,
                    &reason,
                    if pending {
                        "Bound:PendingEvidence"
                    } else {
                        "Bound:Rejected"
                    },
                );
                pending.then_some(candidate)
            }
        }
    });
}

impl Candidate {
    pub(super) fn discarded(&self, pane: &str, state: &str, reason: &str) {
        diagnostic(
            true,
            self.context.channel_id.unwrap_or_default(),
            pane,
            &self.old,
            reason,
            state,
        );
    }

    fn old_record(&self) -> Result<CodexEnd, Failure> {
        match &self.old {
            OldEnd::Replacement(capture) => Ok(capture.record.clone()),
            OldEnd::Confirmed(record) => Ok((**record).clone()),
            OldEnd::Stored(end_id) => {
                let root = crate::config::runtime_root().ok_or("runtime root unavailable")?;
                codex_end::read(&root, self.context.channel_id.ok_or("channel absent")?)?
                    .into_iter()
                    .find(|end| end.end_id == *end_id)
                    .ok_or("End evicted or missing".into())
            }
        }
    }

    fn seal(&mut self, authority: &TmuxSourceAuthority<'_>) -> Result<bool, Failure> {
        let context = &self.context;
        let channel = context.channel_id.ok_or("channel absent")?;
        let root = crate::config::runtime_root().ok_or("runtime root unavailable")?;
        let current = contexts::execution_context("codex", &context.execution_nonce)?;
        if ContextSnapshot::from(&current) != ContextSnapshot::from(context)
            || context.tmux_session != authority.session()
            || context.host.is_none()
            || context.host != contexts::stable_host_identity()
            || context.owner_runtime_root != tc::current_tmux_owner_marker()
            || contexts::observe_spawn_nonce_marker(authority.session())
                != SpawnNonceMarker::Known(context.execution_nonce.clone())
        {
            return Err("successor context/marker/host/owner changed".into());
        }
        let old = self.old_record()?;
        if old.channel != channel
            || old.tmux_session != context.tmux_session
            || old.execution_nonce == context.execution_nonce
            || context.host.as_deref() != Some(old.host.as_str())
            || old.owner_runtime_root != context.owner_runtime_root
        {
            return Err("successor scope differs from End".into());
        }
        let old_context = contexts::execution_context("codex", &old.execution_nonce)?;
        if ContextSnapshot::from(&old_context) != old.context
            || super::identity::verify_source_detail(&old_context, &old.source)?
                != old.provider_root
        {
            return Err("old context or canonical source root changed".into());
        }
        let rows = events::records_strict(channel)
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("binding log corrupt: {e:?}"))?;
        if rows.iter().any(|row| row.channel_id != channel)
            || rows.last().map_or(0, |row| row.seq) < old.binding_seq
        {
            return Err("binding channel/watermark mismatch".into());
        }
        let pane_rows: Vec<_> = rows
            .iter()
            .filter(|row| row.seq > old.binding_seq && row.tmux_session == context.tmux_session)
            .collect();
        let Some(first) = pane_rows.first().copied() else {
            diagnostic(
                false,
                channel,
                authority.session(),
                &self.old,
                "first durable binding pending",
                "Bound:PendingBinding",
            );
            return Ok(false);
        };
        if first.provider != "codex"
            || first.execution_nonce.as_deref() != Some(&context.execution_nonce)
            || first.old.as_ref() != Some(&old.source)
        {
            return Err("first successor binding does not follow the captured source".into());
        }
        if self.first.as_ref().is_some_and(|pinned| pinned != first) {
            return Err("first successor binding changed".into());
        }
        self.first = Some(first.clone());
        let (seq, source) =
            first_source(first, &pane_rows, &context.execution_nonce).map_err(Failure::Rejected)?;
        let Some(source) = source else {
            diagnostic(
                false,
                channel,
                authority.session(),
                &self.old,
                "first Pending binding unresolved",
                "Bound:PendingBinding",
            );
            return Ok(false);
        };
        if source == &old.source {
            return Err("fresh successor reuses old source".into());
        }
        if super::identity::verify_source_detail(context, source)? != old.provider_root {
            return Err("successor provider root changed".into());
        }
        let binding = dedupe::runtime_binding_for_tmux_session_under_source_authority(authority)
            .ok_or_else(|| Failure::Pending("runtime binding absent".into()))?;
        if binding.runtime_kind != crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
            || binding.session_id.as_deref() != Some(&source.session_id)
            || std::path::Path::new(&binding.output_path)
                .canonicalize()
                .ok()
                .as_ref()
                != Some(&source.path)
        {
            return Err("runtime binding differs from first successor".into());
        }
        if let OldEnd::Replacement(capture) = &self.old {
            let Some(record) = confirm::replacement(capture, &context.execution_nonce, first.seq)?
            else {
                return Err(Failure::Pending(
                    "replacement PID/PGID still present".into(),
                ));
            };
            self.old = OldEnd::Confirmed(Box::new(record));
        }
        if let OldEnd::Confirmed(record) = &self.old {
            match codex_end::append(&root, record)? {
                Appended::Durable => self.old = OldEnd::Stored(record.end_id.clone()),
                Appended::PublishedUnsynced(reason) => {
                    if !super::mutant("evicted_recreate") {
                        self.old = OldEnd::Stored(record.end_id.clone());
                    }
                    return Err(Failure::Pending(format!(
                        "replacement End unsynced: {reason}"
                    )));
                }
            }
        }
        let successor = SuccessorSeal {
            execution_nonce: context.execution_nonce.clone(),
            binding_seq: seq,
            source: source.clone(),
            context: ContextSnapshot::from(context),
        };
        match codex_end::seal(&root, channel, &old.end_id, &successor)? {
            Appended::Durable => {
                tracing::info!(
                    channel,
                    pane = authority.session(),
                    end_id = old.end_id,
                    successor = context.execution_nonce,
                    binding_seq = seq,
                    "Codex successor Seal durable"
                );
                Ok(true)
            }
            Appended::PublishedUnsynced(reason) => Err(Failure::Pending(format!(
                "successor Seal unsynced: {reason}"
            ))),
        }
    }
}

fn first_source<'a>(
    first: &'a BindingEvent,
    rows: &[&'a BindingEvent],
    nonce: &str,
) -> Result<(u64, Option<&'a SourceId>), String> {
    let same_execution = |row: &&BindingEvent| {
        row.provider == "codex" && row.execution_nonce.as_deref() == Some(nonce)
    };
    if rows.iter().any(|row| !same_execution(row)) {
        return Err("successor binding execution changed".into());
    }
    match &first.new {
        BindingTarget::Source(source) if rows.len() == 1 => Ok((first.seq, Some(source))),
        BindingTarget::Pending {
            payload_session_id,
            payload_transcript_path,
        } => {
            let Some(resolved) = rows.get(1) else {
                return Ok((first.seq, None));
            };
            match &resolved.new {
                BindingTarget::Resolved {
                    pending_seq,
                    source,
                } if *pending_seq == first.seq
                    && rows.len() == 2
                    && payload_session_id == &source.session_id
                    && payload_transcript_path
                        .as_deref()
                        .is_some_and(|path| std::path::Path::new(path) == source.path) =>
                {
                    Ok((resolved.seq, Some(source)))
                }
                _ => Err("first Pending was not exactly resolved".into()),
            }
        }
        _ => Err("first binding is not an exact fresh source".into()),
    }
}

fn diagnostic(terminal: bool, channel: u64, pane: &str, old: &OldEnd, reason: &str, state: &str) {
    if terminal && super::mutant("seal_reason_removed") {
        tracing::warn!("Codex successor Seal not recorded");
        return;
    }
    let (pid, pgid) = match old {
        OldEnd::Replacement(capture) => (Some(capture.pane_pid), Some(capture.pgid)),
        OldEnd::Stored(_) | OldEnd::Confirmed(_) => (None, None),
    };
    if terminal {
        tracing::warn!(
            channel,
            pane,
            ?pid,
            ?pgid,
            attempt_state = state,
            reason,
            "Codex successor Seal not recorded"
        );
    } else {
        tracing::debug!(
            channel,
            pane,
            ?pid,
            ?pgid,
            attempt_state = state,
            reason,
            "Codex successor Seal not recorded"
        );
    }
}
