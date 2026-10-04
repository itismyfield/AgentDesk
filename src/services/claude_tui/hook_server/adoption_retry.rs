//! Claude continuation adoption requested by a hook, plus a per-pane queue of adoptions not yet
//! persisted or still waiting for their transcript, so recovery does not wait for another hook.

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

use crate::services::tmux_common::with_tmux_source_authority;
use crate::services::tui_prompt_dedupe::binding_context::{
    CapturedContext, HookBindingEnvelope, SpawnNonceMarker, observe_spawn_nonce_marker,
};
use crate::services::tui_prompt_dedupe::binding_events::HookSignal;
use crate::services::tui_prompt_dedupe::{
    AdoptSkip, adopt_claude_continuation_explained, claude_session_rotation_for_tmux,
    reclaim_with_current_prompt, resolve_tmux_session_name,
};

fn captured_nonce(envelope: Option<&HookBindingEnvelope>) -> Option<String> {
    match envelope.map(|e| &e.context) {
        Some(CapturedContext::Captured(context)) => Some(context.execution_nonce.clone()),
        _ => None,
    }
}

fn observation_pane(command: &str, envelope: Option<&HookBindingEnvelope>) -> String {
    resolve_tmux_session_name("claude", command.trim())
        .or_else(|| match envelope.map(|e| &e.context) {
            Some(CapturedContext::Captured(context)) => Some(context.tmux_session.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

// Legacy observations have no execution evidence; only captured executions are fenced.
fn execution_current(tmux: &str, nonce: Option<&str>) -> Result<(), NotDurableReason> {
    let Some(nonce) = nonce else {
        return Ok(());
    };
    match observe_spawn_nonce_marker(tmux) {
        SpawnNonceMarker::Known(current) if current == nonce => Ok(()),
        SpawnNonceMarker::Unreadable => Err(NotDurableReason::ExecutionUnreadable),
        _ => Err(NotDurableReason::ExecutionChanged),
    }
}

/// What the hook that asked for an adoption may be told, judged by its own binding evidence only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdoptionHttp {
    Durable(DurableKind),
    Skipped(AdoptSkip),
    NotDurable(NotDurableReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DurableKind {
    Pending,
    Adopted,
    AlreadyRecorded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotDurableReason {
    Append,
    QueuedBehind,
    ExecutionChanged,
    ExecutionUnreadable,
}

/// Whether the pane's queue moves on to its next entry in this poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueStep {
    Pop,
    Hold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SettleOutcome {
    pub(crate) http: AdoptionHttp,
    pub(crate) queue: QueueStep,
}

#[derive(Clone)]
struct DeferredAdoption {
    command_session_id: String,
    payload_session_id: String,
    hook: HookSignal,
    execution_nonce: Option<String>,
    pending_record: Option<(u64, u64)>,
    /// The entry's binding event is in the log; it only waits for its rotation or its transcript.
    recorded: bool,
    /// Recorded as a Pending whose transcript is not verified yet.
    pending: bool,
}

// Deferred sources of a pane, oldest first; only repeat hooks of one source share an entry.
static DEFERRED: LazyLock<Mutex<HashMap<String, VecDeque<DeferredAdoption>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn deferred() -> std::sync::MutexGuard<'static, HashMap<String, VecDeque<DeferredAdoption>>> {
    DEFERRED.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn queue_behind(tmux_session_name: &str, request: &DeferredAdoption) {
    let mut queues = deferred();
    let queue = queues.entry(tmux_session_name.to_owned()).or_default();
    // Only the same durable record can promote legacy evidence to its restored execution.
    if let Some(key) = request.pending_record
        && let Some(queued) = queue.iter_mut().find(|q| {
            q.pending_record == Some(key)
                && (q.execution_nonce.is_none() || q.execution_nonce == request.execution_nonce)
        })
    {
        if queued.execution_nonce.is_none() {
            queued.execution_nonce = request.execution_nonce.clone();
        }
        queued.recorded |= request.recorded;
        queued.pending |= request.pending;
        return;
    }
    let session = &request.payload_session_id;
    match queue.iter_mut().find(|q| {
        &q.payload_session_id == session
            && q.execution_nonce == request.execution_nonce
            && (q.pending_record.is_none()
                || request.pending_record.is_none()
                || q.pending_record == request.pending_record)
    }) {
        // A repeat hook keeps the first SessionStart, the only one that names the transition.
        Some(queued) if queued.hook.event != "session_start" => {
            if request.hook.event == "session_start" {
                queued.hook = request.hook.clone();
            }
        }
        Some(_) => {}
        None => queue.push_back(request.clone()),
    }
}

fn front(tmux_session_name: &str) -> Option<DeferredAdoption> {
    deferred().get(tmux_session_name)?.front().cloned()
}

fn mark_front_recorded(tmux_session_name: &str, pending: bool, record: Option<(u64, u64)>) {
    if let Some(front) = deferred()
        .get_mut(tmux_session_name)
        .and_then(VecDeque::front_mut)
    {
        front.recorded = true;
        front.pending = pending;
        if record.is_some() {
            front.pending_record = record;
        }
    }
}

fn queued_count(tmux_session_name: &str) -> usize {
    deferred().get(tmux_session_name).map_or(0, VecDeque::len)
}

fn pop_front(tmux_session_name: &str) {
    let mut queues = deferred();
    if let Some(queue) = queues.get_mut(tmux_session_name) {
        queue.pop_front();
        if queue.is_empty() {
            queues.remove(tmux_session_name);
        }
    }
}

#[cfg(test)]
pub(crate) fn adopt_from_hook(
    command_session_id: &str,
    payload_session_id: &str,
    hook: &HookSignal,
) -> AdoptionHttp {
    adopt_from_enveloped_hook(command_session_id, payload_session_id, hook, None)
}

pub(crate) fn adopt_from_enveloped_hook(
    command_session_id: &str,
    payload_session_id: &str,
    hook: &HookSignal,
    envelope: Option<&HookBindingEnvelope>,
) -> AdoptionHttp {
    let request = DeferredAdoption {
        command_session_id: command_session_id.to_owned(),
        payload_session_id: payload_session_id.to_owned(),
        hook: hook.clone(),
        execution_nonce: captured_nonce(envelope),
        pending_record: None,
        recorded: false,
        pending: false,
    };
    let tmux = observation_pane(command_session_id, envelope);
    // Adoption and artifact cutover share the pane authority so a retry cannot rewrite them late.
    with_tmux_source_authority(&tmux, |_| {
        if let Err(reason) = execution_current(&tmux, request.execution_nonce.as_deref()) {
            return AdoptionHttp::NotDurable(reason);
        }
        // A stale queue entry cannot lend its recorded ACK to a new execution's hook.
        while front(&tmux).is_some_and(|entry| {
            execution_current(&tmux, entry.execution_nonce.as_deref())
                == Err(NotDurableReason::ExecutionChanged)
        }) {
            pop_front(&tmux);
        }
        // A hook naming another session replaces a recorded Pending still waiting for its transcript,
        // once the new session's own evidence is durable.
        if claude_session_rotation_for_tmux(&tmux).is_none()
            && front(&tmux).is_some_and(|f| f.pending && f.payload_session_id != payload_session_id)
        {
            let http = settle(&tmux, &request, false).http;
            if matches!(http, AdoptionHttp::Durable(_)) {
                pop_front(&tmux);
            }
            return http;
        }
        let Some((own_recorded, own_is_front)) = own_entry(&tmux, &request) else {
            if !deferred().contains_key(&tmux) {
                return settle(&tmux, &request, false).http;
            }
            queue_behind(&tmux, &request);
            return AdoptionHttp::NotDurable(NotDurableReason::QueuedBehind);
        };
        queue_behind(&tmux, &request);
        let settles_now = own_is_front && claude_session_rotation_for_tmux(&tmux).is_none();
        match own_recorded {
            true => {
                // Settling now only saves the poll's delay, but an adoption this hook could not
                // persist or was refused is still its answer.
                let settled = front(&tmux).filter(|_| settles_now);
                match settled.map(|own| settle(&tmux, &own, true).http) {
                    Some(http @ (AdoptionHttp::NotDurable(_) | AdoptionHttp::Skipped(_))) => http,
                    _ => AdoptionHttp::Durable(DurableKind::AlreadyRecorded),
                }
            }
            false if settles_now => front(&tmux).map_or(
                AdoptionHttp::NotDurable(NotDurableReason::QueuedBehind),
                |own| settle(&tmux, &own, true).http,
            ),
            false => AdoptionHttp::NotDurable(NotDurableReason::QueuedBehind),
        }
    })
}

/// A prompt of the pane's own launch session, which adoption never sees: under the pane authority it
/// may supersede a waiting Pending it outlived, and then that Pending's queued retry is dropped.
pub(crate) fn observe_launch_hook(
    session_id: &str,
    hook: &HookSignal,
    envelope: Option<&HookBindingEnvelope>,
    reclaim: bool,
) -> Result<(), NotDurableReason> {
    let tmux = observation_pane(session_id, envelope);
    with_tmux_source_authority(&tmux, |_| {
        execution_current(&tmux, captured_nonce(envelope).as_deref())?;
        if reclaim
            && reclaim_with_current_prompt(session_id, hook)
            && front(&tmux).is_some_and(|f| f.pending && f.payload_session_id != session_id)
        {
            pop_front(&tmux);
        }
        Ok(())
    })
}

/// `(recorded, at front)` of the queued entry of `payload_session_id`, if the pane queues one.
fn own_entry(tmux: &str, request: &DeferredAdoption) -> Option<(bool, bool)> {
    let queues = deferred();
    let queue = queues.get(tmux)?;
    let at = queue.iter().position(|q| {
        q.payload_session_id == request.payload_session_id
            && q.execution_nonce == request.execution_nonce
    })?;
    Some((queue[at].recorded, at == 0))
}

fn settle(tmux: &str, request: &DeferredAdoption, queued: bool) -> SettleOutcome {
    if let Err(reason) = execution_current(tmux, request.execution_nonce.as_deref()) {
        let changed = reason == NotDurableReason::ExecutionChanged;
        if queued && changed {
            pop_front(tmux);
        }
        return SettleOutcome {
            http: AdoptionHttp::NotDurable(reason),
            queue: if changed {
                QueueStep::Pop
            } else {
                QueueStep::Hold
            },
        };
    }
    let provider = "claude";
    let command_session_id = request.command_session_id.as_str();
    let payload_session_id = request.payload_session_id.as_str();
    // A queued entry whose command session now names another pane cannot settle on its own pane.
    if queued
        && resolve_tmux_session_name(provider, command_session_id.trim()).as_deref() != Some(tmux)
    {
        pop_front(tmux);
        let http = AdoptionHttp::Skipped(AdoptSkip::UnmappedCommandSession);
        return SettleOutcome {
            http,
            queue: QueueStep::Pop,
        };
    }
    match adopt_claude_continuation_explained(command_session_id, payload_session_id, &request.hook)
    {
        Ok((adopted, skip)) => {
            // An adopted source stays queued until its rotation settles and a recorded Pending until
            // its transcript is verified, so later hooks wait behind it; a later queued session replaces it.
            let pending = adopted.is_none() && skip.is_none();
            let pending_record = pending
                .then(|| crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux))
                .flatten()
                .and_then(|channel| {
                    crate::services::tui_prompt_dedupe::binding_events::pending_seq(
                        channel,
                        tmux,
                        payload_session_id,
                    )
                    .map(|seq| (channel, seq))
                });
            // An entry whose pane lost its channel mapping keeps its place until the next pass restores it.
            let no_channel = skip == Some(AdoptSkip::ChannelNotRestored);
            // An unreadable log or pinned file, or a withheld Herdr pane, decided nothing, so the entry
            // keeps its place and marks.
            let transient = matches!(
                skip,
                Some(
                    AdoptSkip::HistoryUnreadable
                        | AdoptSkip::SourceUnreadable
                        | AdoptSkip::HostNotAdmitted
                )
            );
            let held = if pending {
                !(queued && queued_count(tmux) > 1)
            } else {
                no_channel
                    || transient
                    || (adopted.is_some() && claude_session_rotation_for_tmux(tmux).is_some())
            };
            let queue = if held {
                QueueStep::Hold
            } else {
                QueueStep::Pop
            };
            if queued && held && !no_channel && !transient {
                mark_front_recorded(tmux, pending, pending_record);
            } else if queued && !held {
                pop_front(tmux);
            } else if pending {
                let (recorded, pending) = (true, true);
                let request = request.clone();
                queue_behind(
                    tmux,
                    &DeferredAdoption {
                        recorded,
                        pending,
                        pending_record,
                        ..request
                    },
                );
            }
            let http = match skip {
                Some(skip) => AdoptionHttp::Skipped(skip),
                None if adopted.is_some() => AdoptionHttp::Durable(DurableKind::Adopted),
                None => AdoptionHttp::Durable(DurableKind::Pending),
            };
            let Some((tmux_session_name, transcript_path)) = adopted else {
                tracing::debug!(
                    provider,
                    command_session_id,
                    payload_session_id,
                    ?skip,
                    "Claude hook payload session differs from command identity; no runtime binding was adopted"
                );
                return SettleOutcome { http, queue };
            };
            before_artifacts(payload_session_id);
            match crate::services::claude_tui::session::persist_claude_continuation_session(
                &tmux_session_name,
                payload_session_id,
            ) {
                // Only the in-memory binding moved here; the rotation ledger carries the rest,
                // so the message must not read as end-to-end delivery.
                Ok(changed) => tracing::warn!(
                    provider,
                    command_session_id,
                    payload_session_id,
                    tmux_session_name,
                    transcript_path,
                    persistent_artifacts_changed = changed,
                    "rebound Claude TUI runtime binding to the continuation session reported by \
                     the hook payload; rotation queued for delivery-path propagation (#5188)"
                ),
                Err(error) => tracing::error!(
                    provider,
                    command_session_id,
                    payload_session_id,
                    tmux_session_name,
                    error,
                    "adopted Claude continuation in memory but failed to persist cutover artifacts"
                ),
            }
            SettleOutcome { http, queue }
        }
        Err(failure) => {
            if !queued {
                queue_behind(&failure.tmux_session, request);
            }
            tracing::error!(
                provider,
                tmux_session_name = failure.tmux_session,
                payload_session_id,
                error = %failure.error,
                "Claude continuation adoption deferred until its binding event can be persisted"
            );
            SettleOutcome {
                http: AdoptionHttp::NotDurable(NotDurableReason::Append),
                queue: QueueStep::Hold,
            }
        }
    }
}

/// Queues a Pending restored from the log as recorded, so the poll adopts it once its transcript
/// is verified and a hook naming another session replaces it. The restore holds the pane's authority.
pub(crate) fn seed_restored(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    command_session_id: &str,
    payload_session_id: &str,
    hook: &HookSignal,
    execution_nonce: &str,
    pending_record: (u64, u64),
) {
    let request = DeferredAdoption {
        command_session_id: command_session_id.to_owned(),
        payload_session_id: payload_session_id.to_owned(),
        hook: hook.clone(),
        execution_nonce: Some(execution_nonce.to_owned()),
        pending_record: Some(pending_record),
        recorded: true,
        pending: true,
    };
    queue_behind(authority.session(), &request);
}

/// Re-runs each pane's deferred adoptions in hook order with their original evidence.
pub(crate) fn retry_deferred_claude_adoptions() {
    let panes: Vec<String> = deferred().keys().cloned().collect();
    for tmux in panes {
        with_tmux_source_authority(&tmux, |_| {
            // The rotation ledger keeps only the first old transcript, so B→C waits until A→B settles.
            while claude_session_rotation_for_tmux(&tmux).is_none()
                && let Some(request) = front(&tmux)
                && settle(&tmux, &request, true).queue == QueueStep::Pop
            {}
        });
    }
}

#[cfg(not(test))]
fn before_artifacts(_payload_session_id: &str) {}

#[cfg(test)]
type ArtifactProbe = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

#[cfg(test)]
static ARTIFACT_PROBE: Mutex<Option<ArtifactProbe>> = Mutex::new(None);

#[cfg(test)]
fn before_artifacts(payload_session_id: &str) {
    let probe = ARTIFACT_PROBE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    if let Some(probe) = probe {
        probe(payload_session_id);
    }
}

/// Runs `probe` with the payload session just before each artifact cutover.
#[cfg(test)]
pub(crate) fn set_artifact_probe(probe: Option<ArtifactProbe>) {
    *ARTIFACT_PROBE.lock().unwrap_or_else(|p| p.into_inner()) = probe;
}

#[cfg(test)]
pub(crate) fn deferred_adoption_count() -> usize {
    deferred().values().map(VecDeque::len).sum()
}

#[cfg(test)]
pub(crate) fn forget_pending_record_for_tests(tmux: &str) {
    if let Some(queue) = deferred().get_mut(tmux) {
        for entry in queue {
            entry.pending_record = None;
        }
    }
}

#[cfg(test)]
pub(crate) fn reset_deferred_adoptions_for_tests() {
    deferred().clear();
}
