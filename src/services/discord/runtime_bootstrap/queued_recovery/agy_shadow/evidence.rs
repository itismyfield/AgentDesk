//! Evidence for one iMessage execution: the PG row proves the original and destination, and only
//! the inflight row names the nonce, so a PG row alone holds the slot.

use std::collections::BTreeMap;

use sqlx::PgPool;
use uuid::Uuid;

use crate::db::imsg_reply::Obligation;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::imsg_reply::{agy_input, episode};
use crate::services::provider::ProviderKind;
use crate::services::tui_o::store::agy_spool::replay::{AgyReplay, AgySlot, AgyTarget};
use crate::services::tui_o::writer::host::agy_host::{AgyEvidence, AgyEvidenceHold};

/// Each iMessage slot's PG admission row; `Err` when it could not be read.
pub(super) type AdmissionRows = BTreeMap<AgySlot, Result<Option<Obligation>, ()>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PgOriginal {
    Proven,
    Absent,
    Conflict,
}

pub(super) async fn admission_rows(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    destination: u64,
    turns: &[AgyReplay],
) -> AdmissionRows {
    let mut rows = AdmissionRows::new();
    for turn in turns {
        if !matches!(turn.slot, AgySlot::Imessage(_)) {
            continue;
        }
        let row = match pool {
            Some(pool) => {
                let episode = episode::turn(provider, destination, turn.key.id);
                crate::db::imsg_reply::read_agy_binding(
                    pool,
                    destination,
                    turn.key.id,
                    Some(episode.as_str()),
                )
                .await
                .map_err(|error| {
                    tracing::warn!(destination, %error, "[tui_o] agy admission row unreadable");
                })
            }
            None => Err(()),
        };
        rows.insert(turn.slot.clone(), row);
    }
    rows
}

pub(super) fn assemble(
    target: AgyTarget,
    turns: &[AgyReplay],
    inflight: &Result<Option<InflightTurnState>, String>,
    rows: &AdmissionRows,
) -> BTreeMap<AgySlot, AgyEvidence> {
    turns
        .iter()
        .filter_map(|turn| {
            let AgySlot::Imessage(digest) = &turn.slot else {
                return None;
            };
            let found = match (inflight, rows.get(&turn.slot)) {
                (Ok(inflight), Some(Ok(row))) => decide(
                    turn,
                    target,
                    digest,
                    inflight.as_ref(),
                    pg_original(row.as_ref(), target, digest),
                ),
                _ => AgyEvidence::Held(AgyEvidenceHold::Unverifiable),
            };
            Some((turn.slot.clone(), found))
        })
        .collect()
}

/// Inflight rows hold off adoption as prior Legacy work, except the exact execution they bind.
pub(super) fn prior_legacy_settled(
    inflight: &Result<Option<InflightTurnState>, String>,
    turns: &[AgyReplay],
    evidence: &BTreeMap<AgySlot, AgyEvidence>,
) -> bool {
    match inflight {
        Ok(None) => true,
        Ok(Some(row)) => turns.iter().any(|turn| {
            row.user_msg_id == turn.key.id
                && evidence.get(&turn.slot) == Some(&AgyEvidence::Bound(turn.key.clone()))
        }),
        Err(_) => false,
    }
}

fn pg_original(row: Option<&Obligation>, target: AgyTarget, digest: &str) -> PgOriginal {
    let Some(row) = row else {
        return PgOriginal::Absent;
    };
    match agy_input::proven_source(row, target.destination, target.logical_channel) {
        Some(source) if source.digest() == digest => PgOriginal::Proven,
        _ => PgOriginal::Conflict,
    }
}

fn decide(
    turn: &AgyReplay,
    target: AgyTarget,
    digest: &str,
    inflight: Option<&InflightTurnState>,
    pg: PgOriginal,
) -> AgyEvidence {
    let same_id = inflight
        .filter(|row| row.channel_id == target.destination && row.user_msg_id == turn.key.id);
    let bound = same_id.is_some_and(|row| {
        row.logical_channel_id == Some(target.logical_channel)
            && row
                .turn_nonce
                .as_deref()
                .and_then(|nonce| Uuid::parse_str(nonce).ok())
                == Some(turn.key.nonce)
            && row.agy_imessage_source_sha256.as_deref() == Some(digest)
    });
    match (same_id.is_some(), bound, pg) {
        (true, true, PgOriginal::Proven) => AgyEvidence::Bound(turn.key.clone()),
        (false, _, PgOriginal::Proven) => AgyEvidence::Held(AgyEvidenceHold::PgOnly),
        (false, _, PgOriginal::Absent) => AgyEvidence::Held(AgyEvidenceHold::Missing),
        _ => AgyEvidence::Held(AgyEvidenceHold::Conflict),
    }
}
