use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use poise::serenity_prelude::{Channel, ChannelId, GuildChannel, Http};
use sqlx::PgPool;

use crate::services::provider::ProviderKind;
use crate::services::tui_o::channel_policy::BootChannels;
use crate::services::tui_o::cutover;
use crate::services::tui_o::ownership::{self, OwnershipGate};
use crate::services::tui_o::store::agy_spool::replay::{AgySlot, AgyTarget};
use crate::services::tui_o::writer::host::agy_host::{
    AgyAdoption, AgyCustody, AgyEvidence, AgyShadowHost, spool_destinations,
};

mod evidence;

/// How one destination's spool is held at boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Custody {
    /// The current writer selection; adoption may grant the read permit.
    Selected(AgyTarget),
    /// An existing spool outside the selection: replay only, never a new-input permit.
    Recovery(AgyTarget),
}

impl Custody {
    pub(super) fn target(self) -> AgyTarget {
        match self {
            Self::Selected(target) | Self::Recovery(target) => target,
        }
    }
}

pub(super) async fn observe(http: &Http, pool: Option<&PgPool>, provider: &ProviderKind) {
    if *provider != ProviderKind::Antigravity {
        return;
    }
    let Some(snapshot) = cutover::boot_snapshot() else {
        return;
    };
    let gate = ownership::gate(provider.as_str());
    if snapshot.site() != &crate::services::tui_o::channel_policy::Site::Home
        || gate.current().epoch().is_none()
    {
        return;
    }
    let Some(root) = crate::config::runtime_root() else {
        return;
    };
    let scan_root = root.clone();
    let spooled = match tokio::task::spawn_blocking(move || spool_destinations(&scan_root)).await {
        Ok(Ok(destinations)) => destinations,
        Ok(Err(error)) => {
            tracing::warn!(%error, "[tui_o] agy shadow spool index is held");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "[tui_o] agy shadow spool index task stopped");
            return;
        }
    };
    let spooled: BTreeSet<u64> = spooled.into_iter().collect();
    let destinations: BTreeSet<_> = spooled
        .iter()
        .copied()
        .chain(snapshot.agy_channels().iter().copied())
        .collect();
    for channel in destinations {
        // A selected direct channel needs no REST read; every other destination is checked.
        let read = if snapshot.agy_selected(channel) {
            None
        } else {
            match ChannelId::new(channel).to_channel(http).await {
                Ok(Channel::Guild(read)) => Some(read),
                _ => None,
            }
        };
        let held_by = match custody(
            &snapshot,
            channel,
            spooled.contains(&channel),
            read.as_ref(),
        ) {
            Ok(held_by) => held_by,
            Err(reason) => {
                tracing::warn!(channel, reason, "[tui_o] agy shadow custody held");
                continue;
            }
        };
        let inflight =
            super::super::super::inflight::load_inflight_state_read_only_result(provider, channel);
        let first = {
            let (root, gate) = (root.clone(), Arc::clone(&gate));
            tokio::task::spawn_blocking(move || first_read(root, held_by, gate)).await
        };
        let (host, first) = match first {
            Ok(Ok(Some(read))) => read,
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                tracing::warn!(channel, %error, "[tui_o] agy shadow custody held");
                continue;
            }
            Err(error) => {
                tracing::warn!(channel, %error, "[tui_o] agy shadow replay task stopped");
                continue;
            }
        };
        let target = held_by.target();
        let rows = evidence::admission_rows(pool, provider, target.destination, &first.turns).await;
        let found = evidence::assemble(target, &first.turns, &inflight, &rows);
        let settled = evidence::prior_legacy_settled(&inflight, &first.turns, &found);
        let second = tokio::task::spawn_blocking(move || {
            second_read(&host, held_by, &first, settled, &found)
        })
        .await;
        match second {
            Ok(Ok(adoption)) => log_adoption(channel, held_by, &adoption),
            Ok(Err(error)) => tracing::warn!(channel, %error, "[tui_o] agy shadow custody held"),
            Err(error) => tracing::warn!(channel, %error, "[tui_o] agy shadow replay task stopped"),
        }
    }
}

/// Selection first, threads recorded in their usual order; recovery-only is the last fallback.
pub(super) fn custody(
    snapshot: &BootChannels,
    channel: u64,
    spooled: bool,
    read: Option<&GuildChannel>,
) -> Result<Custody, &'static str> {
    if snapshot.agy_selected(channel) {
        return snapshot
            .agy_target(channel)
            .map(Custody::Selected)
            .ok_or("agy selected channel has no target");
    }
    let Some(read) = read.filter(|read| read.id.get() == channel) else {
        return Err("agy destination could not be checked");
    };
    let cached = snapshot.agy_target(channel);
    if snapshot.record_agy_thread(read)
        && let Some(target) = snapshot.agy_target(channel)
    {
        return Ok(Custody::Selected(target));
    }
    // A cached child the fresh read contradicts is ambiguous, not recoverable.
    if cached.is_some() {
        return Err("agy thread relationship changed");
    }
    if !spooled {
        return Err("agy destination has no selected parent");
    }
    snapshot.agy_recovery_target(read).map(Custody::Recovery)
}

/// First read of every slot; its frontiers pin the prefix the second read must prove again.
pub(super) fn first_read(
    root: PathBuf,
    held_by: Custody,
    gate: Arc<OwnershipGate>,
) -> io::Result<Option<(AgyShadowHost, AgyCustody)>> {
    let Some(host) = AgyShadowHost::start(root, held_by.target(), gate)? else {
        return Ok(None);
    };
    let first = host.custody(&BTreeMap::new())?;
    Ok(Some((host, first)))
}

/// Second read over the first read's frontiers; recovery custody never asks for a permit.
pub(super) fn second_read(
    host: &AgyShadowHost,
    held_by: Custody,
    first: &AgyCustody,
    prior_legacy_settled: bool,
    evidence: &BTreeMap<AgySlot, AgyEvidence>,
) -> io::Result<AgyAdoption> {
    let previous = first.frontiers();
    match held_by {
        Custody::Selected(_) => host.adopt(&previous, prior_legacy_settled, evidence),
        Custody::Recovery(_) => host
            .recover(&previous, evidence)
            .map(|custody| AgyAdoption {
                permit: None,
                custody,
            }),
    }
}

fn log_adoption(channel: u64, held_by: Custody, adoption: &AgyAdoption) {
    for held in &adoption.custody.held {
        tracing::warn!(
            channel,
            slot = %held.name,
            reason = ?held.reason,
            "[tui_o] agy shadow slot held"
        );
    }
    tracing::debug!(
        channel,
        ready = adoption.permit.is_some(),
        recovery = matches!(held_by, Custody::Recovery(_)),
        turns = adoption.custody.turns.len(),
        held = adoption.custody.held.len(),
        "[tui_o] agy shadow custody restored"
    );
}

#[cfg(all(test, unix))]
mod custody_tests;
