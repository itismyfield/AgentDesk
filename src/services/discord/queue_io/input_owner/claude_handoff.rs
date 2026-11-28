//! A mapped Claude Legacy front closes its old pane before the ordinary idle launch claims it.
#![cfg(unix)]
use super::drive::Front;
use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
use crate::services::claude::session_handoff::{
    EarlierExecution, LegacyTmux, Liveness, pinned_tmux,
};
use crate::services::claude_tui::host_input::busy::{self, BusyObservation, BusyState};
use crate::services::provider::ProviderKind;
use crate::services::turn_host::{HerdrTurnPlan, TurnHost};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, HandoffHeld as Held};
use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};
type Shared = Arc<crate::services::discord::SharedData>;
type Channel = poise::serenity_prelude::ChannelId;
static WAITING: LazyLock<Mutex<HashSet<(u64, String)>>> = LazyLock::new(Default::default);
struct Waiting((u64, String));
impl Drop for Waiting {
    fn drop(&mut self) {
        WAITING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}
pub(in crate::services::discord) const HELD: &str = "NotSent(\"claude_handoff_held\")";
const RETAINED: &str = "NotSent(\"owner_start_failed\")";

/// What a front's turn runs under, as the context kept with it tells.
pub(crate) enum Start<'a> {
    /// A person's input, which runs in the channel's own session and alone opens a handoff.
    Person,
    /// A routine, scheduled or HTTP start; its kept context names the session it runs under.
    Kept(&'a serde_json::Value),
}
/// A kept start's session against the journal's.
#[derive(Clone, Copy, PartialEq)]
enum Target {
    Same,
    Other,
    Unresolved,
}

pub(crate) async fn prepare(
    shared: Option<&Shared>,
    mailbox: &ChannelMailboxHandle,
    provider: &ProviderKind,
    channel: u64,
    front: &Front,
    start: Start<'_>,
) -> Result<bool, String> {
    let Some(shared) = shared.filter(|_| *provider == ProviderKind::Claude) else {
        return Ok(false);
    };
    let journal = mailbox
        .v2_request(|slot| Ok(slot.as_ref().and_then(|owner| owner.handoff())))
        .await?;
    // Only a person's input opens a handoff; any other front is judged under one that stands.
    let person = matches!(start, Start::Person);
    if journal.is_none() && !person {
        return Ok(false);
    }
    let channel = Channel::new(channel);
    let left = journal.as_ref().filter(|_| left_herdr(channel.get()));
    if let Some(id) = left.map(|j| j.id.clone())
        && abandon(mailbox, id).await?
    {
        tracing::info!(
            channel_id = channel.get(),
            "handoff journal dropped: channel left Herdr"
        );
        return Ok(false);
    }
    let target = match (&journal, start) {
        (Some(journal), Start::Kept(context)) => {
            target(shared, channel, context, &journal.session_key).await
        }
        _ => Target::Same,
    };
    // An ended journal owes a front of another session nothing: it is finished before that front
    // waits on the deadline or the host of a session it does not run under.
    if let Some(journal) = &journal
        && journal.ended
        && target == Target::Other
        && finish(mailbox, &journal.id).await?
    {
        return Ok(false);
    }
    if let Some(journal) = &journal
        && journal.retry_after_ms > now_ms()
    {
        return hold(
            shared,
            mailbox,
            channel,
            front,
            &journal.id,
            journal.failures,
        )
        .await;
    }
    if let Some(journal) = left {
        return spend(shared, mailbox, channel, front, &journal.id, Held::Left).await;
    }
    let refused = journal.as_ref().map(|journal| journal.id.clone());
    let judged = judge(shared, provider, channel, journal.is_some()).await;
    let (key, host) = match judged {
        Ok(Some(target)) => target,
        Ok(None) => return Ok(false),
        Err(_) => return refuse(shared, mailbox, channel, front, refused).await,
    };
    let TurnHost::Herdr(plan) = host else {
        return match refused {
            Some(id) => spend(shared, mailbox, channel, front, &id, Held::Refused).await,
            None => Ok(false),
        };
    };
    if let Some(record) = plan.row.as_ref().and_then(|row| match &row.record {
        HostedRecord::Known(record) if record.state == HostedState::Bound => Some(record),
        _ => None,
    }) {
        let Some(journal) = journal else {
            return Ok(false);
        };
        // A Bound row completes the ended journal that names it; no other journal passes it.
        let named = journal.ended && journal.logical == record.owner.logical_key;
        if named && (finish(mailbox, &journal.id).await? || target == Target::Same) {
            return Ok(false);
        }
        return spend(shared, mailbox, channel, front, &journal.id, Held::Refused).await;
    }
    if journal.is_none()
        && !matches!(
            plan.row.as_ref().map(|row| &row.record),
            Some(HostedRecord::Legacy) | None
        )
    {
        return Ok(false);
    }
    let identity = crate::services::discord::session_identity::SessionIdentity::parse(&key);
    let Some(logical) = identity.map(|identity| identity.tmux_name) else {
        return refuse(shared, mailbox, channel, front, refused).await;
    };
    if let Some(journal) = &journal
        && (journal.logical != logical || journal.session_key != key)
    {
        return spend(shared, mailbox, channel, front, &journal.id, Held::Refused).await;
    }
    let journal = match journal.filter(|_| !person) {
        Some(journal) => journal,
        None => {
            let (begin, name, stored_key) = (front.clone(), logical.clone(), key.clone());
            let begun = mailbox.v2_request(move |slot| {
                slot.as_mut()
                    .ok_or("owner missing")?
                    .begin_handoff(&begin, name, stored_key)
            });
            begun.await?
        }
    };
    let transition = shared
        .session_transition_locks
        .lock(channel)
        .try_lock_owned();
    let Ok(transition) = transition else {
        return hold(
            shared,
            mailbox,
            channel,
            front,
            &journal.id,
            journal.failures,
        )
        .await;
    };
    let ended = if journal.ended {
        true
    } else {
        let pin = journal.pin.clone();
        let pin = pin.map(serde_json::from_value::<pinned_tmux::Pin>);
        let Ok(pin) = pin.transpose() else {
            return spend(shared, mailbox, channel, front, &journal.id, Held::Refused).await;
        };
        let (actor, original, id) = (mailbox.clone(), front.clone(), journal.id.clone());
        let authorize = move |pin| {
            let (actor, original, id) = (actor.clone(), original.clone(), id.clone());
            async move {
                let pin = serde_json::to_value(pin).map_err(|e| e.to_string())?;
                actor
                    .v2_request(move |slot| {
                        slot.as_mut()
                            .ok_or("owner missing")?
                            .handoff_pin(&id, &original, pin)
                    })
                    .await
            }
        };
        let result = close_legacy(channel.get(), &logical, pin.as_ref(), authorize).await;
        match result {
            Ok(()) => {
                let id = journal.id.clone();
                mailbox
                    .v2_request(move |slot| {
                        slot.as_mut().ok_or("owner missing")?.handoff_ended(&id)
                    })
                    .await?
            }
            Err(error) => {
                tracing::info!(channel_id=channel.get(), %error, "Claude handoff retains custody while closing old pane");
                let (id, failures, acked) =
                    count(mailbox, channel, front, &journal.id, Held::Close).await?;
                let noticed = notice(shared, mailbox, channel, (&id, failures, acked));
                let limit = std::time::Duration::from_secs(5);
                if tokio::time::timeout(limit, noticed).await.is_err() {
                    tracing::warn!(channel_id = channel.get(), "handoff retry notice timed out");
                }
                return hold(shared, mailbox, channel, front, &id, failures).await;
            }
        }
    };
    drop(transition);
    let waited = journal.failures.saturating_add(1);
    if !ended {
        return hold(shared, mailbox, channel, front, &journal.id, waited).await;
    }
    // The old pane ended: a front of another session finishes the journal and takes its own path.
    match target {
        Target::Same => {}
        Target::Other if finish(mailbox, &journal.id).await? => return Ok(false),
        _ => return spend(shared, mailbox, channel, front, &journal.id, Held::Refused).await,
    }
    if mailbox.has_active_turn().await.unwrap_or(true) {
        return hold(shared, mailbox, channel, front, &journal.id, waited).await;
    }
    Ok(true)
}

/// Whether a kept start runs under the session `journal_key` names, read as its turn builds it.
async fn target(
    shared: &Shared,
    channel: Channel,
    context: &serde_json::Value,
    journal_key: &str,
) -> Target {
    let key = shared.router_ports.owner_start_session_key;
    match key(shared, channel, context).await {
        None => Target::Unresolved,
        Some(key) if key == journal_key => Target::Same,
        Some(_) => Target::Other,
    }
}
/// Drops the journal once its original is no longer queued; whether none is left to refuse the
/// front's reserve.
async fn finish(mailbox: &ChannelMailboxHandle, id: &str) -> Result<bool, String> {
    let id = id.to_string();
    mailbox
        .v2_request(move |slot| slot.as_mut().ok_or("owner missing")?.finish_handoff(&id))
        .await
}

async fn judge(
    shared: &Shared,
    provider: &ProviderKind,
    channel: Channel,
    continuing: bool,
) -> Result<Option<(String, TurnHost)>, String> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(target) = tests::judged(channel.get()) {
        return Ok(Some(target));
    }
    if !continuing
        && (!crate::services::turn_host::herdr_turn_switched_on()
            || !crate::services::session_host::thread_host::next_is_herdr(channel.get()))
    {
        return Ok(None);
    }
    let key = crate::services::discord::adk_session::turn_session_key(shared, channel, provider)
        .await
        .ok_or(HELD)?;
    let host = crate::services::turn_host::for_turn(
        shared.pg_pool.as_ref(),
        provider,
        channel.get(),
        Some(&key),
    )
    .await;
    Ok(Some((key, host)))
}
/// Whether host policy no longer routes the channel to Herdr, so a journal has no handoff target.
fn left_herdr(channel: u64) -> bool {
    #[cfg(any(test, feature = "test-support"))]
    if tests::judged(channel).is_some() {
        return false;
    }
    crate::services::session_host::thread_host::left_herdr(channel)
}
async fn abandon(mailbox: &ChannelMailboxHandle, id: String) -> Result<bool, String> {
    mailbox
        .v2_request(move |slot| slot.as_mut().ok_or("owner missing")?.abandon_handoff(&id))
        .await
}
async fn close_legacy<F, Fut>(
    channel: u64,
    logical: &str,
    pin: Option<&pinned_tmux::Pin>,
    authorize: F,
) -> Result<(), String>
where
    F: FnMut(pinned_tmux::Pin) -> Fut,
    Fut: std::future::Future<Output = Result<bool, String>>,
{
    #[cfg(any(test, feature = "test-support"))]
    if let Some(result) = tests::close_result(channel) {
        return result;
    }
    match LegacyTmux(logical).liveness() {
        Liveness::Ended if pin.is_none() => Ok(()),
        _ => pinned_tmux::end(logical, channel, pin, authorize).await,
    }
}

fn front_tag(front: &Front) -> String {
    format!("front:{}:{}", front.1.message_id, front.1.queued_generation)
}

pub(in crate::services::discord) fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// Retry only after wrapper row and token cleanup; unrelated wakes still check the durable deadline.
pub(crate) async fn retry_start(shared: &Shared, channel: Channel) -> Result<bool, String> {
    let journal = shared
        .mailbox(channel)
        .v2_request(|slot| Ok(slot.as_ref().and_then(|owner| owner.handoff())))
        .await?;
    if let Some(journal) = journal.filter(|j| j.retry_after_ms > 0) {
        if journal.retry_after_ms <= now_ms() {
            recover(shared.clone(), channel);
            return Ok(false);
        }
        schedule(shared.clone(), channel, journal.id, journal.failures);
        return Ok(true);
    }
    Ok(false)
}

async fn hold(
    shared: &Shared,
    mailbox: &ChannelMailboxHandle,
    channel: Channel,
    front: &Front,
    id: &str,
    failures: u32,
) -> Result<bool, String> {
    let original = front.clone();
    mailbox
        .v2_request(move |slot| slot.as_mut().ok_or("owner missing")?.hold_front(&original))
        .await?;
    schedule(shared.clone(), channel, id.to_string(), failures);
    Err(HELD.into())
}
/// Counts one hold of `front` under the journal and returns its id, count and notice state; the
/// hold that spends the budget retains the front as a visible start failure and ends this look.
async fn count(
    mailbox: &ChannelMailboxHandle,
    channel: Channel,
    front: &Front,
    id: &str,
    how: Held,
) -> Result<(String, u32, bool), String> {
    let (original, journal) = (front.clone(), id.to_string());
    let held = mailbox.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("owner missing")?;
        let held = owner.handoff_held(&original, &journal, how)?;
        Ok(held.map(|held| (held.id, held.failures, held.notice_acked)))
    });
    held.await?.ok_or_else(|| retained(channel, front))
}
fn retained(channel: Channel, front: &Front) -> String {
    tracing::warn!(
        channel_id = channel.get(),
        front_id = front.1.message_id,
        "handoff hold budget spent; front retained"
    );
    RETAINED.into()
}
/// A hold no later look of this journal clears by itself: counted and backed off.
async fn spend(
    shared: &Shared,
    mailbox: &ChannelMailboxHandle,
    channel: Channel,
    front: &Front,
    id: &str,
    how: Held,
) -> Result<bool, String> {
    let (id, failures, _) = count(mailbox, channel, front, id, how).await?;
    schedule(shared.clone(), channel, id, failures);
    Err(HELD.into())
}
/// A front whose host cannot be judged: under a journal it spends that budget, and with none
/// the ordinary start budget of one retry.
async fn refuse(
    shared: &Shared,
    mailbox: &ChannelMailboxHandle,
    channel: Channel,
    front: &Front,
    journal: Option<String>,
) -> Result<bool, String> {
    if let Some(id) = journal {
        return spend(shared, mailbox, channel, front, &id, Held::Refused).await;
    }
    let original = front.clone();
    let held = mailbox.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("owner missing")?;
        owner.unjudged_held(&original)
    });
    if held.await? {
        return Err(retained(channel, front));
    }
    schedule(shared.clone(), channel, front_tag(front), 1);
    Err(HELD.into())
}
pub(in crate::services::discord) fn schedule(
    shared: Shared,
    channel: Channel,
    id: String,
    failures: u32,
) {
    let key = (channel.get(), id.clone());
    if !WAITING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.clone())
    {
        return;
    }
    let waiting = Waiting(key.clone());
    tokio::spawn(async move {
        let checked = id.clone();
        let deadline = shared
            .mailbox(channel)
            .v2_request(move |slot| {
                Ok(slot
                    .as_ref()
                    .and_then(|owner| owner.handoff())
                    .filter(|j| j.id == checked)
                    .map(|j| j.retry_after_ms))
            })
            .await
            .ok()
            .flatten()
            .filter(|at| *at > 0);
        // A later hold must not reuse an expired launch deadline as a zero-delay retry.
        let now = now_ms();
        let delay = deadline
            .filter(|at| *at > now)
            .map_or(1_000u64 << failures.min(5), |at| at - now);
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        drop(waiting);
        let checked = id.clone();
        let live = shared
            .mailbox(channel)
            .v2_request(move |slot| {
                Ok(slot.as_ref().and_then(|owner| {
                    owner
                        .handoff()
                        .filter(|j| j.id == checked)
                        .map(|j| j.retry_after_ms)
                        .or_else(|| {
                            owner
                                .front()
                                .filter(|front| front_tag(front) == checked)
                                .map(|_| 0)
                        })
                }))
            })
            .await;
        match live {
            Ok(Some(at)) if at > now_ms() => schedule(shared, channel, id, failures),
            Ok(Some(_)) => {
                recover(shared.clone(), channel);
                (shared.router_ports.owner_wake)(&shared, channel);
            }
            Err(_) => schedule(shared, channel, id, failures.saturating_add(1)),
            Ok(None) => (),
        }
    });
}
async fn notice(
    shared: &Shared,
    mailbox: &ChannelMailboxHandle,
    channel: Channel,
    (id, failures, acked): (&str, u32, bool),
) {
    if failures < 2 || acked {
        return;
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
        return;
    };
    use crate::services::message_outbox::{
        LIFECYCLE_NOTIFIER_SOURCE, OutboxMessage,
        enqueue_outbox_pg_returning_id_with_persistent_dedupe,
    };
    let key = format!("claude_handoff:{id}:retry");
    let result = enqueue_outbox_pg_returning_id_with_persistent_dedupe(pool, OutboxMessage {
        target: &format!("channel:{}", channel.get()), content: "이전 세션 종료 재시도 중입니다. 입력은 보관 중이며 전환 뒤 자동으로 전달합니다.", bot: "claude", source: LIFECYCLE_NOTIFIER_SOURCE,
        reason_code: Some("lifecycle.handoff_retry"), session_key: Some(&key),
    }).await;
    if result.is_ok() {
        let id = id.to_string();
        if let Err(error) = mailbox
            .v2_request(move |slot| {
                slot.as_mut()
                    .ok_or("owner missing")?
                    .handoff_notice_acked(&id)
            })
            .await
        {
            tracing::warn!(%error, "handoff notice acknowledgment pending");
        }
    }
}

/// An input-free journal still selects an actor at boot; a Bound row closes its remaining work.
pub(crate) fn recover(shared: Shared, channel: Channel) {
    tokio::spawn(async move {
        let mailbox = shared.mailbox(channel);
        let journal = mailbox
            .v2_request(|slot| Ok(slot.as_ref().and_then(|owner| owner.handoff())))
            .await;
        let Ok(Some(journal)) = journal else {
            return;
        };
        if left_herdr(channel.get()) && abandon(&mailbox, journal.id.clone()).await == Ok(true) {
            return;
        }
        let host = judge(&shared, &ProviderKind::Claude, channel, true).await;
        if let Ok(Some((_, TurnHost::Herdr(plan)))) = host
            && journal.ended
            && plan.row.as_ref().is_some_and(|row| matches!(&row.record, HostedRecord::Known(record) if record.state == HostedState::Bound && record.owner.logical_key == journal.logical)) {
            let id = journal.id;
            if let Err(error) = mailbox.v2_request(move |slot| slot.as_mut().ok_or("owner missing")?.finish_handoff(&id)).await { tracing::warn!(%error,"handoff completion journal held"); }
            #[cfg(any(test, feature = "test-support"))]
            tests::finished(channel.get());
        } else if journal.ended || journal.retry_after_ms > now_ms() {
            // An unended journal with no deadline pending waits for an input, never for a timer.
            schedule(shared, channel, journal.id, journal.failures);
        }
    });
}

/// A Bound Herdr row's gated pane, physical writer and attempt target.
pub(crate) fn bound_herdr(
    plan: &HerdrTurnPlan,
) -> Option<(
    crate::services::session_host::HerdrTarget,
    String,
    serde_json::Value,
)> {
    let HostedRecord::Known(record) = &plan.row.as_ref()?.record else {
        return None;
    };
    let at = record
        .location
        .as_ref()
        .filter(|_| record.state == HostedState::Bound)?;
    let pane = crate::services::session_host::herdr_endpoints().target(record)?;
    let target = serde_json::json!({"host":"herdr","owner":record.owner.logical_key,"root":at.socket_addr,"execution_nonce":record.execution_nonce});
    Some((pane, format!("{}#{}", at.socket_addr, at.pane_id), target))
}

/// Observes the supplied host without resolving a replacement execution.
pub(crate) fn observe_busy(
    channel: u64,
    logical: &str,
    host: &TurnHost,
    token_hash: &str,
) -> BusyObservation {
    let plan = match host {
        TurnHost::Tmux => return BusyObservation::only(BusyState::Legacy),
        TurnHost::Herdr(plan) => plan,
        // Lifecycle refusals and returns keep the existing driver's recovery policy.
        TurnHost::Return(_) | TurnHost::Refused(_) => {
            return BusyObservation::only(BusyState::Idle);
        }
    };
    let Some(row) = plan.row.as_ref() else {
        return BusyObservation::only(BusyState::Idle);
    };
    let record = match &row.record {
        HostedRecord::Unknown(_) => return BusyObservation::only(BusyState::Held),
        HostedRecord::Known(record) if record.state == HostedState::Bound => record,
        _ => return BusyObservation::only(BusyState::Idle),
    };
    let runtime_root = crate::config::runtime_root();
    if token_hash.is_empty()
        || record.owner.discord_token_hash != token_hash
        || record.owner.owner_node != plan.endpoint.execution_node
        || runtime_root.as_deref() != Some(std::path::Path::new(&record.owner.runtime_root))
        || record.owner.provider != "claude"
        || record.owner.logical_key != logical
        || record.owner.channel_id != channel.to_string()
    {
        return BusyObservation::only(BusyState::Held);
    }
    let Some(target) = bound_herdr(plan) else {
        return BusyObservation::only(BusyState::Held);
    };
    busy::observe_herdr(
        (channel, logical, &record.execution_nonce),
        target,
        busy_output_ready,
    )
}

fn busy_output_ready(channel: u64) -> bool {
    crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
        channel,
        Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
    ) == Ok(true)
        && crate::services::tui_o::writer::host::channel_accepts(channel)
}

#[cfg(any(test, feature = "test-support"))]
#[path = "claude_handoff_hook_tests.rs"]
pub(crate) mod tests;
