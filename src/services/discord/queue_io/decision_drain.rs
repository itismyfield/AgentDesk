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

pub(crate) mod queued;

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
/// This process's run: its node and its start on the database clock, fixed once.
static RUN: OnceLock<decisions::Run> = OnceLock::new();

/// Fixes this run's start before the process moves any event; every entry asks here first.
/// Until the cluster bootstrap registered this node's name there is no run and nothing moves.
pub(crate) async fn run(pool: &PgPool) -> Result<&'static decisions::Run, sqlx::Error> {
    let Some(node) = node() else {
        let unnamed = "decision holder is not registered yet";
        return Err(sqlx::Error::Protocol(unnamed.into()));
    };
    if let Some(run) = RUN.get() {
        return Ok(run);
    }
    let boot = decisions::clock(pool).await?;
    Ok(RUN.get_or_init(|| decisions::Run::of(&node, boot)))
}

#[cfg(test)]
thread_local! {
    /// Hides this node's registration from the test's thread, as before the cluster bootstrap.
    static UNREGISTERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The name this node registered in the cluster's registry; a temporary name is never one.
fn node() -> Option<String> {
    #[cfg(test)]
    if UNREGISTERED.with(std::cell::Cell::get) {
        return None;
    }
    crate::services::cluster::node_registry::SELF_INSTANCE_ID
        .get()
        .cloned()
}

/// Marks that approved events exist, so the owner's drain settles them from now on.
pub(crate) fn note_open() {
    OPEN.store(true, Ordering::Relaxed);
}

const INTERRUPTED: &str = "interrupted_before_delivery";
/// The owner's start found its Bound session's execution absent from the Herdr server.
pub(crate) const MISSING: &str = "NotSent(\"herdr_execution_missing\")";
#[cfg(unix)]
const CLOSED: &str = "NotSent(\"decision_closed\")";
#[cfg(unix)]
const UNAPPROVED: &str = "NotSent(\"decision_not_approved\")";

/// How a queued original's kept context relates to the Decision source.
#[cfg(unix)]
enum Claim<'a> {
    /// Another source: the owner's ordinary handoff takes it.
    Other,
    /// The Decision source without an origin that names an event: it is never written.
    Unfounded,
    Event(Uuid, &'a str),
}

#[cfg(unix)]
fn claim_of(context: Option<&serde_json::Value>) -> Claim<'_> {
    let Some(context) = context else {
        return Claim::Other;
    };
    let origin = (context.pointer("/metadata/human_input/origin_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|_| ChannelMailboxHandle::decision_input(context));
    match origin.and_then(|origin| Some((decisions::event_of_origin(origin)?, origin))) {
        Some((event, origin)) => Claim::Event(event, origin),
        None if self_declared(context["source"].as_str(), context.get("metadata")) => {
            Claim::Unfounded
        }
        None => Claim::Other,
    }
}

/// Removes a front that claims the Decision source without its event's approval: nothing of it
/// is written and no event moves. The id alone names it, since its origin proves nothing.
#[cfg(unix)]
async fn refuse_unapproved(mailbox: &ChannelMailboxHandle, front: &Front) -> Result<(), String> {
    let message = front.1.message_id;
    let removed = mailbox.v2_request(move |slot| match slot.as_mut() {
        Some(owner) => owner.discard_decision(message, "decision:"),
        None => Ok(()),
    });
    if let Err(error) = removed.await {
        tracing::warn!(message_id = message, %error, "unapproved decision front left queued");
    }
    Err(UNAPPROVED.into())
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

/// Ends a row whose holder's run PG shows over. The update checks that again, under `run` or
/// this process's own; without a fixed run nothing proves the holder gone and the row stays.
async fn end_abandoned(
    pool: &PgPool,
    row: &Unsettled,
    states: (AttemptState, AttemptState),
    detail: Option<&str>,
    run: Option<&decisions::Run>,
) -> Result<bool, String> {
    let Some(run) = run.or(RUN.get()) else {
        return Ok(false);
    };
    decisions::end_abandoned(pool, row.event_id, states, detail, run)
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

/// Settles one event under this process's own run.
async fn settle(
    pool: &PgPool,
    mailbox: Option<&ChannelMailboxHandle>,
    row: &Unsettled,
    written: Option<&Result<(), String>>,
) -> Result<(), String> {
    settle_as(pool, mailbox, row, written, None).await
}

/// Settles one event from its owner's records and its turn's receipt; `written` is the attempt
/// that just ran. A waiting or running event stays, and a stale one ends only under `run`.
async fn settle_as(
    pool: &PgPool,
    mailbox: Option<&ChannelMailboxHandle>,
    row: &Unsettled,
    written: Option<&Result<(), String>>,
    run: Option<&decisions::Run>,
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
        (Resolving | Normalizing, _) if row.stale => {
            let fence = (row.state, Closing);
            if end_abandoned(pool, row, fence, Some(INTERRUPTED), run).await? {
                discard(mailbox, row).await?;
                step(pool, row, (Closing, AttemptState::Failed), None).await?;
            }
            Ok(())
        }
        (NormalWait | MayHaveSent, _) if sent => step(pool, row, (row.state, Delivered), None)
            .await
            .map(drop),
        (NormalWait | MayHaveSent, Some(Held::NotSent(reason))) => {
            fail(pool, mailbox, row, &reason).await
        }
        (NormalWait, Some(Held::Gone)) => fail(pool, mailbox, row, "original_removed").await,
        (MayHaveSent, Some(Held::InFlight)) => Ok(()),
        // The session's execution is confirmed gone: no turn boundary will ever come for it.
        (MayHaveSent, Some(Held::Waiting)) if proven_unsent.is_some_and(|e| e == MISSING) => {
            fail(pool, mailbox, row, "herdr_execution_missing").await
        }
        // The owner put the original back on a proof that no key was sent: it waits again.
        (MayHaveSent, Some(Held::Waiting)) if proven_unsent.is_some() => {
            step(pool, row, (MayHaveSent, NormalWait), None)
                .await
                .map(drop)
        }
        (MayHaveSent, Some(Held::Gone)) if proven_unsent.is_some() => {
            fail(pool, mailbox, row, proven_unsent.map_or("", String::as_str)).await
        }
        // No proof either way: a write the owner never confirmed, a turn that ended incomplete
        // or a journal whose holder's run is over closes without another write.
        (MayHaveSent, held)
            if row.stale || held == Some(Held::Unconfirmed) || receipt == Some(false) =>
        {
            let end = (MayHaveSent, UnknownClosed);
            let closed = match held == Some(Held::Unconfirmed) || receipt == Some(false) {
                true => step(pool, row, end, None).await?,
                false => end_abandoned(pool, row, end, None, run).await?,
            };
            if closed {
                discard(mailbox, row).await?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Runs the owner's idle handoff of `front`. A Decision front is first checked against its PG
/// event and journaled: an unapproved or closed one leaves unrun, and the result settles it.
#[cfg(unix)]
pub(crate) async fn hand_idle(
    pool: Option<&PgPool>,
    mailbox: &ChannelMailboxHandle,
    context: Option<&serde_json::Value>,
    front: Front,
    idle: &Handoff,
) -> Result<(), String> {
    use AttemptState::{Closing, Delivered, Failed, MayHaveSent, UnknownClosed};
    let (event_id, origin) = match claim_of(context) {
        Claim::Other => return idle(front).await,
        Claim::Unfounded => return refuse_unapproved(mailbox, &front).await,
        Claim::Event(event_id, origin) => (event_id, origin),
    };
    note_open();
    let unavailable = || "NotSent(\"decision_store_unavailable\")".to_string();
    let Some(pool) = pool else {
        return Err(unavailable());
    };
    if run(pool).await.is_err() {
        return Err(unavailable());
    }
    // The journal below is taken only for the input PG approved under this very origin.
    let approved = match decisions::load_event(pool, event_id).await {
        Ok(event) => event.filter(|event| {
            event.origin() == origin
                && front.1.channel_id == Some(event.channel_id())
                && front.1.author_id == event.actor_id()
                && event.approves(&front.1.text)
        }),
        Err(_) => return Err(unavailable()),
    };
    let Some(approved) = approved else {
        return refuse_unapproved(mailbox, &front).await;
    };
    let mut row = Unsettled {
        event_id,
        origin: origin.to_string(),
        agent_id: String::new(),
        channel_id: approved.channel_id(),
        state: MayHaveSent,
        stale: false,
    };
    match decisions::begin_write(pool, event_id).await {
        Ok(true) => {}
        // Closed, or journaled by an attempt that is gone: this original never runs.
        Ok(false) => {
            match decisions::state_of(pool, event_id).await {
                Ok(Some(Closing)) => {
                    row.state = Closing;
                    settle(pool, Some(mailbox), &row, None).await?;
                }
                // This owner still holds the original unattempted, so the attempt that
                // journaled it is gone; only that turn's own receipt can call it delivered.
                Ok(Some(MayHaveSent)) => {
                    let to = match receipt(pool, &row).await {
                        Some(true) => Delivered,
                        _ => UnknownClosed,
                    };
                    step(pool, &row, (MayHaveSent, to), None).await?;
                    discard(Some(mailbox), &row).await?;
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

/// Settles the channel's open events, then removes each original its owner still holds of an
/// event PG closed. Returns the originals whose removal failed: only their notices wait.
pub(crate) async fn settle_channel(
    pool: &PgPool,
    mailbox: &ChannelMailboxHandle,
    channel: u64,
) -> Vec<u64> {
    let tickets = mailbox.v2_request(|slot| {
        Ok((slot.as_ref()).map_or_else(Vec::new, |owner| owner.decision_tickets()))
    });
    let tickets = tickets.await.unwrap_or_default();
    // Without an approved event met here or a Decision original held, nothing reads PG.
    if !OPEN.load(Ordering::Relaxed) && tickets.is_empty() {
        return Vec::new();
    }
    let rows = decisions::unsettled(pool, Some(channel), RUN.get()).await;
    for row in rows.unwrap_or_default() {
        if let Err(error) = settle(pool, Some(mailbox), &row, None).await {
            tracing::warn!(event_id = %row.event_id, %error, "decision input left unsettled");
        }
    }
    if tickets.is_empty() {
        return Vec::new();
    }
    let closed = decisions::closed_under(pool, &tickets).await;
    let mut owed = Vec::new();
    for (origin, channel_id) in closed.unwrap_or_default() {
        let row = Unsettled {
            event_id: Uuid::nil(),
            origin,
            agent_id: String::new(),
            channel_id,
            state: AttemptState::Failed,
            stale: false,
        };
        if let Err(error) = discard(Some(mailbox), &row).await {
            tracing::warn!(origin = %row.origin, %error, "closed decision original still held");
            owed.push(message_of(row.channel_id, &row.origin));
        }
    }
    owed
}

/// Settles, without a second write, every row whose holder's run is over for this node's run
/// started at `boot`; a waiting original keeps waiting under its ticket.
async fn recover_before(
    pool: &PgPool,
    boot: chrono::DateTime<chrono::Utc>,
    mailbox_of: impl Fn(u64) -> Option<ChannelMailboxHandle>,
) -> Result<usize, sqlx::Error> {
    let Some(node) = node() else {
        return Ok(0);
    };
    let run = decisions::Run::of(&node, boot);
    let rows = decisions::unsettled(pool, None, Some(&run)).await?;
    if !rows.is_empty() {
        note_open();
    }
    for row in &rows {
        let mailbox = mailbox_of(row.channel_id);
        if let Err(error) = settle_as(pool, mailbox.as_ref(), row, None, Some(&run)).await {
            tracing::warn!(event_id = %row.event_id, %error, "decision input left unsettled");
        }
    }
    Ok(rows.len())
}

/// Boot recovery; returns how many unsettled rows it read. It runs beside the first deliveries:
/// the run's start is fixed by whichever asks first, so it never ends a row this process moved.
pub async fn recover_boot(pool: &PgPool) -> Result<usize, sqlx::Error> {
    let boot = run(pool).await?.boot;
    recover_before(pool, boot, global_mailbox).await
}

/// Runs the boot recovery beside a runtime's other start-up work; without PG there is no event.
pub(crate) fn spawn_boot_recovery(pool: Option<PgPool>) {
    settle_on_offline();
    let Some(pool) = pool else { return };
    tokio::spawn(async move {
        // Whose attempts are abandoned is judged under this node's registered name.
        while node().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        if let Err(error) = recover_boot(&pool).await {
            tracing::warn!(%error, "decision boot recovery did not run");
        }
    });
}

/// Has the registry's offline listing settle what those nodes held; asked at a runtime's start.
pub(crate) fn settle_on_offline() {
    fn settle(pool: PgPool) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(async move { settle_abandoned(&pool).await })
    }
    let _ = crate::services::cluster::node_registry::ON_NODES_OFFLINE.set(settle);
}

/// Settles every unsettled attempt from its evidence, without another write: asked when the
/// registry lists a node offline, and by the selection pass for attempts on idle channels.
pub(crate) async fn settle_abandoned(pool: &PgPool) {
    let settled = match run(pool).await {
        Ok(run) => recover_before(pool, run.boot, global_mailbox).await,
        Err(error) => Err(error),
    };
    if let Err(error) = settled {
        tracing::warn!(%error, "decision attempts of an offline node left unsettled");
    }
}

fn global_mailbox(channel: u64) -> Option<ChannelMailboxHandle> {
    let channel = poise::serenity_prelude::ChannelId::new(channel.max(1));
    ChannelMailboxRegistry::global_handle(channel)
}

#[cfg(test)]
#[path = "decision_drain_tests.rs"]
mod tests;
