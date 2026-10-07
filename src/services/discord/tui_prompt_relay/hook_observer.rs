#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use crate::services::claude_tui::hook_server::{HookEvent, HookEventKind};
use crate::services::tui_prompt_dedupe::{
    ObservedTuiPrompt, extract_prompt_from_hook_payload, extract_prompt_id_from_hook_payload,
    observe_hook_prompt_by_tmux_with_prompt_id_at, resolve_tmux_session_name,
    subscribe_observed_prompts,
};
use tracing::Instrument;

#[cfg(test)]
#[derive(Default)]
pub(super) struct HookObserverProbe {
    pub(super) pause_after_first: AtomicBool,
    pub(super) paused: AtomicBool,
    pub(super) release: tokio::sync::Notify,
    pub(super) dequeued: std::sync::atomic::AtomicUsize,
    pub(super) alias_dequeued: std::sync::atomic::AtomicUsize,
    pub(super) observation_calls: std::sync::atomic::AtomicUsize,
    pub(super) processed: std::sync::atomic::AtomicUsize,
}

pub(super) fn hook_observation_target(event: &HookEvent) -> Option<String> {
    let target = resolve_tmux_session_name(&event.provider, &event.session_id);
    if let Some(fanout) = event.fanout.as_ref()
        && !fanout.primary_discarded
        && let Some(origin) = resolve_tmux_session_name(&event.provider, &fanout.origin_session_id)
        && target.as_ref() == Some(&origin)
    {
        return None;
    }
    Some(target.unwrap_or_else(|| event.session_id.trim().to_string()))
}

/// Hook prompts and observed-prompt relay share one loop; tests pass their own hooks and relay.
pub(super) fn spawn_tui_prompt_relay_observer(
    provider_name: String,
    hook_rx: tokio::sync::broadcast::Receiver<HookEvent>,
    relay: impl FnMut(ObservedTuiPrompt) -> futures::future::BoxFuture<'static, ()> + Send + 'static,
) {
    spawn_tui_prompt_relay_observer_inner(
        provider_name,
        hook_rx,
        relay,
        #[cfg(test)]
        None,
    );
}

pub(super) fn spawn_tui_prompt_relay_observer_inner(
    provider_name: String,
    mut hook_rx: tokio::sync::broadcast::Receiver<HookEvent>,
    mut relay: impl FnMut(ObservedTuiPrompt) -> futures::future::BoxFuture<'static, ()> + Send + 'static,
    #[cfg(test)] probe: Option<Arc<HookObserverProbe>>,
) {
    let observer_span = tracing::info_span!(
        "tui_prompt_relay_observer",
        provider = %provider_name
    );
    // Subscribe before spawning so callers can publish immediately after this
    // function returns without racing the observer task's first poll.
    let mut observed_rx = subscribe_observed_prompts();
    super::super::task_supervisor::spawn_observed("tui_prompt_relay_observer", async move {
        loop {
            tokio::select! {
                hook_event = async {
                    #[cfg(test)]
                    if let Some(probe) = probe.as_ref()
                        && probe.pause_after_first.load(Ordering::SeqCst)
                        && probe.dequeued.load(Ordering::SeqCst) > 0
                    {
                        probe.paused.store(true, Ordering::SeqCst);
                        probe.release.notified().await;
                    }
                    hook_rx.recv().await
                } => {
                    #[cfg(test)]
                    if let (Some(probe), Ok(event)) = (probe.as_ref(), &hook_event) {
                        probe.dequeued.fetch_add(1, Ordering::SeqCst);
                        if event.fanout.is_some() {
                            probe.alias_dequeued.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    match hook_event {
                        Ok(event) if event.provider == provider_name
                            && event.kind == HookEventKind::UserPromptSubmit =>
                        {
                            if let Some(target) = hook_observation_target(&event)
                                && let Some(prompt) = extract_prompt_from_hook_payload(&event.payload)
                            {
                                #[cfg(test)]
                                if let Some(probe) = probe.as_ref() {
                                    probe.observation_calls.fetch_add(1, Ordering::SeqCst);
                                }
                                let prompt_id = (event.provider == "claude")
                                    .then(|| extract_prompt_id_from_hook_payload(&event.payload))
                                    .flatten();
                                let observation = observe_hook_prompt_by_tmux_with_prompt_id_at(
                                    &event.provider,
                                    &target,
                                    &prompt,
                                    prompt_id.as_deref(),
                                    event.received_at,
                                );
                                tracing::debug!(
                                    provider = %event.provider,
                                    session_id = %event.session_id,
                                    prompt_id = prompt_id.as_deref().unwrap_or(""),
                                    observation = ?observation,
                                    "observed TUI UserPromptSubmit hook"
                                );
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(
                                provider = %provider_name,
                                skipped,
                                "TUI prompt relay lagged hook events"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                    #[cfg(test)]
                    if let Some(probe) = probe.as_ref() {
                        probe.processed.fetch_add(1, Ordering::SeqCst);
                    }
                }
                observed = observed_rx.recv() => {
                    match observed {
                        Ok(prompt) if prompt.provider == provider_name => {
                            relay(prompt).await;
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(
                                provider = %provider_name,
                                skipped,
                                "TUI prompt relay lagged observed prompt events"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        }
    }.instrument(observer_span));
}
