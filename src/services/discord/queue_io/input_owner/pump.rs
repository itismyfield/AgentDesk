//! One attempt at a time per physical writer: the pump reserves the owner's head for a prepared
//! payload and runs its effect, while attempts on other writers run meanwhile.
#![cfg(unix)]
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use super::effect::{self, Clock, Completion, Leaf, PreparedPayload, parse};
use super::target::TargetKey;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

type Lane = tokio::sync::Mutex<()>;

fn lane(key: TargetKey) -> Arc<Lane> {
    static LANES: OnceLock<Mutex<HashMap<TargetKey, Weak<Lane>>>> = OnceLock::new();
    let mut lanes = LANES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    lanes.retain(|_, lane| lane.strong_count() > 0);
    if let Some(lane) = lanes.get(&key).and_then(Weak::upgrade) {
        return lane;
    }
    let lane = Arc::new(Lane::default());
    lanes.insert(key, Arc::downgrade(&lane));
    lane
}

/// Reserves the owner's head for `prepared` and runs `leaf` on its frame, unsplit. The owner,
/// not the pump, picks which original the attempt carries.
pub(crate) async fn submit(
    mailbox: &ChannelMailboxHandle,
    prepared: PreparedPayload,
    clock: Clock,
    leaf: Leaf,
) -> Result<Completion, String> {
    let lane = lane(TargetKey::of(&prepared.attempt)?);
    let _writer = lane.lock().await;
    let PreparedPayload {
        frame,
        sources,
        attempt,
    } = prepared;
    let payload = serde_json::json!({
        "digest": format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(frame.as_bytes())),
        "frame": frame,
        "attribution": {"sources": sources},
    });
    mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("V2 not adopted")?;
            owner.reserve(parse(attempt)?, parse(payload)?).map(drop)
        })
        .await?;
    effect::run(mailbox, clock, leaf).await
}

#[cfg(test)]
#[path = "pump_tests.rs"]
mod tests;
