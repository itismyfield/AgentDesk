//! Coordinate recovery classification, holds, attempt tally and final watcher-claim recheck.
//! Owned rows cannot enter general completion or numeric-offset restoration in the same pass.

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight::{InflightEpisodePin, InflightTurnState};

#[cfg(unix)]
#[path = "legacy_restart_replay/install.rs"]
pub(super) mod install;
#[path = "legacy_restart_replay/proof.rs"]
pub(super) mod proof;
#[cfg(unix)]
#[path = "legacy_restart_replay/reader.rs"]
pub(super) mod reader;

#[cfg(all(test, unix))]
#[path = "legacy_restart_replay/reader_tests.rs"]
mod reader_tests;

/// The pinned source policy of the row's tmux source authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourcePolicy {
    Legacy,
    VerifiedCurrent,
    VerifiedUnavailable,
}

/// What a metadata probe of the stored normalized output found. Only `NotFound` is `Missing`;
/// any other I/O error stays `Unreadable` and is never read as an empty file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NormalizedOutputProbe {
    Present,
    Missing,
    Unreadable,
}

pub(super) fn probe_normalized_output(output_path: Option<&str>) -> NormalizedOutputProbe {
    let Some(path) = output_path.filter(|path| !path.trim().is_empty()) else {
        return NormalizedOutputProbe::Missing;
    };
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => NormalizedOutputProbe::Present,
        Ok(_) => NormalizedOutputProbe::Unreadable,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            NormalizedOutputProbe::Missing
        }
        Err(_) => NormalizedOutputProbe::Unreadable,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CoordinateRecoveryDisposition {
    NotApplicable,
    NeedsProof,
    RetryInstall,
    ResumeManagedSource,
    NeedsEvidence,
    RefusedByExistingAuthority,
}

/// Why a coordinate recovery stopped short of an install. Every variant keeps the delivery
/// obligation; none of them is a recovery, a drained backlog, an offset advance or a rerun.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CoordinateHold {
    /// Missing evidence the source may still produce: partial record, unwritten turn or prefix.
    AwaitEvidence(&'static str),
    /// A transient install-side failure; the same obligation resumes on the next idle pass.
    RetryInstall(&'static str),
    /// The stored identity or prefix cannot be proven from the source; never guessed.
    NeedsEvidence(&'static str),
}

const NEEDS_EVIDENCE_SLUGS: &[&str] = &[
    "native_turn_unknown",
    "session_unknown",
    "session_mismatch",
    "turns_unnamed",
    "ambiguous_turn",
    "malformed_record",
    "prefix_mismatch",
    "source_not_regular",
    "source_identity_unavailable",
    "terminal_turn_mismatch",
];

/// Decides whether coordinate recovery owns `state` before any general restore path runs.
pub(super) fn classify(
    state: &InflightTurnState,
    output: NormalizedOutputProbe,
    policy: SourcePolicy,
) -> CoordinateRecoveryDisposition {
    use CoordinateRecoveryDisposition as D;
    let marker = state.legacy_replay_coordinate.as_ref();
    let coordinate_holds: Vec<&str> = state
        .replay_hold_reasons
        .iter()
        .filter_map(|reason| {
            reason.strip_prefix(crate::services::discord::inflight::COORDINATE_HOLD_PREFIX)
        })
        .collect();
    let codex_tui = state.provider == "codex"
        && state.runtime_kind == Some(RuntimeHandoffKind::CodexTui)
        && state.tmux_session_name.is_some();
    let owned = marker.is_some()
        || !coordinate_holds.is_empty()
        || (codex_tui && output != NormalizedOutputProbe::Present);
    if !owned {
        return D::NotApplicable;
    }
    if !codex_tui
        || policy != SourcePolicy::Legacy
        || state.runtime_kind_unknown_on_disk
        || state.version > crate::services::discord::inflight::inflight_state_version()
        || state.herdr_before_start_stop.is_some()
        || state.tui_terminal_kind.is_some()
        || state.terminal_delivery_committed
        || state.rebind_origin
        || state.restart_mode.is_some()
    {
        return D::RefusedByExistingAuthority;
    }
    if marker.is_some_and(|marker| marker.is_installed()) {
        return match output {
            NormalizedOutputProbe::Present => D::ResumeManagedSource,
            _ => D::NeedsProof,
        };
    }
    if marker.is_some()
        || coordinate_holds
            .iter()
            .any(|slug| matches!(*slug, "pending" | "persist_failed"))
    {
        return D::RetryInstall;
    }
    let unnamed = |value: &Option<String>| value.as_deref().is_none_or(|v| v.trim().is_empty());
    if unnamed(&state.native_turn_id)
        || unnamed(&state.session_id)
        || unnamed(&state.turn_nonce)
        || coordinate_holds
            .iter()
            .any(|slug| NEEDS_EVIDENCE_SLUGS.contains(slug))
    {
        return D::NeedsEvidence;
    }
    D::NeedsProof
}

/// Counts replay attempts for startup/idle reporting. A held row is never a recovery and keeps
/// the backlog open.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReplayTally {
    pub(super) recovered: usize,
    pub(super) held: usize,
}

impl ReplayTally {
    pub(super) fn record(&mut self, attempt: Result<(), CoordinateHold>) {
        match attempt {
            Ok(()) => self.recovered += 1,
            Err(_) => self.held += 1,
        }
    }

    pub(super) fn backlog_drained(&self) -> bool {
        self.held == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClaimRecheck {
    Proceed,
    Refuse(&'static str),
}

/// Rechecks a lifecycle candidate before watcher claim; changed coordinate groups and newly
/// owned rows stay with coordinate recovery.
pub(super) fn recheck_before_claim(
    scanned: &InflightTurnState,
    durable_now: Option<&InflightTurnState>,
    output_now: NormalizedOutputProbe,
    policy: SourcePolicy,
) -> ClaimRecheck {
    let Some(durable) = durable_now else {
        return ClaimRecheck::Refuse("row_absent");
    };
    if !InflightEpisodePin::from_state(scanned).matches_state(durable) {
        return ClaimRecheck::Refuse("episode_changed");
    }
    if !scanned.coordinate_group_eq(durable) {
        return ClaimRecheck::Refuse("coordinate_changed");
    }
    if classify(durable, output_now, policy) != CoordinateRecoveryDisposition::NotApplicable {
        return ClaimRecheck::Refuse("coordinate_recovery_target");
    }
    ClaimRecheck::Proceed
}
