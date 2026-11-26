use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
use std::time::Duration;

use tokio::time::Instant;

use super::drive::{Drive, Front};
use super::origin::InputOrigin;
use crate::services::discord::{ChannelId, SharedData, session_transition::SessionTransitionLease};
use crate::services::discord::{queue_io::retained_notice, turn_lease};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

mod witness;

const BOUND: Duration = Duration::from_secs(180);
const LOOK: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(30);

struct Hold {
    mailbox: ChannelMailboxHandle,
    front: Front,
    since: Instant,
    runtime: Weak<SharedData>,
    provider: ProviderKind,
}

fn same_front(expected: &Front, current: &Front) -> bool {
    expected.0 == current.0
        && expected.1.message_id == current.1.message_id
        && expected.1.queued_generation == current.1.queued_generation
}

struct Clock {
    hold: Arc<Hold>,
    // None while the deadline task owns the observation window.
    due: Option<Instant>,
    declined: HashSet<&'static str>,
    waiting: bool,
}

fn clocks() -> MutexGuard<'static, HashMap<u64, Clock>> {
    static CLOCKS: LazyLock<Mutex<HashMap<u64, Clock>>> = LazyLock::new(Default::default);
    CLOCKS.lock().unwrap_or_else(|error| error.into_inner())
}

/// Keep the exact runtime with the normal drive's refusal without consulting a global registry.
pub(in crate::services::discord) fn attach(
    mut drive: Drive,
    runtime: Option<(Arc<SharedData>, ProviderKind)>,
) -> Arc<Drive> {
    if let Some((shared, provider)) = runtime
        .filter(|(_, provider)| matches!(provider, ProviderKind::Claude | ProviderKind::Codex))
    {
        let channel = drive.channel;
        let mailbox = drive.mailbox.clone();
        let handoff = drive.handoff;
        drive.handoff = Arc::new(move |front| {
            let (handoff, shared, provider, mailbox) = (
                handoff.clone(),
                shared.clone(),
                provider.clone(),
                mailbox.clone(),
            );
            Box::pin(async move {
                let result = handoff(front.clone()).await;
                if result.is_err() {
                    record(&shared, provider, channel, mailbox, front).await;
                }
                result
            })
        });
    }
    Arc::new(drive)
}

async fn record(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    channel: u64,
    mailbox: ChannelMailboxHandle,
    front: Front,
) {
    let since = Instant::now();
    if clocks().get(&channel).is_some_and(|clock| {
        clock.hold.mailbox.same_actor(&mailbox) && same_front(&clock.hold.front, &front)
    }) {
        return;
    }
    let expected = front.clone();
    let leads = mailbox
        .v2_request(move |slot| {
            let current = slot.as_ref().and_then(|owner| owner.front());
            Ok(current.is_some_and(|current| same_front(&expected, &current)))
        })
        .await;
    if leads != Ok(true) {
        return;
    }
    let context = match &front.0 {
        Some(ticket) => {
            mailbox
                .v2_admission_context(front.1.clone(), ticket.clone())
                .await
        }
        None => Ok(None),
    };
    if !context.is_ok_and(|context| InputOrigin::from_context(context.as_ref()).is_person())
        || !current_actor(shared, channel, &mailbox)
    {
        return;
    }
    clocks().insert(
        channel,
        Clock {
            hold: Arc::new(Hold {
                mailbox,
                front,
                since,
                runtime: Arc::downgrade(shared),
                provider,
            }),
            due: Some(since + BOUND),
            declined: HashSet::new(),
            waiting: false,
        },
    );
}

pub(super) fn sync_front(channel: u64, mailbox: &ChannelMailboxHandle, front: Option<&Front>) {
    let mut all = clocks();
    if all.get(&channel).is_some_and(|clock| {
        clock.hold.mailbox.same_actor(mailbox)
            && !front.is_some_and(|front| same_front(&clock.hold.front, front))
    }) {
        all.remove(&channel);
    }
}

pub(super) fn due(drive: &Drive) -> Option<Instant> {
    let all = clocks();
    let clock = all.get(&drive.channel)?;
    if !clock.hold.mailbox.same_actor(&drive.mailbox) {
        return None;
    }
    clock.due
}

fn current_actor(shared: &SharedData, channel: u64, mailbox: &ChannelMailboxHandle) -> bool {
    shared
        .mailbox_peek(ChannelId::new(channel))
        .is_some_and(|current| current.same_actor(mailbox))
}

enum Decline {
    Retry(&'static str, Option<&'static str>),
    Reset(&'static str),
    NoLease,
}

use Decline::{NoLease, Reset, Retry};

pub(super) async fn check(drive: &Drive) {
    let channel = drive.channel;
    let hold = {
        let mut all = clocks();
        let Some(clock) = all.get_mut(&channel) else {
            return;
        };
        if !clock.hold.mailbox.same_actor(&drive.mailbox)
            || !clock.due.is_some_and(|due| due <= Instant::now())
        {
            return;
        }
        clock.due = None;
        clock.hold.clone()
    };
    let Some(shared) = hold.runtime.upgrade() else {
        finish(channel, &hold);
        return;
    };
    match release(&shared, channel, &hold).await {
        Ok((expected, witness)) => {
            finish(channel, &hold);
            tracing::warn!(
                channel_id = channel,
                message_id = hold.front.1.message_id,
                shape = "lease",
                turn_nonce = %expected.turn_nonce,
                held_ms = hold.since.elapsed().as_millis() as u64,
                witness,
                provider = hold.provider.as_str(),
                settlement = "operator_released",
                "input_hold_bound_released"
            );
            notice(&shared, channel, &hold, None).await;
        }
        Err(Retry(reason, waiting)) => {
            let (declined, post) = {
                let mut all = clocks();
                let Some(clock) = all
                    .get_mut(&channel)
                    .filter(|c| Arc::ptr_eq(&c.hold, &hold))
                else {
                    return;
                };
                clock.due = Some(Instant::now() + RETRY);
                (
                    clock.declined.insert(reason),
                    waiting.filter(|_| !clock.waiting),
                )
            };
            if declined {
                declined_event(channel, &hold, reason);
            }
            if let Some(reason) = post
                && notice(&shared, channel, &hold, Some(reason)).await
            {
                let mut all = clocks();
                if let Some(clock) = all
                    .get_mut(&channel)
                    .filter(|c| Arc::ptr_eq(&c.hold, &hold))
                {
                    clock.waiting = true;
                }
                tracing::info!(
                    channel_id = channel,
                    message_id = hold.front.1.message_id,
                    reason,
                    "input_hold_bound_waiting"
                );
            }
        }
        Err(Reset(reason)) => {
            finish(channel, &hold);
            declined_event(channel, &hold, reason);
        }
        Err(NoLease) => finish(channel, &hold),
    }
}

fn finish(channel: u64, hold: &Arc<Hold>) {
    let mut all = clocks();
    if all
        .get(&channel)
        .is_some_and(|c| Arc::ptr_eq(&c.hold, hold))
    {
        all.remove(&channel);
    }
}

fn declined_event(channel: u64, hold: &Hold, reason: &'static str) {
    tracing::info!(
        channel_id = channel,
        message_id = hold.front.1.message_id,
        reason,
        "input_hold_bound_declined"
    );
}

async fn notice(shared: &SharedData, channel: u64, hold: &Hold, waiting: Option<&str>) -> bool {
    let Some(pool) = &shared.pg_pool else {
        return false;
    };
    let input = &hold.front.1;
    let waiting = waiting.map(|reason| match reason {
        "draft" => "화면 초안",
        "modal" => "모달",
        "running" => "작업 중",
        "unbound_message_identity" => "앞 턴의 메시지 식별자 없음",
        _ => "관측 실패",
    });
    match retained_notice::hold_bound(
        pool,
        hold.provider.as_str(),
        channel,
        input.message_id,
        &input.text,
        waiting,
    )
    .await
    {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(channel_id = channel, message_id = input.message_id, %error, "hold bound notice enqueue failed");
            false
        }
    }
}

async fn release(
    shared: &Arc<SharedData>,
    channel: u64,
    hold: &Arc<Hold>,
) -> Result<(turn_lease::LeaseIdentity, &'static str), Decline> {
    let id = ChannelId::new(channel);
    if !current_actor(shared, channel, &hold.mailbox) {
        return Err(Reset("incarnation_replaced"));
    }
    let _transition = SessionTransitionLease::try_acquire(shared.as_ref(), id)
        .map_err(|_| Retry("gate", None))?;
    let readiness_since = gate(shared, channel, hold).await?;
    let expected = turn_lease::identity_for_hold_bound(shared, &hold.provider, id, &hold.mailbox)
        .await
        .map_err(identity_decline)?
        .ok_or(NoLease)?;
    check_readiness_hold(readiness_since, &expected)?;
    if expected.user_message_id == 0 {
        return Err(Retry(
            "unbound_message_identity",
            Some("unbound_message_identity"),
        ));
    }

    let host = observe(shared, channel, &expected).await?;
    for _ in 1..3 {
        tokio::time::sleep(LOOK).await;
        check_readiness_hold(gate(shared, channel, hold).await?, &expected)?;
        let current =
            turn_lease::identity_for_hold_bound(shared, &hold.provider, id, &hold.mailbox).await;
        match current {
            Ok(Some(current)) if current == expected => {}
            Ok(_) => return Err(Reset("lease_changed")),
            Err(error) => return Err(identity_decline(error)),
        }
        if observe(shared, channel, &expected).await? != host {
            return Err(Retry("observation_unknown", Some("host_changed")));
        }
    }

    check_readiness_hold(gate(shared, channel, hold).await?, &expected)?;
    match turn_lease::release_for_hold_bound(
        shared,
        &hold.provider,
        id,
        &hold.mailbox,
        expected.clone(),
    )
    .await
    {
        Ok(true) => Ok((expected, host)),
        Ok(false) => Err(Reset("lease_changed")),
        Err(error) => {
            tracing::info!(channel_id = channel, %error, "hold bound release declined");
            Err(Reset(if current_actor(shared, channel, &hold.mailbox) {
                "lease_changed"
            } else {
                "incarnation_replaced"
            }))
        }
    }
}

fn identity_decline(error: String) -> Decline {
    if error == "mailbox incarnation changed" {
        Reset("incarnation_replaced")
    } else if error.contains("observation unavailable") {
        Retry("observation_unknown", Some("identity_unread"))
    } else {
        Retry("gate", None)
    }
}

async fn observe(
    shared: &SharedData,
    channel: u64,
    expected: &turn_lease::LeaseIdentity,
) -> Result<&'static str, Decline> {
    match witness::observe(shared, channel, expected).await {
        witness::Observation::Idle(host) => Ok(host),
        witness::Observation::NotIdle(reason) => Err(Retry("not_idle", Some(reason))),
        witness::Observation::Unknown(reason) => Err(Retry("observation_unknown", Some(reason))),
    }
}

fn check_readiness_hold(
    readiness_since: Option<u64>,
    expected: &turn_lease::LeaseIdentity,
) -> Result<(), Decline> {
    // A deadline already armed before this turn belongs to the idle refusal path.
    if readiness_since.is_some_and(|at| at <= expected.started_at.timestamp_millis().max(0) as u64)
    {
        return Err(Retry("gate", None));
    }
    Ok(())
}

async fn gate(shared: &SharedData, channel: u64, hold: &Arc<Hold>) -> Result<Option<u64>, Decline> {
    if !current_actor(shared, channel, &hold.mailbox) {
        return Err(Reset("incarnation_replaced"));
    }
    // Echo custody joins the exclusions here when pending echoes gain their own deadline.
    if super::restart_gate::inflight(hold.provider.as_str(), &shared.token_hash, channel)
        || super::ingress::ingress_hold(shared, false).is_some()
        || crate::services::discord::router::intake_dispatch::writer_boot_held(
            &hold.provider,
            channel,
        )
        .await
    {
        return Err(Retry("gate", None));
    }
    let expected = hold.front.clone();
    let state = hold
        .mailbox
        .v2_request(move |slot| {
            let owner = slot.as_ref().ok_or("V2 not adopted")?;
            let front = owner.front().filter(|front| same_front(&expected, front));
            Ok(front.map(|front| {
                (
                    owner.retry_waiting(&front) || owner.handoff().is_some(),
                    owner.front_hold().map(|readiness| readiness.started_at_ms),
                )
            }))
        })
        .await
        .map_err(|_| Retry("gate", None))?;
    let Some((gated, readiness_since)) = state.filter(|_| {
        clocks()
            .get(&channel)
            .is_some_and(|clock| Arc::ptr_eq(&clock.hold, hold))
    }) else {
        return Err(Reset("attempted"));
    };
    if gated {
        return Err(Retry("gate", None));
    }
    Ok(readiness_since)
}
