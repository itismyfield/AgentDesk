use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use super::InjectMode;

static FORCED: Mutex<Option<HashMap<u64, (InjectMode, PathBuf)>>> = Mutex::new(None);
static CODEX_ENABLED: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static QUEUED_ENABLED: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static CRASHING: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static FINAL_LOOKUP: Mutex<Option<HashMap<u64, Arc<Notify>>>> = Mutex::new(None);
type Park = (Arc<Notify>, Arc<Notify>);
static BEFORE_RESERVE: Mutex<Option<HashMap<u64, Park>>> = Mutex::new(None);
static QUEUED_PHASE: Mutex<Option<HashMap<(u64, &'static str), Park>>> = Mutex::new(None);
static GATEWAYLESS: Mutex<Option<HashMap<u64, Arc<Notify>>>> = Mutex::new(None);
static OWNER_CRASHING: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static HANDBACK_FAULTS: Mutex<Vec<(u64, HandbackFault)>> = Mutex::new(Vec::new());

/// How the channel's next handback answer is lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandbackFault {
    /// The actor wrote the queue front but its answer never came back.
    AnswerLost,
    /// The settle never reached the actor; the ticket is gone with it.
    Dropped,
}

pub(crate) fn fault_handback(channel_id: u64, fault: HandbackFault) {
    let mut faults = HANDBACK_FAULTS.lock().unwrap_or_else(|e| e.into_inner());
    faults.push((channel_id, fault));
}

/// The handback as the channel's scripted fault leaves it.
pub(super) async fn hand_back<T>(
    channel_id: u64,
    handed: impl std::future::Future<Output = Result<T, &'static str>>,
) -> Result<T, &'static str> {
    let fault = {
        let mut faults = HANDBACK_FAULTS.lock().unwrap_or_else(|e| e.into_inner());
        let at = faults
            .iter()
            .position(|(channel, _)| *channel == channel_id);
        at.map(|at| faults.remove(at).1)
    };
    match fault {
        None => handed.await,
        Some(HandbackFault::AnswerLost) => {
            assert!(handed.await.is_ok(), "the scripted handback lands");
            Err("handback_unknown")
        }
        // Never polled, so the settle and its ticket go nowhere.
        Some(HandbackFault::Dropped) => Err("handback_unknown"),
    }
}

/// Makes the channel's owner panic after its reservation, outside the pane effect.
pub(crate) fn crash_owner(channel_id: u64) {
    let mut crashing = OWNER_CRASHING.lock().unwrap_or_else(|e| e.into_inner());
    crashing.push(channel_id);
}

pub(super) fn owner_crash(channel_id: u64) {
    let crashing = OWNER_CRASHING.lock().unwrap_or_else(|e| e.into_inner());
    let crash = crashing.contains(&channel_id);
    drop(crashing);
    assert!(!crash, "scripted owner crash");
}

/// Lets the channel's deliver start run without a gateway context: the headless start's own
/// transition wait and mailbox claim, nothing after. The signal fires as each start begins.
pub(crate) fn start_without_gateway(channel_id: u64) -> Arc<Notify> {
    let mut starts = GATEWAYLESS.lock().unwrap_or_else(|e| e.into_inner());
    let starts = starts.get_or_insert_with(HashMap::new);
    starts.entry(channel_id).or_default().clone()
}

pub(in crate::services::discord::health::turn_deliver) fn start<'a>(
    shared: &'a crate::services::discord::SharedData,
    request: &'a super::HumanInputRequest,
    reservation: &'a crate::services::discord::router::HeadlessTurnReservation,
    metadata: &'a Option<serde_json::Value>,
) -> Option<impl std::future::Future<Output = super::super::StartAttempt> + 'a> {
    use super::super::StartAttempt;
    let starts = GATEWAYLESS.lock().unwrap_or_else(|e| e.into_inner());
    let begun = starts.as_ref()?.get(&request.channel_id.get())?.clone();
    Some(async move {
        see_start(shared, request, metadata).await;
        begun.notify_one();
        let channel = request.channel_id;
        let owner = poise::serenity_prelude::UserId::new(request.author_id);
        let identity = (request.provider.clone(), None);
        let claim = crate::services::discord::router::claim_reserved_headless_turn;
        match claim(shared, channel, owner, reservation, identity).await {
            Ok(_) => StartAttempt::Started(reservation.turn_id(channel)),
            Err(crate::services::discord::router::HeadlessTurnStartError::Conflict(_)) => {
                StartAttempt::Busy
            }
            Err(error) => StartAttempt::Unavailable(error.to_string()),
        }
    })
}

/// What one gatewayless start saw as it began: its GUID's episode and its prompt's contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StartSeen {
    pub episode: Option<String>,
    pub contract: Option<String>,
}

static STARTS_SEEN: Mutex<Vec<(u64, StartSeen)>> = Mutex::new(Vec::new());

async fn see_start(
    shared: &crate::services::discord::SharedData,
    request: &super::HumanInputRequest,
    metadata: &Option<serde_json::Value>,
) {
    let origin = metadata
        .as_ref()
        .and_then(|m| m.pointer("/human_input/origin_id"));
    let read = crate::db::imsg_reply::read_obligation;
    let episode = match (shared.pg_pool.as_ref(), origin.and_then(|o| o.as_str())) {
        (Some(pool), Some(origin)) => read(pool, origin).await.ok().flatten(),
        _ => None,
    };
    let lead = crate::services::imsg_reply::with_started_contract;
    let seen = StartSeen {
        episode: episode.and_then(|row| row.episode_key),
        contract: lead(None, Some(request.source.as_str()), metadata.as_ref()),
    };
    let mut starts = STARTS_SEEN.lock().unwrap_or_else(|e| e.into_inner());
    starts.push((request.channel_id.get(), seen));
}

pub(crate) fn starts_seen(channel_id: u64) -> Vec<StartSeen> {
    let starts = STARTS_SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let mine = starts.iter().filter(|(channel, _)| *channel == channel_id);
    mine.map(|(_, seen)| seen.clone()).collect()
}

/// Parks the channel's next owner before its reservation: `(reached, resume)`.
pub(crate) fn park_before_reserve(channel_id: u64) -> Park {
    let park: Park = Default::default();
    let mut parks = BEFORE_RESERVE.lock().unwrap_or_else(|e| e.into_inner());
    parks
        .get_or_insert_with(HashMap::new)
        .insert(channel_id, park.clone());
    park
}

pub(crate) fn park_queued(channel: u64, stage: &'static str) -> Park {
    let park: Park = Default::default();
    QUEUED_PHASE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert((channel, stage), park.clone());
    park
}

pub(super) async fn before_queued(channel: u64, stage: &'static str) {
    let park = QUEUED_PHASE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|parks| parks.remove(&(channel, stage)));
    if let Some((reached, resume)) = park {
        reached.notify_one();
        resume.notified().await;
    }
}

pub(super) async fn before_reserve(channel_id: u64) {
    let park = {
        let mut parks = BEFORE_RESERVE.lock().unwrap_or_else(|e| e.into_inner());
        parks.as_mut().and_then(|parks| parks.remove(&channel_id))
    };
    if let Some((reached, resume)) = park {
        reached.notify_one();
        resume.notified().await;
    }
}

/// Fires when the channel's resolve is about to take the core lock for its final name lookup.
pub(crate) fn final_lookup_signal(channel_id: u64) -> Arc<Notify> {
    let mut signals = FINAL_LOOKUP.lock().unwrap_or_else(|e| e.into_inner());
    let signals = signals.get_or_insert_with(HashMap::new);
    signals.entry(channel_id).or_default().clone()
}

pub(super) fn final_name_lookup(channel_id: u64) {
    final_lookup_signal(channel_id).notify_one();
}

pub(crate) fn forced(channel_id: u64) -> Option<(InjectMode, PathBuf)> {
    let forced = FORCED.lock().unwrap_or_else(|e| e.into_inner());
    forced.as_ref()?.get(&channel_id).cloned()
}

pub(crate) fn set_pane_program(channel_id: u64, program: PathBuf) {
    set(channel_id, InjectMode::All, program);
}

pub(crate) fn set(channel_id: u64, mode: InjectMode, program: PathBuf) {
    let mut forced = FORCED.lock().unwrap_or_else(|e| e.into_inner());
    forced
        .get_or_insert_with(HashMap::new)
        .insert(channel_id, (mode, program));
}

pub(crate) fn set_queued_head(channel_id: u64, open: bool) {
    let mut enabled = QUEUED_ENABLED.lock().unwrap_or_else(|e| e.into_inner());
    enabled.retain(|channel| *channel != channel_id);
    if open {
        enabled.push(channel_id);
    }
}

pub(crate) fn queued_head_enabled(channel_id: u64) -> bool {
    QUEUED_ENABLED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&channel_id)
}

/// Opens Codex injection on the channel, as turning the constant on would.
pub(crate) fn enable_codex(channel_id: u64) {
    let mut enabled = CODEX_ENABLED.lock().unwrap_or_else(|e| e.into_inner());
    enabled.push(channel_id);
}

pub(super) fn codex_enabled(channel_id: u64) -> bool {
    let enabled = CODEX_ENABLED.lock().unwrap_or_else(|e| e.into_inner());
    enabled.contains(&channel_id)
}

pub(crate) fn clear(channel_id: u64) {
    set_queued_head(channel_id, false);
    let mut enabled = CODEX_ENABLED.lock().unwrap_or_else(|e| e.into_inner());
    enabled.retain(|channel| *channel != channel_id);
    let mut forced = FORCED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = forced.as_mut() {
        map.remove(&channel_id);
    }
    let mut crashing = CRASHING.lock().unwrap_or_else(|e| e.into_inner());
    crashing.retain(|channel| *channel != channel_id);
    let mut crashing = OWNER_CRASHING.lock().unwrap_or_else(|e| e.into_inner());
    crashing.retain(|channel| *channel != channel_id);
    let mut faults = HANDBACK_FAULTS.lock().unwrap_or_else(|e| e.into_inner());
    faults.retain(|(channel, _)| *channel != channel_id);
    let mut starts = GATEWAYLESS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(starts) = starts.as_mut() {
        starts.remove(&channel_id);
    }
}

/// Makes the channel's effect panic outside its own `catch_unwind`, as a dying executor would.
pub(crate) fn crash_effect(channel_id: u64) {
    let mut crashing = CRASHING.lock().unwrap_or_else(|e| e.into_inner());
    crashing.push(channel_id);
}

pub(super) fn crash(channel_id: u64) {
    let crashing = CRASHING.lock().unwrap_or_else(|e| e.into_inner());
    let crash = crashing.contains(&channel_id);
    drop(crashing);
    assert!(!crash, "scripted executor crash");
}
