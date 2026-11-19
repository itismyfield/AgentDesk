//! A Busy-held front no read proved ready gets the owner's fixed deadline and the drive's timer
//! for it, so the hold ends at that due even when no look ever clears.
use std::sync::Arc;

use super::super::drive::{Drive, Front, arm_deadline};

/// Why the owner holds the front: Busy admission read neither a busy turn nor an idle proof.
const CAUSE: &str = "busy_readiness_unproven";

/// Records the owner's deadline for `front`, kept if one is set, and arms the drive's timer.
pub(crate) async fn hold(drive: Arc<Drive>, front: &Front) {
    let (front, now) = (
        front.clone(),
        chrono::Utc::now().timestamp_millis().max(0) as u64,
    );
    let held = drive.mailbox.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("V2 not adopted")?;
        owner.hold_readiness(&front, CAUSE, now).map(drop)
    });
    match held.await {
        Ok(()) => arm_deadline(drive),
        Err(error) => {
            tracing::warn!(channel_id = drive.channel, %error, "readiness hold has no deadline")
        }
    }
}
