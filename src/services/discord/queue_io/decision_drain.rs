//! The Decision source, refused at every request boundary because only a PG event grants it,
//! and the one settlement an approved event gets after a write, a failure or a restart.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use sqlx::PgPool;
use uuid::Uuid;

#[cfg(unix)]
use super::input_owner::drive::{Front, Handoff};
use super::input_owner::origin::InputOrigin;
use crate::db::decisions::{self, AttemptState, SOURCE, Unsettled};
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, ChannelMailboxRegistry, DecisionHeld as Held,
};

/// Whether a request body claims the Decision source, in its source or its input metadata.
pub(crate) fn self_declared(source: Option<&str>, metadata: Option<&serde_json::Value>) -> bool {
    let named = |value: &str| value.trim().eq_ignore_ascii_case(SOURCE);
    let field = |key| {
        metadata
            .and_then(|metadata| metadata.pointer(key))
            .and_then(serde_json::Value::as_str)
    };
    source.is_some_and(named)
        || field("/human_input/source").is_some_and(named)
        || field("/human_input/origin_id")
            .is_some_and(|origin| origin.trim_start().starts_with("decision:"))
}

pub(crate) const SOURCE_REFUSED: &str = "decision_source_not_allowed";

/// A handoff never carries a Decision input: both its HTTP route and its service entry ask here.
pub(crate) fn reject_handoff_source(
    source: Option<&str>,
    metadata: Option<&serde_json::Value>,
) -> Result<(), &'static str> {
    if self_declared(source, metadata) {
        return Err(SOURCE_REFUSED);
    }
    Ok(())
}

/// Set once this process met an approved event; until then no settlement here reads PG.
static OPEN: AtomicBool = AtomicBool::new(false);
/// This process's start on the database clock, set by its boot recovery.
static BOOT: OnceLock<chrono::DateTime<chrono::Utc>> = OnceLock::new();

/// Marks that approved events exist, so the owner's drain settles them from now on.
pub(crate) fn note_open() {
    OPEN.store(true, Ordering::Relaxed);
}

const INTERRUPTED: &str = "interrupted_before_delivery";
#[cfg(unix)]
const CLOSED: &str = "NotSent(\"decision_closed\")";

/// The origin a queued original's kept context names, when it is a Decision input.
fn origin_of(context: Option<&serde_json::Value>) -> Option<&str> {
    let context = context.filter(|context| ChannelMailboxHandle::decision_input(context))?;
    context.pointer("/metadata/human_input/origin_id")?.as_str()
}

/// The message id the event's original is admitted under on its channel.
fn message_of(channel: u64, origin: &str) -> u64 {
    let metadata = serde_json::json!({"human_input": {"origin_id": origin}});
    InputOrigin::decision_identity(channel, Some(SOURCE), Some(&metadata)).map_or(0, |id| id.0)
}

/// What the channel's owner holds of the original; `None` when no owner holds the channel.
async fn held(mailbox: Option<&ChannelMailboxHandle>, row: &Unsettled) -> Option<Held> {
    let (message, origin) = (message_of(row.channel_id, &row.origin), row.origin.clone());
    let read = mailbox?.v2_request(move |slot| {
        Ok((slot.as_ref()).map(|owner| owner.decision_held(message, &origin)))
    });
    read.await.ok().flatten()
}

/// Removes the event's original from its owner unrun; without an owner there is none to remove.
async fn discard(mailbox: Option<&ChannelMailboxHandle>, row: &Unsettled) -> Result<(), String> {
    let Some(mailbox) = mailbox else {
        return Ok(());
    };
    let (message, origin) = (message_of(row.channel_id, &row.origin), row.origin.clone());
    let removed = mailbox.v2_request(move |slot| match slot.as_mut() {
        Some(owner) => owner.discard_decision(message, &origin),
        None => Ok(()),
    });
    removed.await
}

/// The terminal receipt of the turn that consumed the event's origin: whether it completed.
async fn receipt(pool: &PgPool, row: &Unsettled) -> Option<bool> {
    let channels = [row.channel_id.to_string()];
    let found = crate::db::turns::find_origin_receipt(pool, &row.origin, &row.agent_id, &channels);
    let receipt = found.await.ok().flatten()?;
    Some(receipt.terminal_outcome == "completed")
}

fn unsent(error: &str) -> bool {
    error.starts_with("NotSent(") || error.starts_with("Failed(")
}

async fn step(
    pool: &PgPool,
    row: &Unsettled,
    states: (AttemptState, AttemptState),
    detail: Option<&str>,
) -> Result<bool, String> {
    decisions::advance(pool, row.event_id, states, None, detail)
        .await
        .map_err(|error| error.to_string())
}

/// Ends an event as failed: the closing fence first, then exactly its original leaves the
/// owner, then the end is recorded. A crash in between leaves the fence for the next settlement.
async fn fail(
    pool: &PgPool,
    mailbox: Option<&ChannelMailboxHandle>,
    row: &Unsettled,
    reason: &str,
) -> Result<(), String> {
    use AttemptState::{Closing, Failed, MayHaveSent};
    let fenced = match row.state {
        Closing => true,
        MayHaveSent => step(pool, row, (MayHaveSent, Closing), Some(reason)).await?,
        _ => (decisions::close(pool, row.event_id, reason).await).map_err(|e| e.to_string())?,
    };
    if fenced {
        discard(mailbox, row).await?;
        step(pool, row, (Closing, Failed), None).await?;
    }
    Ok(())
}

/// Closes an event its first attempt could not hand over while the owner may hold its original.
pub(crate) async fn close_admitted(
    pool: &PgPool,
    (event_id, origin, channel_id): (Uuid, &str, u64),
    reason: &str,
) -> Result<(), String> {
    note_open();
    let row = Unsettled {
        event_id,
        origin: origin.to_string(),
        agent_id: String::new(),
        channel_id,
        state: AttemptState::Resolving,
        stale: false,
    };
    fail(pool, global_mailbox(channel_id).as_ref(), &row, reason).await
}

/// Settles one event from its owner's records and its turn's receipt; `written` is the result
/// of the attempt that just ran. A waiting or still running event is left as it is.
async fn settle(
    pool: &PgPool,
    mailbox: Option<&ChannelMailboxHandle>,
    row: &Unsettled,
    written: Option<&Result<(), String>>,
) -> Result<(), String> {
    use AttemptState::{
        Closing, Delivered, MayHaveSent, NormalWait, Normalizing, Resolving, UnknownClosed,
    };
    let held = held(mailbox, row).await;
    let journaled = matches!(row.state, NormalWait | MayHaveSent);
    let receipt = match journaled {
        true => receipt(pool, row).await,
        false => None,
    };
    let sent =
        matches!(written, Some(Ok(()))) || held == Some(Held::Delivered) || receipt == Some(true);
    let proven_unsent = written.and_then(|w| w.as_ref().err()).filter(|e| unsent(e));
    match (row.state, held) {
        (Closing, _) => fail(pool, mailbox, row, "").await,
        (Resolving | Normalizing, _) if row.stale => fail(pool, mailbox, row, INTERRUPTED).await,
        (NormalWait | MayHaveSent, _) if sent => step(pool, row, (row.state, Delivered), None)
            .await
            .map(drop),
        (NormalWait | MayHaveSent, Some(Held::NotSent(reason))) => {
            fail(pool, mailbox, row, &reason).await
        }
        (NormalWait, Some(Held::Gone)) => fail(pool, mailbox, row, "original_removed").await,
        (MayHaveSent, Some(Held::InFlight)) => Ok(()),
        // The owner put the original back on a proof that no key was sent: it waits again.
        (MayHaveSent, Some(Held::Waiting)) if proven_unsent.is_some() => {
            step(pool, row, (MayHaveSent, NormalWait), None)
                .await
                .map(drop)
        }
        (MayHaveSent, Some(Held::Gone)) if proven_unsent.is_some() => {
            fail(pool, mailbox, row, proven_unsent.map_or("", String::as_str)).await
        }
        // No proof either way: a journal an earlier process wrote, a write the owner never
        // confirmed or a turn that ended incomplete closes without another write.
        (MayHaveSent, held)
            if row.stale || held == Some(Held::Unconfirmed) || receipt == Some(false) =>
        {
            if step(pool, row, (MayHaveSent, UnknownClosed), None).await? {
                discard(mailbox, row).await?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Runs the owner's idle handoff of `front`. A Decision front is journaled in PG first: a closed
/// event's original is removed unrun, and the handoff's result settles the event.
#[cfg(unix)]
pub(crate) async fn hand_idle(
    pool: Option<&PgPool>,
    mailbox: &ChannelMailboxHandle,
    context: Option<&serde_json::Value>,
    front: Front,
    idle: &Handoff,
) -> Result<(), String> {
    use AttemptState::{Closing, Delivered, Failed, MayHaveSent, UnknownClosed};
    let Some(origin) = origin_of(context) else {
        return idle(front).await;
    };
    note_open();
    let unavailable = || "NotSent(\"decision_store_unavailable\")".to_string();
    let (Some(pool), Some(event_id)) = (pool, decisions::event_of_origin(origin)) else {
        return Err(unavailable());
    };
    let mut row = Unsettled {
        event_id,
        origin: origin.to_string(),
        agent_id: String::new(),
        channel_id: front.1.channel_id.unwrap_or_default(),
        state: MayHaveSent,
        stale: false,
    };
    match decisions::begin_write(pool, event_id).await {
        Ok(true) => {}
        // Closed, or journaled by an attempt that is gone: this original never runs.
        Ok(false) => {
            row.stale = true;
            match decisions::state_of(pool, event_id).await {
                Ok(Some(state @ (Closing | MayHaveSent))) => {
                    row.state = state;
                    settle(pool, Some(mailbox), &row, None).await?;
                }
                Ok(Some(Delivered | Failed | UnknownClosed)) => {
                    discard(Some(mailbox), &row).await?;
                }
                _ => {}
            }
            return Err(CLOSED.into());
        }
        Err(_) => return Err(unavailable()),
    }
    let result = idle(front).await;
    if let Err(error) = settle(pool, Some(mailbox), &row, Some(&result)).await {
        tracing::warn!(%event_id, %error, "decision input left for its next settlement");
    }
    result
}

/// Settles every unsettled event of `channel` from its owner's records. The owner's notice drain
/// runs this first, so a failed Decision original is gone before any resend notice is posted.
pub(crate) async fn settle_channel(pool: &PgPool, mailbox: &ChannelMailboxHandle, channel: u64) {
    if !OPEN.load(Ordering::Relaxed) {
        return;
    }
    let rows = decisions::unsettled(pool, Some(channel), BOOT.get().copied()).await;
    for row in rows.unwrap_or_default() {
        if let Err(error) = settle(pool, Some(mailbox), &row, None).await {
            tracing::warn!(event_id = %row.event_id, %error, "decision input left unsettled");
        }
    }
}

/// Settles every row last written before `boot` without a second write; a waiting original
/// keeps waiting under its ticket.
async fn recover_before(
    pool: &PgPool,
    boot: chrono::DateTime<chrono::Utc>,
    mailbox_of: impl Fn(u64) -> Option<ChannelMailboxHandle>,
) -> Result<usize, sqlx::Error> {
    let rows = decisions::unsettled(pool, None, Some(boot)).await?;
    if !rows.is_empty() {
        note_open();
    }
    for row in &rows {
        let mailbox = mailbox_of(row.channel_id);
        if let Err(error) = settle(pool, mailbox.as_ref(), row, None).await {
            tracing::warn!(event_id = %row.event_id, %error, "decision input left unsettled");
        }
    }
    Ok(rows.len())
}

/// Boot recovery, called once before this process delivers any event; returns how many
/// unsettled rows it read. The first call fixes the instant that tells an earlier process's rows.
pub async fn recover_boot(pool: &PgPool) -> Result<usize, sqlx::Error> {
    let boot = match BOOT.get() {
        Some(boot) => *boot,
        None => {
            let now = decisions::clock(pool).await?;
            *BOOT.get_or_init(|| now)
        }
    };
    recover_before(pool, boot, global_mailbox).await
}

/// Runs the boot recovery beside a runtime's other start-up work; without PG there is no event.
pub(crate) fn spawn_boot_recovery(pool: Option<PgPool>) {
    let Some(pool) = pool else { return };
    tokio::spawn(async move {
        if let Err(error) = recover_boot(&pool).await {
            tracing::warn!(%error, "decision boot recovery did not run");
        }
    });
}

fn global_mailbox(channel: u64) -> Option<ChannelMailboxHandle> {
    let channel = poise::serenity_prelude::ChannelId::new(channel.max(1));
    ChannelMailboxRegistry::global_handle(channel)
}

#[cfg(test)]
#[path = "decision_drain_tests.rs"]
mod tests;
