//! A Decision original on a queue no input owner drives. Its source names the event, PG says
//! whether it may still run, and only a wait that proved nothing was written puts it back.

use std::sync::atomic::Ordering;

use sqlx::PgPool;
use uuid::Uuid;

use super::{OPEN, RUN, note_open, run, settle};
use crate::db::decisions::{self, AttemptState};
use crate::services::turn_orchestrator::Intervention;

/// The origin a queue entry claims under the Decision source, whether or not it names an event.
pub(crate) fn origin(entry: &Intervention) -> Option<&str> {
    (entry.source_message_queued_generations.iter())
        .filter_map(|source| source.origin_id.as_deref())
        .find(|origin| origin.starts_with("decision:"))
}

/// What a non-owner drain does with the original it dequeued.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Dequeued {
    /// Not a Decision input: the queue runs it as it always did.
    Other,
    /// The approved event, journaled for this one start.
    Journaled(Uuid),
    /// Closed, never approved or not the approved input: it never runs and is not put back.
    Closed,
    /// PG could not be read: the original goes back unrun.
    Unread,
}

/// Checks a dequeued original against its PG event and takes the journal its start runs under.
/// A journal an earlier attempt left on a still queued original closes unknown, never reruns.
pub(crate) async fn begin(pool: Option<&PgPool>, channel: u64, entry: &Intervention) -> Dequeued {
    use AttemptState::{MayHaveSent, UnknownClosed};
    let Some(origin) = origin(entry) else {
        return Dequeued::Other;
    };
    note_open();
    let (Some(pool), Some(event_id)) = (pool, decisions::event_of_origin(origin)) else {
        return Dequeued::Closed;
    };
    let event = match (run(pool).await, decisions::load_event(pool, event_id).await) {
        (Ok(_), Ok(event)) => event,
        _ => return Dequeued::Unread,
    };
    // A Decision original is never merged: one source, the approved actor, the approved bytes.
    let approved = event.is_some_and(|event| {
        event.origin() == origin
            && event.channel_id() == channel
            && event.actor_id() == entry.author_id.get()
            && entry.source_message_queued_generations.len() == 1
            && event.approves(&entry.text)
    });
    if !approved {
        return Dequeued::Closed;
    }
    match decisions::begin_write(pool, event_id).await {
        Ok(true) => Dequeued::Journaled(event_id),
        Ok(false) => {
            let gone = (MayHaveSent, UnknownClosed);
            if let Err(error) = decisions::advance(pool, event_id, gone, None, None).await {
                tracing::warn!(%event_id, %error, "decision journal left for boot recovery");
            }
            Dequeued::Closed
        }
        Err(_) => Dequeued::Unread,
    }
}

/// How a journaled non-owner start ended.
pub(crate) enum Ended {
    /// A turn took the input: that turn's receipt settles the event.
    Started,
    /// Nothing was written and the original is back on the queue: it waits again.
    WaitsAgain,
    /// A clear removed the original before anything was written.
    Cleared,
    /// The start failed after its journal, with no proof either way: nothing is sent again.
    Unknown,
}

/// Records how the start `begin` journaled ended; `None` is an input that is no Decision.
pub(crate) async fn end(pool: Option<&PgPool>, event_id: Option<Uuid>, ended: Ended) {
    use AttemptState::{Closing, Failed, MayHaveSent, NormalWait, UnknownClosed};
    let (Some(pool), Some(event_id)) = (pool, event_id) else {
        return;
    };
    let (states, detail) = match ended {
        Ended::Started => return,
        Ended::WaitsAgain => ((MayHaveSent, NormalWait), None),
        Ended::Unknown => ((MayHaveSent, UnknownClosed), Some("queue_start_failed")),
        Ended::Cleared => ((MayHaveSent, Closing), Some("cleared")),
    };
    let mut moved = decisions::advance(pool, event_id, states, None, detail).await;
    if matches!(moved, Ok(true)) && states.1 == Closing {
        moved = decisions::advance(pool, event_id, (Closing, Failed), None, None).await;
    }
    if let Err(error) = moved {
        tracing::warn!(%event_id, %error, "decision queue start left for its next settlement");
    }
}

/// Settles the events of a channel no owner drives from their turns' receipts. The queue's
/// kickoff asks at each turn boundary; until this process met an approved event nothing reads PG.
pub(crate) async fn settle_started(pool: Option<&PgPool>, channel: u64) {
    let Some(pool) = pool.filter(|_| OPEN.load(Ordering::Relaxed)) else {
        return;
    };
    let rows = decisions::unsettled(pool, Some(channel), RUN.get()).await;
    for row in rows.unwrap_or_default() {
        if let Err(error) = settle(pool, None, &row, None).await {
            tracing::warn!(event_id = %row.event_id, %error, "decision input left unsettled");
        }
    }
}

#[cfg(test)]
#[path = "queued_tests.rs"]
mod tests;
