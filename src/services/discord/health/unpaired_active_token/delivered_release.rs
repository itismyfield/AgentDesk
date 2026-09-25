//! Inert verdict: may a rowless mailbox anchor whose answer was delivered be released?
//! Only episode-exact positive evidence past a monotonic grace decides; absence keeps it.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use poise::serenity_prelude::ChannelId;
use tokio::time::Instant;

use crate::services::discord::inflight::inflight_state_path;
use crate::services::discord::outbound::completed_turn_ledger;
use crate::services::discord::outbound::delivery_record::{self, DeliveryRecord};
use crate::services::discord::{self as discord, SharedData};
use crate::services::platform::tmux::SessionPresence;
use crate::services::provider::ProviderKind;
use crate::services::tmux_turn_liveness;

const OWNER_FINISH_GRACE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord::health) enum Reason {
    BootReconcilePending,
    UnmeasuredRow,
    NoLedgerWitness,
    ReceiptUnreadable,
    NoEpisodeReceipt,
    PaneMissing,
    PaneUnmeasured,
    PaneNotProvenIdle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord::health) enum Verdict {
    NotApplicable,
    Defer,
    Refuse(Reason),
    Hold(Reason),
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneProof {
    Proven,
    NotProven,
    Unmeasured,
    Missing,
}

#[derive(Debug, Clone, Default)]
struct DeliveredReleaseObservation {
    restart_pinned: bool,
    session: Option<String>,
    row_present: Option<bool>,
    ledger_committed_at_ms: Option<u64>,
    record_readable: bool,
    episode_receipt: bool,
}

fn decide(
    obs: &DeliveredReleaseObservation,
    grace_elapsed: bool,
    pane: Option<PaneProof>,
) -> Verdict {
    if obs.restart_pinned {
        return Verdict::NotApplicable;
    }
    if obs.session.is_none() {
        return Verdict::Refuse(Reason::PaneUnmeasured);
    }
    match obs.row_present {
        Some(true) => return Verdict::NotApplicable,
        None => return Verdict::Refuse(Reason::UnmeasuredRow),
        Some(false) => {}
    }
    if obs.ledger_committed_at_ms.is_none() {
        return Verdict::Refuse(Reason::NoLedgerWitness);
    }
    if !obs.record_readable {
        return Verdict::Refuse(Reason::ReceiptUnreadable);
    }
    if !obs.episode_receipt {
        return Verdict::Refuse(Reason::NoEpisodeReceipt);
    }
    if !grace_elapsed {
        return Verdict::Defer;
    }
    match pane {
        Some(PaneProof::Proven) => Verdict::Release,
        Some(PaneProof::NotProven) => Verdict::Hold(Reason::PaneNotProvenIdle),
        Some(PaneProof::Missing) => Verdict::Refuse(Reason::PaneMissing),
        Some(PaneProof::Unmeasured) | None => Verdict::Hold(Reason::PaneUnmeasured),
    }
}

fn episode_receipt(
    record: &DeliveryRecord,
    provider: &ProviderKind,
    channel_id: u64,
    nonce: &str,
    session: &str,
) -> bool {
    record.confirmed_deliveries.iter().any(|receipt| {
        let source = &receipt.source;
        source.is_authoritative()
            && source.provider == provider.as_str()
            && source.turn_nonce == nonce
            && source.tmux_session_name == session
            && source.delivery_channel_id == channel_id
            && receipt.delivery_channel_id == channel_id
            && source.offset_authority_channel_id == channel_id
            && receipt.message_id != 0
    })
}

fn row_presence_from(root: Option<&Path>, exists: io::Result<bool>) -> Option<bool> {
    root?;
    exists.ok()
}

fn row_presence(provider: &ProviderKind, channel_id: u64) -> Option<bool> {
    let root = discord::inflight::inflight_runtime_root();
    let row = root
        .as_deref()
        .map(|root| inflight_state_path(root, provider, channel_id));
    row_presence_from(
        root.as_deref(),
        row.map_or(Ok(false), |row| row.try_exists()),
    )
}

fn observe(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    nonce: &str,
    obs: &mut DeliveredReleaseObservation,
) {
    let Some(session) = obs.session.clone().filter(|_| !obs.restart_pinned) else {
        return;
    };
    obs.row_present = row_presence(provider, channel_id);
    if obs.row_present != Some(false) {
        return;
    }
    obs.ledger_committed_at_ms =
        completed_turn_ledger::completed_turn_committed_at_ms(provider, channel_id, user_msg_id);
    if obs.ledger_committed_at_ms.is_none() {
        return;
    }
    if let Some(record) = delivery_record::read_record(provider, channel_id) {
        obs.record_readable = true;
        obs.episode_receipt = episode_receipt(&record, provider, channel_id, nonce, &session);
    }
}

fn pane_proof_from(presence: SessionPresence, proven_idle: impl FnOnce() -> bool) -> PaneProof {
    match presence {
        SessionPresence::Missing => PaneProof::Missing,
        SessionPresence::ProbeFailed => PaneProof::Unmeasured,
        SessionPresence::Present if proven_idle() => PaneProof::Proven,
        SessionPresence::Present => PaneProof::NotProven,
    }
}

async fn pane_proof(session: &str) -> PaneProof {
    if cfg!(not(unix)) {
        return PaneProof::Unmeasured;
    }
    let session = session.to_owned();
    #[cfg(test)]
    if let Some((presence, proven_idle)) = tests::pane_seam(&session) {
        return measured_pane(&session, presence, proven_idle);
    }
    tokio::task::spawn_blocking(move || {
        let presence = crate::services::platform::tmux::session_presence(&session);
        measured_pane(&session, presence, None)
    })
    .await
    .unwrap_or(PaneProof::Unmeasured)
}

fn measured_pane(session: &str, presence: SessionPresence, proven_idle: Option<bool>) -> PaneProof {
    pane_proof_from(presence, || {
        proven_idle.unwrap_or_else(|| tmux_turn_liveness::provider_session_is_proven_idle(session))
    })
}

type GraceKey = (String, u64);
static GRACE: LazyLock<Mutex<HashMap<GraceKey, (u64, String, Instant)>>> =
    LazyLock::new(Default::default);

fn grace_observe(key: &GraceKey, user_msg_id: u64, nonce: &str, now: Instant) -> Instant {
    let mut grace = GRACE.lock().unwrap_or_else(|error| error.into_inner());
    let entry = grace
        .entry(key.clone())
        .or_insert((user_msg_id, nonce.to_owned(), now));
    if entry.0 != user_msg_id || entry.1 != nonce {
        *entry = (user_msg_id, nonce.to_owned(), now);
    }
    entry.2
}

fn grace_reset(key: &GraceKey) {
    GRACE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(key);
}

fn settle(key: &GraceKey, verdict: Verdict) -> Verdict {
    if matches!(verdict, Verdict::Refuse(_) | Verdict::NotApplicable) {
        grace_reset(key);
    }
    let would_release = verdict == Verdict::Release;
    tracing::debug!(provider = %key.0, channel_id = key.1, ?verdict, would_release, "L1 verdict");
    verdict
}

pub(in crate::services::discord::health) async fn reconcile_channel(
    provider: &ProviderKind,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
) -> Verdict {
    if !shared.restart.reconcile_done.load(Ordering::Acquire) {
        return Verdict::Hold(Reason::BootReconcilePending);
    }
    let s0 = discord::mailbox_snapshot(shared, channel_id).await;
    let (Some(token), Some(anchor), Some(nonce)) = (
        s0.cancel_token.clone(),
        s0.active_user_message_id,
        s0.active_turn_nonce
            .clone()
            .filter(|nonce| !nonce.is_empty()),
    ) else {
        return Verdict::NotApplicable;
    };
    let key: GraceKey = (provider.as_str().to_owned(), channel_id.get());
    let mut obs = DeliveredReleaseObservation {
        restart_pinned: token.restart_mode().is_some(),
        session: token.tmux_session_name().filter(|name| !name.is_empty()),
        ..Default::default()
    };
    observe(provider, channel_id.get(), anchor.get(), &nonce, &mut obs);
    let evidence = decide(&obs, false, None);
    if evidence != Verdict::Defer {
        return settle(&key, evidence);
    }

    let now = Instant::now();
    let first = grace_observe(&key, anchor.get(), &nonce, now);
    if now.duration_since(first) < OWNER_FINISH_GRACE {
        return settle(&key, evidence);
    }
    let session = obs.session.clone().unwrap_or_default();
    let verdict = decide(&obs, true, Some(pane_proof(&session).await));
    if verdict != Verdict::Release {
        return settle(&key, verdict);
    }

    let s1 = discord::mailbox_snapshot(shared, channel_id).await;
    let same_token = s1
        .cancel_token
        .is_some_and(|current| Arc::ptr_eq(&current, &token));
    let same_episode = same_token
        && s1.active_user_message_id == Some(anchor)
        && s1.active_turn_nonce.as_deref() == Some(nonce.as_str());
    if !same_episode || row_presence(provider, channel_id.get()) != Some(false) {
        return settle(&key, Verdict::NotApplicable);
    }
    settle(&key, Verdict::Release)
}

#[cfg(test)]
#[path = "delivered_release_tests.rs"]
mod tests;
