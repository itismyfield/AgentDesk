use std::sync::Mutex;

use sqlx::PgPool;

use crate::services::engine_runtime::health_port::SlotResetOutcome;

/// Each spawned slot-thread runtime clear, in order, for the tests that wait on it.
static RUNTIME_CLEARS: Mutex<Vec<(u64, SlotResetOutcome)>> = Mutex::new(Vec::new());
static RUNTIME_CLEARS_DONE: Mutex<Vec<Vec<u64>>> = Mutex::new(Vec::new());

pub(super) fn record_runtime_clear(thread: u64, outcome: SlotResetOutcome) {
    RUNTIME_CLEARS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push((thread, outcome));
}

pub(super) fn record_runtime_clear_done(slot_threads: Vec<u64>) {
    RUNTIME_CLEARS_DONE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(slot_threads);
}

pub fn runtime_clear_completion_count(slot_threads: &[u64]) -> usize {
    RUNTIME_CLEARS_DONE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .iter()
        .filter(|run| run.as_slice() == slot_threads)
        .count()
}

pub fn take_runtime_clears_after_done(
    slot_threads: &[u64],
) -> Option<Vec<(u64, SlotResetOutcome)>> {
    let mut runs = RUNTIME_CLEARS_DONE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let run = runs.iter().position(|run| run.as_slice() == slot_threads)?;
    runs.remove(run);
    Some(
        RUNTIME_CLEARS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .extract_if(.., |(thread, _)| slot_threads.contains(thread))
            .collect(),
    )
}

pub async fn runtime_target_session_keys(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
    channels: &[u64],
) -> Result<Vec<Option<String>>, String> {
    let target = super::build_slot_clear_target_pg(pool, agent_id, slot_index).await?;
    Ok(channels
        .iter()
        .map(|channel| {
            target
                .runtime_targets
                .iter()
                .find(|target| target.thread_channel_id == *channel)
                .and_then(|target| target.session_key.clone())
        })
        .collect())
}
