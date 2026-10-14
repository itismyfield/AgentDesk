//! The single write authority for the legacy replay coordinate group: the install marker and the
//! `p5925_coordinate:` holds. General writers must keep that group as it is (see `store.rs`).

use super::*;
use crate::services::discord::inflight::model::{
    COORDINATE_HOLD_PREFIX, CoordinateInstallPhase, InstalledSourceWitness, LegacyReplayCoordinate,
    PriorNormalizedCoordinate, RawTurnWitness, is_coordinate_hold_reason,
};

const COORDINATE_MARKER_VERSION: u32 = 1;

pub(in crate::services::discord) use crate::services::discord::inflight::CoordinateSourceWitness;

/// The durable row a proof or plan was built from. A long scan runs without the lock, so every
/// field is checked again under it before any coordinate write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct CoordinateExpectation {
    save_generation: u64,
    turn_nonce: String,
    session_id: Option<String>,
    native_turn_id: Option<String>,
    composed_seed: String,
    source: CoordinateSourceWitness,
}

impl CoordinateExpectation {
    /// `None` when the row has no nonce: an unnamed episode is never replayed.
    pub(in crate::services::discord) fn of(state: &InflightTurnState) -> Option<Self> {
        let turn_nonce = state
            .turn_nonce
            .clone()
            .filter(|nonce| !nonce.trim().is_empty())?;
        Some(Self {
            save_generation: state.save_generation,
            turn_nonce,
            session_id: state.session_id.clone(),
            native_turn_id: state.native_turn_id.clone(),
            composed_seed: state.full_response.clone(),
            source: CoordinateSourceWitness::of(state),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum CoordinateChange {
    /// Make `slug` the row's only coordinate hold; holds of other namespaces stay.
    Hold(&'static str),
    /// Drop the coordinate holds. The install marker stays until the turn retires.
    Release,
    /// Persist pending before the proof scan begins outside the flock.
    BeginPrepare { install_id: String },
    /// Attach the proof to the exact Preparing snapshot the scan started from.
    AttachProof {
        install_id: String,
        raw: RawTurnWitness,
    },
    /// Record an already verified proof while preparing its replacement source.
    Prepare {
        install_id: String,
        raw: RawTurnWitness,
    },
    /// Move the offsets into the published normalized namespace of the prepared install.
    Install {
        install_id: String,
        installed: InstalledSourceWitness,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum CoordinateRefusal {
    NonceMismatch,
    StaleSnapshot,
    SeedChanged,
    SourceChanged,
    TerminalCommitted,
    ExistingAuthority,
    PhaseOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum CoordinateRmwOutcome {
    Saved,
    Refused(CoordinateRefusal),
    Write(GuardedSaveOutcome),
}

fn check_expectation(
    durable: &InflightTurnState,
    expected: &CoordinateExpectation,
) -> Result<(), CoordinateRefusal> {
    if durable.turn_nonce.as_deref() != Some(expected.turn_nonce.as_str()) {
        return Err(CoordinateRefusal::NonceMismatch);
    }
    if durable.terminal_delivery_committed {
        return Err(CoordinateRefusal::TerminalCommitted);
    }
    if durable.rebind_origin || durable.restart_mode.is_some() {
        return Err(CoordinateRefusal::ExistingAuthority);
    }
    if durable.save_generation != expected.save_generation {
        return Err(CoordinateRefusal::StaleSnapshot);
    }
    if durable.session_id != expected.session_id
        || durable.native_turn_id != expected.native_turn_id
        || durable.full_response != expected.composed_seed
    {
        return Err(CoordinateRefusal::SeedChanged);
    }
    admit_source_patch(durable, &expected.source)
}

/// A watcher stream/terminal/watermark patch applies only to the coordinate source it was
/// produced against; a patch from a replaced source must not move the new source's state.
pub(in crate::services::discord) fn admit_source_patch(
    durable: &InflightTurnState,
    patch_source: &CoordinateSourceWitness,
) -> Result<(), CoordinateRefusal> {
    if patch_source.matches_state(durable) {
        Ok(())
    } else {
        Err(CoordinateRefusal::SourceChanged)
    }
}

fn set_coordinate_hold(state: &mut InflightTurnState, slug: Option<&str>) {
    state
        .replay_hold_reasons
        .retain(|reason| !is_coordinate_hold_reason(reason));
    if let Some(slug) = slug {
        state
            .replay_hold_reasons
            .push(format!("{COORDINATE_HOLD_PREFIX}{slug}"));
    }
}

/// Applies one coordinate change to the exact episode `locked` holds, after re-checking the
/// snapshot the caller planned from. `locked` adopts the persisted row on success.
pub(in crate::services::discord) fn apply_coordinate_change(
    locked: &mut LockedInflightEpisode,
    expected: &CoordinateExpectation,
    change: CoordinateChange,
) -> CoordinateRmwOutcome {
    let durable = locked.state();
    if let Err(refusal) = check_expectation(durable, expected) {
        return CoordinateRmwOutcome::Refused(refusal);
    }
    let mut next = durable.clone();
    let source_rebase = match change {
        CoordinateChange::Hold(slug) => {
            set_coordinate_hold(&mut next, Some(slug));
            false
        }
        CoordinateChange::Release => {
            set_coordinate_hold(&mut next, None);
            false
        }
        CoordinateChange::BeginPrepare { install_id } => {
            prepare_marker(&mut next, durable, install_id, None);
            false
        }
        CoordinateChange::Prepare { install_id, raw } => {
            if !proof_matches(durable, &raw) {
                return CoordinateRmwOutcome::Refused(CoordinateRefusal::SeedChanged);
            }
            prepare_marker(&mut next, durable, install_id, Some(raw));
            false
        }
        CoordinateChange::AttachProof { install_id, raw } => {
            if !proof_matches(durable, &raw) {
                return CoordinateRmwOutcome::Refused(CoordinateRefusal::SeedChanged);
            }
            let Some(marker) = next.legacy_replay_coordinate.as_mut().filter(|marker| {
                marker.phase == CoordinateInstallPhase::Preparing && marker.install_id == install_id
            }) else {
                return CoordinateRmwOutcome::Refused(CoordinateRefusal::PhaseOrder);
            };
            marker.raw = Some(raw);
            false
        }
        CoordinateChange::Install {
            install_id,
            installed,
        } => {
            let Some(marker) = next.legacy_replay_coordinate.as_mut().filter(|marker| {
                marker.phase == CoordinateInstallPhase::Preparing
                    && marker.install_id == install_id
                    && marker.raw.is_some()
            }) else {
                return CoordinateRmwOutcome::Refused(CoordinateRefusal::PhaseOrder);
            };
            marker.phase = CoordinateInstallPhase::Installed;
            next.output_path = Some(installed.output_path.clone());
            marker.installed = Some(installed);
            next.last_offset = 0;
            next.turn_start_offset = Some(0);
            next.last_watcher_relayed_offset = None;
            next.last_watcher_relayed_generation_mtime_ns = None;
            true
        }
    };
    match locked.persist_coordinate_change_under_guard(
        &next,
        source_rebase,
        "src/services/discord/inflight/save_store/coordinate_replay.rs:apply_coordinate_change",
    ) {
        GuardedSaveOutcome::Saved => CoordinateRmwOutcome::Saved,
        other => CoordinateRmwOutcome::Write(other),
    }
}

fn proof_matches(state: &InflightTurnState, raw: &RawTurnWitness) -> bool {
    state.session_id.as_deref() == Some(raw.session_id.as_str())
        && state.native_turn_id.as_deref() == Some(raw.native_turn_id.as_str())
        && raw.prefix_len == state.full_response.len()
        && raw.prefix_record_end > raw.turn_start
}

fn prepare_marker(
    next: &mut InflightTurnState,
    durable: &InflightTurnState,
    install_id: String,
    raw: Option<RawTurnWitness>,
) {
    let prior = durable.legacy_replay_coordinate.as_ref().map_or_else(
        || PriorNormalizedCoordinate {
            output_path: durable.output_path.clone(),
            last_offset: durable.last_offset,
            turn_start_offset: durable.turn_start_offset,
        },
        |marker| marker.prior.clone(),
    );
    next.legacy_replay_coordinate = Some(LegacyReplayCoordinate {
        version: COORDINATE_MARKER_VERSION,
        install_id,
        phase: CoordinateInstallPhase::Preparing,
        prior,
        raw,
        installed: durable
            .legacy_replay_coordinate
            .as_ref()
            .and_then(|m| m.installed.clone()),
    });
    set_coordinate_hold(next, Some("pending"));
}

impl LockedInflightEpisode {
    pub(in crate::services::discord) fn begin_coordinate_preparation(
        &mut self,
        snapshot: &InflightTurnState,
        install_id: String,
    ) -> GuardedSaveOutcome {
        self.change_coordinate_snapshot(snapshot, CoordinateChange::BeginPrepare { install_id })
    }

    pub(in crate::services::discord) fn attach_coordinate_proof(
        &mut self,
        snapshot: &InflightTurnState,
        install_id: String,
        raw: RawTurnWitness,
    ) -> GuardedSaveOutcome {
        self.change_coordinate_snapshot(snapshot, CoordinateChange::AttachProof { install_id, raw })
    }

    pub(in crate::services::discord) fn install_coordinate_source(
        &mut self,
        snapshot: &InflightTurnState,
        install_id: String,
        output_path: String,
        file_identity: (u64, u64),
        generation_mtime_ns: i64,
    ) -> GuardedSaveOutcome {
        let installed = InstalledSourceWitness {
            output_path,
            file_identity,
            generation_mtime_ns,
        };
        self.change_coordinate_snapshot(
            snapshot,
            CoordinateChange::Install {
                install_id,
                installed,
            },
        )
    }

    pub(in crate::services::discord) fn release_coordinate_hold(
        &mut self,
        snapshot: &InflightTurnState,
    ) -> GuardedSaveOutcome {
        self.change_coordinate_snapshot(snapshot, CoordinateChange::Release)
    }

    fn change_coordinate_snapshot(
        &mut self,
        snapshot: &InflightTurnState,
        change: CoordinateChange,
    ) -> GuardedSaveOutcome {
        let Some(expected) = CoordinateExpectation::of(snapshot) else {
            return GuardedSaveOutcome::Unnameable;
        };
        match apply_coordinate_change(self, &expected, change) {
            CoordinateRmwOutcome::Saved => GuardedSaveOutcome::Saved,
            CoordinateRmwOutcome::Refused(_) => GuardedSaveOutcome::AuthorityPinned,
            CoordinateRmwOutcome::Write(outcome) => outcome,
        }
    }
}
