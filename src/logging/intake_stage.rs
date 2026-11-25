//! One `intake_stage` line per intake waypoint of a Discord message, timed from its snowflake
//! creation; stage gaps come from subtracting lines that share a `msg_id`.

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

tokio::task_local! {
    /// The gateway message whose event handler runs on this task, for waypoints that see no id.
    static GATEWAY_MESSAGE: Option<u64>;
}

/// Milliseconds from `message`'s snowflake creation to `now_ms`; a clock behind creation reads 0.
fn since_created_ms(message: u64, now_ms: u64) -> u64 {
    now_ms.saturating_sub((message >> 22) + DISCORD_EPOCH_MS)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn emit(message: u64, stage: &str, at_ms: u64, wait_ms: Option<u64>) {
    let since = since_created_ms(message, at_ms);
    match wait_ms {
        Some(wait) => tracing::info!(
            "intake_stage msg_id={message} stage={stage} since_created_ms={since} wait_ms={wait}"
        ),
        None => {
            tracing::info!("intake_stage msg_id={message} stage={stage} since_created_ms={since}")
        }
    }
}

pub(crate) fn mark(message: u64, stage: &str, wait_ms: Option<u64>) {
    emit(message, stage, now_ms(), wait_ms);
}

/// Marks the gateway message running on this task, if any.
pub(crate) fn mark_current(stage: &str) {
    if let Some(message) = GATEWAY_MESSAGE.try_with(|m| *m).ok().flatten() {
        mark(message, stage, None);
    }
}

/// Marks the user message of a `discord:<channel>:<message>` turn id.
pub(crate) fn mark_turn(turn_id: &str, stage: &str) {
    let message = turn_id
        .strip_prefix("discord:")
        .and_then(|rest| rest.rsplit_once(':'))
        .and_then(|(_, message)| message.parse().ok());
    if let Some(message) = message {
        mark(message, stage, None);
    }
}

/// Runs a gateway event handler with `message` visible to [`mark_current`] and [`lock_wait`].
pub(crate) fn gateway<F: Future>(
    message: Option<u64>,
    handler: F,
) -> impl Future<Output = F::Output> {
    if let Some(message) = message {
        mark(message, "gateway", None);
    }
    GATEWAY_MESSAGE.scope(message, handler)
}

/// A lock guard that logs its acquisition wait after release, so the line never lengthens a hold.
pub(crate) struct Waited<G> {
    guard: Option<G>,
    line: Option<(u64, &'static str, u64, u64)>,
}

impl<G> Deref for Waited<G> {
    type Target = G;
    fn deref(&self) -> &G {
        self.guard.as_ref().expect("guard is held until drop")
    }
}

impl<G> DerefMut for Waited<G> {
    fn deref_mut(&mut self) -> &mut G {
        self.guard.as_mut().expect("guard is held until drop")
    }
}

impl<G> Drop for Waited<G> {
    fn drop(&mut self) {
        drop(self.guard.take());
        if let Some((message, stage, at_ms, wait_ms)) = self.line.take() {
            emit(message, stage, at_ms, Some(wait_ms));
        }
    }
}

/// Awaits `acquire`; under a gateway message the wait is logged as `stage` once the guard drops.
pub(crate) async fn lock_wait<F: Future>(stage: &'static str, acquire: F) -> Waited<F::Output> {
    let message = GATEWAY_MESSAGE.try_with(|m| *m).ok().flatten();
    let started = Instant::now();
    let guard = acquire.await;
    let line = message.map(|m| (m, stage, now_ms(), started.elapsed().as_millis() as u64));
    Waited {
        guard: Some(guard),
        line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_created_counts_from_snowflake_and_floors_future_at_zero() {
        let created = 1_764_029_880_123;
        let message = ((created - DISCORD_EPOCH_MS) << 22) | 0x3f_ffff;
        assert_eq!(since_created_ms(message, created), 0);
        assert_eq!(since_created_ms(message, created + 7_400), 7_400);
        assert_eq!(since_created_ms(message, created - 1), 0);
    }
}
