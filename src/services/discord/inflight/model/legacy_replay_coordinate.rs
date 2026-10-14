//! Durable coordinate origin and install state. Only the coordinate replay RMW changes the
//! marker; numeric offsets name the byte namespace it identifies.

use super::{Deserialize, InflightTurnState, Serialize};

/// Hold-reason namespace owned by the coordinate replay RMW; general writers never add or drop it.
pub(in crate::services::discord) const COORDINATE_HOLD_PREFIX: &str = "p5925_coordinate:";

pub(in crate::services::discord) fn is_coordinate_hold_reason(reason: &str) -> bool {
    reason.starts_with(COORDINATE_HOLD_PREFIX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum CoordinateInstallPhase {
    /// A proof and normalized file are being prepared; the prior source still names the offsets.
    Preparing,
    /// The reconstructed normalized source is published; offsets name its byte namespace.
    Installed,
}

/// Normalized coordinates the row named before replay. Never a raw seek position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct PriorNormalizedCoordinate {
    pub output_path: Option<String>,
    pub last_offset: u64,
    pub turn_start_offset: Option<u64>,
}

/// The raw rollout turn and composed-prefix boundary a proof verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct RawTurnWitness {
    pub path: String,
    pub file_identity: (u64, u64),
    pub session_id: String,
    pub native_turn_id: String,
    pub turn_start: u64,
    pub prefix_record_end: u64,
    pub prefix_len: usize,
}

/// The reconstructed normalized byte namespace an install published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct InstalledSourceWitness {
    pub output_path: String,
    pub file_identity: (u64, u64),
    pub generation_mtime_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct LegacyReplayCoordinate {
    pub version: u32,
    pub install_id: String,
    pub phase: CoordinateInstallPhase,
    pub prior: PriorNormalizedCoordinate,
    #[serde(default)]
    pub raw: Option<RawTurnWitness>,
    #[serde(default)]
    pub installed: Option<InstalledSourceWitness>,
}

/// Coordinate namespace captured when a watcher observes output, before its RMW lock.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct CoordinateSourceWitness(
    Option<(String, InstalledSourceWitness)>,
);

impl CoordinateSourceWitness {
    #[cfg(test)]
    pub(in crate::services::discord) fn of(state: &InflightTurnState) -> Self {
        Self(
            state
                .coordinate_source()
                .map(|(id, source)| (id.to_owned(), source.clone())),
        )
    }

    pub(in crate::services::discord) fn matches_state(&self, state: &InflightTurnState) -> bool {
        self.0.as_ref().map(|(id, source)| (id.as_str(), source)) == state.coordinate_source()
    }
}

#[cfg(test)]
impl LegacyReplayCoordinate {
    pub(in crate::services::discord) fn is_installed(&self) -> bool {
        self.phase == CoordinateInstallPhase::Installed && self.installed.is_some()
    }
}

impl InflightTurnState {
    /// The source incarnation whose byte namespace the offsets name, including during replacement
    /// preparation; `None` is the prior legacy source.
    pub(in crate::services::discord) fn coordinate_source(
        &self,
    ) -> Option<(&str, &InstalledSourceWitness)> {
        let marker = self.legacy_replay_coordinate.as_ref()?;
        marker
            .installed
            .as_ref()
            .map(|installed| (marker.install_id.as_str(), installed))
    }

    /// Offsets, watermarks and terminal flags of one source incarnation never merge into another.
    pub(in crate::services::discord) fn same_coordinate_source(&self, other: &Self) -> bool {
        self.coordinate_source() == other.coordinate_source()
    }

    fn coordinate_hold_reasons(&self) -> impl Iterator<Item = &String> {
        self.replay_hold_reasons
            .iter()
            .filter(|reason| is_coordinate_hold_reason(reason))
    }

    /// Whether two rows carry the same coordinate field group: install marker and namespaced holds.
    pub(in crate::services::discord) fn coordinate_group_eq(&self, other: &Self) -> bool {
        self.legacy_replay_coordinate == other.legacy_replay_coordinate
            && self
                .coordinate_hold_reasons()
                .eq(other.coordinate_hold_reasons())
    }

    pub(in crate::services::discord) fn has_coordinate_group(&self) -> bool {
        self.legacy_replay_coordinate.is_some() || self.coordinate_hold_reasons().next().is_some()
    }
}
