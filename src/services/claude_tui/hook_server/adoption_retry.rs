//! Claude continuation adoption requested by a hook, plus a per-pane queue of adoptions whose
//! binding event could not be persisted, so recovery does not wait for another hook.

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

use crate::services::tmux_common::with_tmux_source_authority;
use crate::services::tui_prompt_dedupe::binding_events::HookSignal;
use crate::services::tui_prompt_dedupe::{
    adopt_claude_continuation_session, claude_session_rotation_for_tmux, resolve_tmux_session_name,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdoptionReport {
    Adopted,
    NotAdopted,
    Deferred,
}

#[derive(Clone)]
struct DeferredAdoption {
    command_session_id: String,
    payload_session_id: String,
    hook: HookSignal,
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
    let session = &request.payload_session_id;
    match queue.iter_mut().find(|q| &q.payload_session_id == session) {
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

fn pop_front(tmux_session_name: &str) {
    let mut queues = deferred();
    if let Some(queue) = queues.get_mut(tmux_session_name) {
        queue.pop_front();
        if queue.is_empty() {
            queues.remove(tmux_session_name);
        }
    }
}

pub(crate) fn adopt_from_hook(
    command_session_id: &str,
    payload_session_id: &str,
    hook: &HookSignal,
) -> AdoptionReport {
    let request = DeferredAdoption {
        command_session_id: command_session_id.to_owned(),
        payload_session_id: payload_session_id.to_owned(),
        hook: hook.clone(),
    };
    let tmux = resolve_tmux_session_name("claude", command_session_id.trim()).unwrap_or_default();
    // Adoption and artifact cutover share the pane authority so a retry cannot rewrite them late.
    with_tmux_source_authority(&tmux, |_| {
        if deferred().contains_key(&tmux) {
            queue_behind(&tmux, &request);
            return AdoptionReport::Deferred;
        }
        settle(&tmux, &request, false)
    })
}

fn settle(tmux: &str, request: &DeferredAdoption, queued: bool) -> AdoptionReport {
    let provider = "claude";
    let command_session_id = request.command_session_id.as_str();
    let payload_session_id = request.payload_session_id.as_str();
    match adopt_claude_continuation_session(command_session_id, payload_session_id, &request.hook) {
        Ok(adopted) => {
            if queued {
                pop_front(tmux);
            }
            let Some((tmux_session_name, transcript_path)) = adopted else {
                tracing::debug!(
                    provider,
                    command_session_id,
                    payload_session_id,
                    "Claude hook payload session differs from command identity but no safe runtime binding adoption was available"
                );
                return AdoptionReport::NotAdopted;
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
            AdoptionReport::Adopted
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
            AdoptionReport::Deferred
        }
    }
}

/// Re-runs each pane's deferred adoptions in hook order with their original evidence.
pub(crate) fn retry_deferred_claude_adoptions() {
    let panes: Vec<String> = deferred().keys().cloned().collect();
    for tmux in panes {
        with_tmux_source_authority(&tmux, |_| {
            // The rotation ledger keeps only the first old transcript, so B→C waits until A→B settles.
            while claude_session_rotation_for_tmux(&tmux).is_none()
                && let Some(request) = front(&tmux)
                && settle(&tmux, &request, true) != AdoptionReport::Deferred
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
