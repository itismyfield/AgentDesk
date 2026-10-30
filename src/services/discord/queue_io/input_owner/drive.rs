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
/// Runs one granted front to its local settlement; the AI's completion is not awaited.
pub(crate) type Handoff =
    Arc<dyn Fn(Front) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

pub(crate) struct Drive {
    pub(crate) mailbox: ChannelMailboxHandle,
    pub(crate) channel: u64,
    pub(crate) handoff: Handoff,
    /// How long a front held under a modal waits before the drive looks at it again.
    pub(crate) recheck: Duration,
}

#[derive(Default)]
struct Lane {
    running: AtomicBool,
    again: AtomicBool,
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
        let mut last = None;
        while let Some(front) = grant(&drive).await {
            // A front that did not move since its last handoff waits for the next wake.
            let seen = serde_json::to_value(&front).ok();
            if last.is_some() && last == seen {
                break;
            }
            last = seen;
            let result = (drive.handoff)(front).await;
            if result == Err(format!("{:?}", Completion::NotSent(MODAL.into()))) {
                let again = Arc::clone(&drive);
                tokio::spawn(async move {
                    tokio::time::sleep(again.recheck).await;
                    wake(again);
                });
                break;
            }
            if let Err(error) = result {
                tracing::info!(channel = drive.channel, %error, "owner front not delivered");
            }
        }
        lane.running.store(false, Ordering::SeqCst);
        // A wake that landed after the last look starts the loop again here, never twice.
        if !lane.again.load(Ordering::SeqCst) || lane.running.swap(true, Ordering::SeqCst) {
            return;
        }
    }
}

/// The owner's unattempted front, granted in the same actor turn that read it.
async fn grant(drive: &Drive) -> Option<Front> {
    let granted = drive
        .mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("V2 not adopted")?;
            let front = owner.front();
            if let Some(front) = &front {
                owner.grant_front(front)?;
            }
            Ok(front)
        })
        .await;
    granted.unwrap_or_else(|error| {
        tracing::warn!(channel = drive.channel, %error, "owner front unavailable");
        None
    })
}

#[cfg(test)]
#[path = "drive_tests.rs"]
mod tests;
