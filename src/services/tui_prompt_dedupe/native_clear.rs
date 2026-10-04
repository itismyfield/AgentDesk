//! Dormant native-clear orchestration. Canonical binding records alone decide commitment.

use super::{
    binding_context::{BindingContext, SpawnNonceMarker, observe_spawn_nonce_marker},
    binding_events::{self, BindingCause, BindingEvent, BindingTarget, SourceId},
};
use crate::services::{
    claude_tui::host_input::NativeClearSubmission,
    tmux_common::{self, TmuxSourceAuthority},
};
use std::{future::Future, io, pin::Pin, time::Duration};
use tokio::{
    sync::{OwnedMutexGuard, oneshot, watch},
    time::Instant,
};

pub(crate) const NATIVE_CLEAR_BUDGET: Duration = Duration::from_secs(20);
const FINISH_RESERVE: Duration = Duration::from_secs(2);
type Step<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClearAdmission {
    Native,
    Fallback,
}

#[derive(Clone, Debug)]
pub(crate) struct ClearTicket {
    pub context: BindingContext,
    pub old: SourceId,
    pub baseline: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClearCommit {
    pub seq: u64,
    pub session: String,
    pub source: Option<SourceId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClearDecision {
    Committed(ClearCommit),
    Fallback,
    Hold,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClearOutcome {
    Native(ClearCommit),
    Fallback,
    // The caller must keep subsequent admission closed; no reset is authorized.
    Hold(Option<ClearCommit>),
}

#[allow(dead_code)]
pub(crate) struct CanonicalClearWaiter {
    pub ticket: ClearTicket,
    pub changed: watch::Receiver<u64>,
}

#[allow(dead_code)]
impl CanonicalClearWaiter {
    // Subscribe before reading the baseline so a fast hook cannot be lost.
    pub(crate) fn capture(context: BindingContext, old: SourceId) -> io::Result<Self> {
        let channel = context
            .channel_id
            .filter(|id| *id != 0)
            .ok_or_else(|| io::Error::other("clear channel missing"))?;
        if context.provider != "claude" || context.execution_nonce.is_empty() {
            return Err(io::Error::other("unsupported clear execution"));
        }
        let changed = binding_events::subscribe_binding_events(channel)?;
        let baseline = binding_events::records_strict(channel)?
            .map_err(|_| io::Error::other("clear log corrupt"))?
            .last()
            .map_or(0, |event| event.seq);
        let ticket = ClearTicket {
            context,
            old,
            baseline,
        };
        let valid =
            tmux_common::try_with_tmux_source_authority(&ticket.context.tmux_session, |_| {
                current_execution(&ticket)
                    && binding_events::pinned_source(channel, &ticket.context.tmux_session)
                        .is_ok_and(|pin| pin.as_ref() == Some(&ticket.old))
            })
            .unwrap_or(false);
        if !valid {
            return Err(io::Error::other("clear source changed or busy"));
        }
        Ok(Self { ticket, changed })
    }

    // Commitment and the caller's existing fallback cut share the source authority.
    pub(crate) fn decide<R>(
        &self,
        fallback: impl FnOnce(&TmuxSourceAuthority<'_>) -> R,
    ) -> (ClearDecision, Option<R>) {
        tmux_common::try_with_tmux_source_authority(
            &self.ticket.context.tmux_session,
            |authority| {
                let records =
                    binding_events::records_strict(self.ticket.context.channel_id.unwrap_or(0));
                let Ok(Ok(records)) = records else {
                    return (ClearDecision::Hold, None);
                };
                if let Some(commit) = clear_commit(&self.ticket, &records) {
                    return (ClearDecision::Committed(commit), None);
                }
                if !current_execution(&self.ticket) {
                    return (ClearDecision::Hold, None);
                }
                (ClearDecision::Fallback, Some(fallback(authority)))
            },
        )
        .unwrap_or((ClearDecision::Hold, None))
    }
}

fn current_execution(ticket: &ClearTicket) -> bool {
    matches!(observe_spawn_nonce_marker(&ticket.context.tmux_session), SpawnNonceMarker::Known(n) if n == ticket.context.execution_nonce)
}

fn clear_commit(ticket: &ClearTicket, records: &[BindingEvent]) -> Option<ClearCommit> {
    let mut committed = None;
    for event in records.iter().filter(|e| e.seq > ticket.baseline) {
        if event.channel_id != ticket.context.channel_id.unwrap_or(0)
            || event.provider != ticket.context.provider
            || event.tmux_session != ticket.context.tmux_session
            || event.execution_nonce.as_deref() != Some(ticket.context.execution_nonce.as_str())
        {
            continue;
        }
        match &event.new {
            BindingTarget::Source(_) | BindingTarget::Pending { .. }
                if event.cause == BindingCause::Clear
                    && event.evidence.hook_event.as_deref() == Some("session_start")
                    && event.old.as_ref() == Some(&ticket.old) =>
            {
                let (session, source) = match &event.new {
                    BindingTarget::Source(source) => {
                        (source.session_id.clone(), Some(source.clone()))
                    }
                    BindingTarget::Pending {
                        payload_session_id, ..
                    } => (payload_session_id.clone(), None),
                    _ => unreachable!(),
                };
                if !session.trim().is_empty() && session != ticket.old.session_id {
                    committed = Some(ClearCommit {
                        seq: event.seq,
                        session,
                        source,
                    });
                }
            }
            BindingTarget::Resolved {
                pending_seq,
                source,
            } if committed
                .as_ref()
                .is_some_and(|c| c.seq == *pending_seq && c.session == source.session_id) =>
            {
                committed.as_mut()?.source = Some(source.clone());
            }
            _ => {}
        }
    }
    committed
}

// Implementors use existing selector/reset owners; these callbacks create no authority.
#[allow(dead_code)]
pub(crate) trait NativeClearHost: Send + 'static {
    fn changes(&self) -> watch::Receiver<u64>;
    fn prepare(&mut self, deadline: Instant) -> Step<'_, bool>;
    fn submit(&mut self, deadline: Instant) -> NativeClearSubmission;
    fn decide(&mut self, allow_fallback: bool) -> ClearDecision;
    fn composer_empty(&mut self, deadline: Instant) -> bool;
    fn save(&mut self, commit: ClearCommit, deadline: Instant) -> Step<'_, bool>;
    fn fallback(&mut self, deadline: Instant) -> Step<'_, bool>;
}

// Dropping the receiver never cancels the worker or releases its transition guard early.
#[allow(dead_code)]
pub(crate) fn start_native_clear<H: NativeClearHost>(
    host: H,
    admission: ClearAdmission,
    guard: OwnedMutexGuard<()>,
) -> oneshot::Receiver<ClearOutcome> {
    let (send, recv) = oneshot::channel();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let result = runtime.block_on(run(host, admission));
        let _ = send.send(result);
    });
    recv
}

async fn run<H: NativeClearHost>(mut host: H, admission: ClearAdmission) -> ClearOutcome {
    let end = Instant::now() + NATIVE_CLEAR_BUDGET;
    let native_end = end - FINISH_RESERVE;
    let mut changed = host.changes();
    let ready = admission == ClearAdmission::Native
        && tokio::time::timeout_at(native_end, host.prepare(native_end))
            .await
            .unwrap_or(false);
    if ready {
        let submitted = host.submit(native_end);
        if submitted != NativeClearSubmission::NotSent {
            loop {
                if matches!(host.decide(false), ClearDecision::Committed(_)) {
                    break;
                }
                if tokio::time::timeout_at(native_end, changed.changed())
                    .await
                    .is_err()
                {
                    break;
                }
                if changed.has_changed().is_err() {
                    break;
                }
            }
        }
    }
    match host.decide(true) {
        ClearDecision::Committed(commit) => {
            if !host.composer_empty(end) {
                return ClearOutcome::Hold(Some(commit));
            }
            if tokio::time::timeout_at(end, host.save(commit.clone(), end))
                .await
                .unwrap_or(false)
            {
                ClearOutcome::Native(commit)
            } else {
                ClearOutcome::Hold(Some(commit))
            }
        }
        ClearDecision::Fallback => {
            if tokio::time::timeout_at(end, host.fallback(end))
                .await
                .unwrap_or(false)
            {
                ClearOutcome::Fallback
            } else {
                ClearOutcome::Hold(None)
            }
        }
        ClearDecision::Hold => ClearOutcome::Hold(None),
    }
}
