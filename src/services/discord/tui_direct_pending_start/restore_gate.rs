use std::sync::Arc;

use super::super::SharedData;
use super::TuiDirectPendingStart;
use crate::services::discord::input_runtime::fence;

/// Checked at each claim instant: a previous-process record whose provider is proven
/// idle has no output left to own, so it is retired instead of claimed.
pub(super) async fn already_finished(
    shared: &Arc<SharedData>,
    record: &TuiDirectPendingStart,
) -> bool {
    if record.generation == 0
        || record.generation == shared.restart.current_generation
        || record.captured_source.is_some()
    {
        return false;
    }
    let tmux_session_name = record.tmux_session_name.clone();
    let idle =
        tokio::task::spawn_blocking(move || provider_session_proven_idle(&tmux_session_name))
            .await
            .unwrap_or(false);
    idle && super::retire_completed(record.key(), &record.tmux_session_name)
}

/// A closed input gate keeps the durable record, starts no worker and frees the prompt-anchor
/// slot; an admitted worker holds its effect until it claims, aborts or gives up.
pub(super) fn input_effect(record: &TuiDirectPendingStart) -> Result<Option<fence::Permit>, ()> {
    let provider =
        crate::services::provider::ProviderKind::from_str_or_unsupported(&record.provider);
    fence::effect::admit(&provider, record.channel_id).map_err(|failure| {
        fence::record_failure(
            &provider,
            record.channel_id,
            &[record.anchor_message_id],
            failure,
        );
        super::release_prompt_anchor_slot(record);
    })
}

pub(super) fn spawn_admitted(
    effect: Result<Option<fence::Permit>, ()>,
    worker: impl std::future::Future<Output = ()> + Send + 'static,
) {
    if let Ok(permit) = effect {
        // An admitted worker is polled on the input worker, where its claim's row writer is allowed.
        super::super::task_supervisor::spawn_observed(
            "tui_direct_pending_start_worker",
            fence::effect::detached(permit.clone(), fence::effect::scope(permit, worker)),
        );
    }
}

#[cfg(not(test))]
fn provider_session_proven_idle(tmux_session_name: &str) -> bool {
    crate::services::tmux_turn_liveness::provider_session_is_proven_idle(tmux_session_name)
}

#[cfg(test)]
pub(in crate::services::discord) static PROVIDER_IDLE_PROBE_FOR_TESTS: std::sync::Mutex<(
    bool,
    u32,
)> = std::sync::Mutex::new((false, 0));

#[cfg(test)]
fn provider_session_proven_idle(_tmux_session_name: &str) -> bool {
    let mut probe = PROVIDER_IDLE_PROBE_FOR_TESTS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    probe.1 += 1;
    probe.0
}
