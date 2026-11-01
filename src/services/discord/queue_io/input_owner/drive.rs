//! The owner's execution entry: one drive per channel grants the owner's unattempted front, hands
//! it to a turn and, once that local transaction settled, takes the next front.
#![cfg(unix)]
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use super::effect::{Completion, MODAL};
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
    /// How long a front held under a modal waits before the drive looks at it again.
    pub(crate) recheck: Duration,
    pub(crate) settled: Option<Settled>,
}

#[derive(Default)]
struct Lane {
    running: AtomicBool,
    again: AtomicBool,
    /// A head's confirmation deadline is already awaited.
    armed: AtomicBool,
}

fn lane(channel: u64) -> Arc<Lane> {
    static LANES: OnceLock<Mutex<HashMap<u64, Weak<Lane>>>> = OnceLock::new();
    let mut lanes = LANES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    lanes.retain(|_, lane| lane.strong_count() > 0);
    if let Some(lane) = lanes.get(&channel).and_then(Weak::upgrade) {
        return lane;
    }
    let lane = Arc::new(Lane::default());
    lanes.insert(channel, Arc::downgrade(&lane));
    lane
}

/// Starts the channel's drive unless one runs; a running drive looks at the front once more.
pub(crate) fn wake(drive: Arc<Drive>) {
    let lane = lane(drive.channel);
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
            let result = (drive.handoff)(front).await;
            if result == Err(format!("{:?}", Completion::NotSent(MODAL.into()))) {
                let again = Arc::clone(&drive);
                tokio::spawn(async move {
                    tokio::time::sleep(again.recheck).await;
                    wake(again);
                });
                break;
            }
            stalled = result.is_err();
            if let Err(error) = result {
                tracing::info!(channel_id = drive.channel, %error, "owner front not delivered");
            }
        }
        if let Some(settled) = &drive.settled {
            settled().await;
        }
        lane.running.store(false, Ordering::SeqCst);
        // A wake that landed after the last look starts the loop again here, never twice.
        if !lane.again.load(Ordering::SeqCst) || lane.running.swap(true, Ordering::SeqCst) {
            settle_at_deadline(lane, drive);
            return;
        }
    }
}

/// The head's confirmation window ends once at its deadline: the owner settles it there, then the
/// drive looks again. Nothing polls before that deadline.
fn settle_at_deadline(lane: Arc<Lane>, drive: Arc<Drive>) {
    if lane.armed.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        let now = || chrono::Utc::now().timestamp_millis().max(0) as u64;
        let due = super::recovery::settle_due(&drive.mailbox).await;
        let ticked = match due {
            Ok(Some(due)) => {
                tokio::time::sleep(Duration::from_millis(due.saturating_sub(now()))).await;
                let at = now();
                let ticked = drive
                    .mailbox
                    .v2_request(move |slot| slot.as_mut().ok_or("V2 not adopted")?.tick(at));
                Some(ticked.await)
            }
            _ => None,
        };
        lane.armed.store(false, Ordering::SeqCst);
        match ticked {
            Some(Ok(())) => wake(drive),
            Some(Err(error)) => {
                tracing::warn!(channel_id = drive.channel, %error, "owner deadline unsettled")
            }
            None => {}
        }
    });
}

/// The owner's unattempted front, granted in the same actor turn that read it. After a failed
/// handoff whose right the queue neither spent nor revoked the front has not moved: none is.
async fn grant(drive: &Drive, stalled: bool) -> Option<Front> {
    let granted = drive
        .mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("V2 not adopted")?;
            if stalled && owner.front_outstanding() {
                return Ok(None);
            }
            let front = owner.front();
            if let Some(front) = &front {
                owner.grant_front(front)?;
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
