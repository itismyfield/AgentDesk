//! One `intake_stage` line per intake waypoint of a Discord message, timed from its snowflake
//! creation; stage gaps come from subtracting lines that share a `msg_id`.

use std::fmt::Write as _;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::services::tui_input::input_key::is_discord_key;

const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// A line captured while a lock may be held: message, stage, capture time in ms, wait in ms.
type Line = (u64, &'static str, u64, Option<u64>);

tokio::task_local! {
    /// The gateway message whose event handler runs on this task, for waypoints that see no id.
    static GATEWAY_MESSAGE: Option<u64>;
    /// A queued item's representative id and the source ids merged into it.
    static MERGED: (u64, Arc<[u64]>);
    /// Lines [`defer`] captured for the enclosing [`after`] to write.
    static DEFERRED: Arc<Mutex<Vec<Line>>>;
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

/// `merged` names the queued representative and the message the stage acted on.
fn write_line(
    message: u64,
    stage: &str,
    at_ms: u64,
    wait_ms: Option<u64>,
    merged: Option<(u64, u64)>,
) {
    let since = since_created_ms(message, at_ms);
    let mut line = format!("intake_stage msg_id={message} stage={stage} since_created_ms={since}");
    if let Some(wait) = wait_ms {
        let _ = write!(line, " wait_ms={wait}");
    }
    if let Some((rep, target)) = merged {
        let _ = write!(line, " rep_id={rep} target_id={target}");
    }
    tracing::info!("{line}");
}

/// Writes a Discord message's line, and one per source merged into it when it is the queued
/// representative; synthetic ids carry no snowflake time and are skipped.
fn emit(message: u64, stage: &str, at_ms: u64, wait_ms: Option<u64>) {
    if !is_discord_key(message) {
        return;
    }
    write_line(message, stage, at_ms, wait_ms, None);
    let _ = MERGED.try_with(|(rep, sources)| {
        if *rep != message {
            return;
        }
        for &source in sources.iter() {
            if source != message && is_discord_key(source) {
                write_line(source, stage, at_ms, wait_ms, Some((*rep, message)));
            }
        }
    });
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

/// Runs a queued item's handling with its merged source ids, so each source gets the
/// representative's lines.
pub(crate) fn merged<F: Future>(
    representative: u64,
    sources: Vec<u64>,
    run: F,
) -> impl Future<Output = F::Output> {
    MERGED.scope((representative, sources.into()), run)
}

/// Captures a line for the enclosing [`after`]; outside one it is dropped rather than written
/// where a lock may be held.
pub(crate) fn defer(message: u64, stage: &'static str, wait_ms: Option<u64>) {
    let line = (message, stage, now_ms(), wait_ms);
    let _ = DEFERRED.try_with(|lines| lines.lock().unwrap_or_else(|e| e.into_inner()).push(line));
}

/// Runs `locked`, which may hold locks, and writes the lines it deferred once it returns.
pub(crate) async fn after<F: Future>(locked: F) -> F::Output {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let output = DEFERRED.scope(lines.clone(), locked).await;
    let lines = std::mem::take(&mut *lines.lock().unwrap_or_else(|e| e.into_inner()));
    for (message, stage, at_ms, wait_ms) in lines {
        emit(message, stage, at_ms, wait_ms);
    }
    output
}

/// A lock guard that logs its acquisition wait after release, so the line never lengthens a hold.
pub(crate) struct Waited<G> {
    guard: Option<G>,
    line: Option<Line>,
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
            emit(message, stage, at_ms, wait_ms);
        }
    }
}

/// Awaits `acquire`; under a gateway message the wait is logged as `stage` once the guard drops.
pub(crate) async fn lock_wait<F: Future>(stage: &'static str, acquire: F) -> Waited<F::Output> {
    let message = GATEWAY_MESSAGE.try_with(|m| *m).ok().flatten();
    let started = Instant::now();
    let guard = acquire.await;
    let line = message.map(|m| {
        (
            m,
            stage,
            now_ms(),
            Some(started.elapsed().as_millis() as u64),
        )
    });
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
