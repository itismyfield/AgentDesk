//! The owner's execution entry: one drive per channel grants the owner's unattempted front, hands
//! it to a turn and, once that local transaction settled, takes the next front.
#![cfg(unix)]
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::time::Instant;

use super::hold_bound;

use crate::services::turn_orchestrator::{ChannelMailboxHandle, PendingQueueItem};

/// The owner's granted front: its slot's ticket and the original.
pub(crate) type Front = (Option<String>, PendingQueueItem);
/// The right to reserve one front with the originals merged behind it when it was claimed, spent
/// from its grant by the owner's turn claim; it moves, never copies, from that claim to the reserve.
pub(crate) struct FrontClaim(Front, Vec<PendingQueueItem>, u64);

impl FrontClaim {
    pub(crate) fn front(&self) -> &Front {
        &self.0
    }

    /// The queued originals behind the front that its one prompt carries, in arrival order.
    pub(crate) fn followers(&self) -> &[PendingQueueItem] {
        &self.1
    }

    /// The owner's number for this claim; only the current claim of that owner reserves.
    pub(crate) fn number(&self) -> u64 {
        self.2
    }
}

/// The owner's turn claim on a front the drive granted; `None` when the owner refuses it.
pub(crate) async fn claim_turn(
    mailbox: &ChannelMailboxHandle,
    message: u64,
    front: &Front,
) -> Result<Option<FrontClaim>, String> {
    let front = front.clone();
    let claimed = mailbox.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("V2 not adopted")?;
        let claimed = owner.claim_turn(message, &front);
        Ok(claimed.map(|front| FrontClaim(front, owner.followers(), owner.claim_number())))
    });
    claimed.await
}

/// Runs one granted front to its local settlement; the AI's completion is not awaited.
pub(crate) type Handoff =
    Arc<dyn Fn(Front) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

/// Runs once a pass found no front left to hand; the notices the owner owes post here.
pub(crate) type Settled = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub(crate) struct Drive {
    pub(crate) mailbox: ChannelMailboxHandle,
    pub(crate) channel: u64,
    pub(crate) handoff: Handoff,
    pub(crate) settled: Option<Settled>,
}

struct Lane {
    mailbox: ChannelMailboxHandle,
    running: AtomicBool,
    again: AtomicBool,
    /// The settle deadline already awaited; an earlier one replaces it.
    armed: Mutex<Option<Deadline>>,
}

fn lane(drive: &Drive) -> Arc<Lane> {
    type Lanes = HashMap<u64, Vec<Weak<Lane>>>;
    static LANES: OnceLock<Mutex<Lanes>> = OnceLock::new();
    let mut lanes = LANES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    lanes.retain(|_, actors| {
        actors.retain(|lane| lane.strong_count() > 0);
        !actors.is_empty()
    });
    let actors = lanes.entry(drive.channel).or_default();
    if let Some(lane) = actors
        .iter()
        .filter_map(Weak::upgrade)
        .find(|lane| lane.mailbox.same_actor(&drive.mailbox))
    {
        return lane;
    }
    let lane = Arc::new(Lane {
        mailbox: drive.mailbox.clone(),
        running: AtomicBool::new(false),
        again: AtomicBool::new(false),
        armed: Mutex::new(None),
    });
    actors.push(Arc::downgrade(&lane));
    lane
}

/// Starts the channel's drive unless one runs; a running drive looks at the front once more.
pub(crate) fn wake(drive: Arc<Drive>) {
    let lane = lane(&drive);
    lane.again.store(true, Ordering::SeqCst);
    if !lane.running.swap(true, Ordering::SeqCst) {
        tokio::spawn(run(lane, drive));
    }
}

async fn run(lane: Arc<Lane>, drive: Arc<Drive>) {
    loop {
        lane.again.store(false, Ordering::SeqCst);
        let mut stalled = false;
        while let Some(front) = grant(&drive, stalled).await {
            let message = front.1.message_id;
            let timing = crate::logging::intake_timing::span_for(message);
            crate::logging::intake_timing::mark_message(
                message,
                crate::logging::intake_timing::Stage::Front,
            );
            let result = tracing::Instrument::instrument((drive.handoff)(front), timing).await;
            if let Err(error) = &result {
                tracing::info!(channel_id = drive.channel, %error, "owner front not delivered");
            }
            // A front refused before any key for a reason a later look may clear waits for a real
            // wake (a turn boundary, new input, its pane's screen recovering) or its retry due.
            if result.is_err() && held(&drive, message).await {
                super::held_watch::watch(drive.clone(), message);
                break;
            }
            crate::logging::intake_timing::finish_message(message, &result);
            stalled = result.is_err();
        }
        if let Some(settled) = &drive.settled {
            settled().await;
        }
        lane.running.store(false, Ordering::SeqCst);
        // A wake that landed after the last look starts the loop again here, never twice.
        if !lane.again.load(Ordering::SeqCst) || lane.running.swap(true, Ordering::SeqCst) {
            settle_at_deadline(lane, drive, true);
            return;
        }
    }
}

/// Arms the owner's next settle deadline for a hold that no drive pass ended.
pub(crate) fn arm_deadline(drive: Arc<Drive>) {
    settle_at_deadline(lane(&drive), drive, true);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Deadline {
    Owner(u64),
    HoldBound(Instant),
}

impl Deadline {
    fn instant(self, now: Instant, wall_ms: u64) -> Instant {
        match self {
            Self::Owner(at) => now + Duration::from_millis(at.saturating_sub(wall_ms)),
            Self::HoldBound(at) => at,
        }
    }
}

fn wall_now() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// Wait for the earlier queue settlement or hold check. Only queue settlement wakes the drive;
/// a successful hold check gets its wake through the finalizer's completion event.
fn settle_at_deadline(lane: Arc<Lane>, drive: Arc<Drive>, include_owner: bool) {
    tokio::spawn(async move {
        let owner = if include_owner {
            super::recovery::settle_due(&drive.mailbox)
                .await
                .ok()
                .flatten()
                .map(Deadline::Owner)
        } else {
            None
        };
        let bound = hold_bound::due(&drive).map(Deadline::HoldBound);
        let (now, wall_ms) = (Instant::now(), wall_now());
        let Some(deadline) = owner
            .into_iter()
            .chain(bound)
            .min_by_key(|due| due.instant(now, wall_ms))
        else {
            return;
        };
        let instant = deadline.instant(now, wall_ms);
        {
            let mut armed = lane.armed.lock().unwrap_or_else(|e| e.into_inner());
            if armed.is_some_and(|at| at == deadline || at.instant(now, wall_ms) <= instant) {
                return;
            }
            *armed = Some(deadline);
        }
        tokio::time::sleep_until(instant).await;
        if *lane.armed.lock().unwrap_or_else(|e| e.into_inner()) != Some(deadline) {
            return;
        }
        let ticked = match deadline {
            Deadline::Owner(_) => {
                let at = wall_now();
                Some(
                    drive
                        .mailbox
                        .v2_request(move |slot| slot.as_mut().ok_or("V2 not adopted")?.tick(at))
                        .await,
                )
            }
            Deadline::HoldBound(_) => {
                hold_bound::check(&drive).await;
                None
            }
        };
        {
            let mut armed = lane.armed.lock().unwrap_or_else(|e| e.into_inner());
            if *armed == Some(deadline) {
                *armed = None;
            }
        }
        match ticked {
            Some(Ok(())) => wake(drive),
            Some(Err(error)) => {
                tracing::warn!(channel_id = drive.channel, %error, "owner deadline unsettled");
                settle_at_deadline(lane, drive, false);
            }
            None => settle_at_deadline(lane, drive, true),
        }
    });
}

/// Whether the owner held `message` at the front after its handoff refused before any key.
async fn held(drive: &Drive, message: u64) -> bool {
    let held = drive.mailbox.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("V2 not adopted")?;
        Ok(owner.take_held(message))
    });
    held.await.unwrap_or(false)
}

/// The owner's unattempted front, granted in the same actor turn that read it. After a failed
/// handoff whose right the queue neither spent nor revoked the front has not moved: none is.
async fn grant(drive: &Drive, stalled: bool) -> Option<Front> {
    let (channel, mailbox) = (drive.channel, drive.mailbox.clone());
    let granted = drive
        .mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("V2 not adopted")?;
            let front = owner.front();
            hold_bound::sync_front(channel, &mailbox, front.as_ref());
            if stalled && owner.front_outstanding() {
                return Ok(None);
            }
            // A front in an armed retry wait waits for its due or a read of its cause.
            if let Some(front) = &front
                && !owner.grant_front(front)?
            {
                return Ok(None);
            }
            Ok(front)
        })
        .await;
    granted.unwrap_or_else(|error| {
        tracing::warn!(channel_id = drive.channel, %error, "owner front unavailable");
        None
    })
}

#[cfg(test)]
#[path = "drive_tests.rs"]
mod tests;

#[cfg(test)]
pub(crate) async fn grant_stalled_for_tests(
    mailbox: ChannelMailboxHandle,
    channel: u64,
) -> Option<Front> {
    grant(
        &Drive {
            mailbox,
            channel,
            handoff: Arc::new(|_| Box::pin(async { Ok(()) })),
            settled: None,
        },
        true,
    )
    .await
}
