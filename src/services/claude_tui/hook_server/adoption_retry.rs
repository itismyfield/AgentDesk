//! Claude continuation adoption requested by a hook, plus one retry slot per pane for adoptions
//! whose binding event could not be persisted, so recovery does not wait for another hook.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::services::tui_prompt_dedupe::adopt_claude_continuation_session;
use crate::services::tui_prompt_dedupe::binding_events::HookSignal;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdoptionReport {
    Adopted,
    NotAdopted,
    Deferred,
}

struct DeferredAdoption {
    command_session_id: String,
    payload_session_id: String,
    hook: HookSignal,
}

static DEFERRED: LazyLock<Mutex<HashMap<String, DeferredAdoption>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn deferred() -> std::sync::MutexGuard<'static, HashMap<String, DeferredAdoption>> {
    DEFERRED.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub(crate) fn adopt_from_hook(
    command_session_id: &str,
    payload_session_id: &str,
    hook: &HookSignal,
) -> AdoptionReport {
    let provider = "claude";
    match adopt_claude_continuation_session(command_session_id, payload_session_id, hook) {
        Ok(Some((tmux_session_name, transcript_path))) => {
            let mut slots = deferred();
            let owed = slots.get(&tmux_session_name);
            if owed.is_some_and(|slot| slot.hook.received_at <= hook.received_at) {
                slots.remove(&tmux_session_name);
            }
            drop(slots);
            match crate::services::claude_tui::session::persist_claude_continuation_session(
                &tmux_session_name,
                payload_session_id,
            ) {
                // #5188: the old wording ("adopted Claude continuation
                // session") read as if the whole delivery path had followed
                // the rotation. It had not — only the in-memory runtime
                // binding was rebound, and the launch-script rehydration pass
                // could then revert even that. The message now states exactly
                // what this call site changes and defers the rest to the
                // rotation ledger, so a reader cannot mistake it for
                // end-to-end success.
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
        Ok(None) => {
            tracing::debug!(
                provider,
                command_session_id,
                payload_session_id,
                "Claude hook payload session differs from command identity but no safe runtime binding adoption was available"
            );
            AdoptionReport::NotAdopted
        }
        Err(failure) => {
            let request = DeferredAdoption {
                command_session_id: command_session_id.to_owned(),
                payload_session_id: payload_session_id.to_owned(),
                hook: hook.clone(),
            };
            // The newest hook for a pane wins; an older retry must not replace it.
            let mut slots = deferred();
            let newer = slots
                .get(&failure.tmux_session)
                .is_none_or(|slot| slot.hook.received_at <= request.hook.received_at);
            if newer {
                slots.insert(failure.tmux_session.clone(), request);
            }
            drop(slots);
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

/// Re-runs every deferred adoption with its original hook evidence; each one that fails again stays queued.
pub(crate) fn retry_deferred_claude_adoptions() {
    let requests: Vec<DeferredAdoption> = deferred().drain().map(|(_, request)| request).collect();
    for request in requests {
        adopt_from_hook(
            &request.command_session_id,
            &request.payload_session_id,
            &request.hook,
        );
    }
}

#[cfg(test)]
pub(crate) fn deferred_adoption_count() -> usize {
    deferred().len()
}
