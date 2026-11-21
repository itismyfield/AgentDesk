//! One attempt at a time per physical writer: the pump reserves the owner's head for a prepared
//! payload and runs its effect, while attempts on other writers run meanwhile.
#![cfg(unix)]
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use super::drive::FrontClaim;
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
#[cfg(test)]
pub(crate) async fn submit(
    mailbox: &ChannelMailboxHandle,
    prepared: PreparedPayload,
    clock: Clock,
    leaf: Leaf,
) -> Result<Completion, String> {
    submit_front(mailbox, None, prepared, clock, leaf).await
}

/// As `submit`; given a turn's claim on the owner's front, that front is reserved once, never one
/// the queue moved since the claim.
pub(crate) async fn submit_front(
    mailbox: &ChannelMailboxHandle,
    claim: Option<FrontClaim>,
    prepared: PreparedPayload,
    clock: Clock,
    leaf: Leaf,
) -> Result<Completion, String> {
    let attribution = serde_json::json!({"sources": prepared.sources});
    submit_with_attribution(mailbox, claim, prepared, attribution, clock, leaf).await
}

async fn submit_with_attribution(
    mailbox: &ChannelMailboxHandle,
    claim: Option<FrontClaim>,
    prepared: PreparedPayload,
    attribution: serde_json::Value,
    clock: Clock,
    leaf: Leaf,
) -> Result<Completion, String> {
    let lane = lane(TargetKey::of(&prepared.attempt)?);
    let _writer = lane.lock().await;
    let PreparedPayload {
        frame,
        sources: _,
        attempt,
    } = prepared;
    let followers = claim.as_ref().map_or(&[][..], FrontClaim::followers);
    let payload = serde_json::json!({
        "digest": format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(frame.as_bytes())),
        "frame": frame,
        "attribution": attribution,
        "followers": followers,
    });
    mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("V2 not adopted")?;
            let (attempt, payload) = (parse(attempt)?, parse(payload)?);
            let reserved = match claim {
                Some(claim) => {
                    owner.reserve_claimed((claim.front(), claim.number()), attempt, payload)
                }
                None => owner.reserve(attempt, payload),
            };
            reserved?;
            if owner.json()?["head_injection"]["payload"]["attribution"]
                .get("native_intent")
                .is_some()
            {
                owner.native_intent()?;
            }
            Ok(())
        })
        .await
        .map_err(effect::unsent)?;
    effect::run(mailbox, clock, leaf).await
}

pub(crate) async fn submit_native_front(
    mailbox: &ChannelMailboxHandle,
    claim: Option<FrontClaim>,
    prepared: PreparedPayload,
    intent: crate::services::tui_input::human_frame::history::v2::native::Intent,
    clock: Clock,
    leaf: Leaf,
) -> Result<Completion, String> {
    let attribution = intent.attribution(&prepared.sources);
    submit_with_attribution(mailbox, claim, prepared, attribution, clock, leaf).await
}

#[cfg(test)]
#[path = "pump_tests.rs"]
mod tests;
