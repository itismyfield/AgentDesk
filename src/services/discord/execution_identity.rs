//! Execution-identity read model for the #5071 T3 identity/fence series.
//!
//! This module is a READ MODEL: the authority for every destruction decision
//! lives downstream (T3-A1 and later), and this module only observes and
//! compares. Nothing here cancels a watcher, kills a tmux session or a process,
//! removes a registry entry, or writes any marker.
//!
//! The only backing store is the per-spawn `.spawn_nonce` marker that
//! `tmux_session_files` already writes at each provider spawn. This slice adds
//! no durable row, no new marker file, and no new file format — it reuses
//! `read_spawn_nonce` verbatim. The `.generation` mtime identity conjunct keeps
//! its existing reader (`read_generation_file_mtime_ns`) and is deliberately
//! NOT folded in here: the T3 read model is `(tmux_session_name, spawn_nonce?)`
//! only.
//!
//! Because T3-A0 lands with no destructive consumer, the items below have no
//! production caller yet; `ExecutionIdentityMode::Enforce` therefore changes
//! nothing at runtime. T3-A1 is the slice that converts real call sites.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::ExecutionIdentityMode;

use super::tmux_session_files::read_spawn_nonce;

/// One tmux session's execution identity as the on-disk markers describe it.
///
/// `spawn_nonce` is `None` whenever the marker is missing, unreadable, or empty
/// — the same three cases `read_spawn_nonce` folds together. A `None` is an
/// absence of evidence and is never widened into a wildcard match; see
/// [`compare_spawn_nonce`].
#[allow(dead_code)] // #5071 T3-A0: read model lands before its T3-A1 call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct SessionIncarnationRef {
    tmux_session_name: String,
    spawn_nonce: Option<String>,
}

/// The outcome of comparing a captured incarnation against the live one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum IncarnationObservation {
    /// Both sides carry a readable nonce and the two are byte-equal.
    Match,
    /// Both sides carry a readable nonce and the two differ: the session name
    /// now denotes a DIFFERENT spawn than the one that was captured.
    Mismatch,
    /// At least one side has no readable nonce, so the two spawns cannot be
    /// related at all. This is distinct from `Match`: an `Enforce` consumer must
    /// treat it as a deny, not as an identity proof.
    Unknown,
}

/// Compare a captured spawn nonce against the one read at observation time.
///
/// A readable nonce on BOTH sides is the only way to reach `Match`. Either side
/// being `None` yields `Unknown`, so a missing marker can never be mistaken for
/// proof that the captured spawn is still the live one — that widening is
/// exactly the `#{name}#0` collision `.spawn_nonce` was introduced to avoid.
#[allow(dead_code)] // #5071 T3-A0: read model lands before its T3-A1 call sites.
pub(in crate::services::discord) fn compare_spawn_nonce(
    captured: Option<&str>,
    current: Option<&str>,
) -> IncarnationObservation {
    match (captured, current) {
        (Some(captured), Some(current)) if captured == current => IncarnationObservation::Match,
        (Some(_), Some(_)) => IncarnationObservation::Mismatch,
        _ => IncarnationObservation::Unknown,
    }
}

#[allow(dead_code)] // #5071 T3-A0: read model lands before its T3-A1 call sites.
impl SessionIncarnationRef {
    /// Capture the identity of `tmux_session_name` by reading the existing
    /// `.spawn_nonce` marker once. Reading is the whole operation: no marker is
    /// created, refreshed, or removed, so capturing is safe on any path.
    pub(in crate::services::discord) fn capture(tmux_session_name: &str) -> Self {
        Self {
            spawn_nonce: read_spawn_nonce(tmux_session_name),
            tmux_session_name: tmux_session_name.to_string(),
        }
    }

    /// Build a ref from an already-read nonce, for callers that captured the
    /// marker earlier in their own flow and only need the comparison.
    pub(in crate::services::discord) fn from_parts(
        tmux_session_name: &str,
        spawn_nonce: Option<String>,
    ) -> Self {
        Self {
            tmux_session_name: tmux_session_name.to_string(),
            spawn_nonce,
        }
    }

    pub(in crate::services::discord) fn tmux_session_name(&self) -> &str {
        &self.tmux_session_name
    }

    pub(in crate::services::discord) fn spawn_nonce(&self) -> Option<&str> {
        self.spawn_nonce.as_deref()
    }

    /// Re-read the marker for the same session name and compare it against the
    /// captured value.
    ///
    /// There is no shared lock between this read and a concurrent spawn's
    /// rename, so the answer describes the moment of the read and not a
    /// linearizable window; the design records that limit as a non-guarantee
    /// rather than papering over it with a retry loop.
    pub(in crate::services::discord) fn observe_current(&self) -> IncarnationObservation {
        let current = read_spawn_nonce(&self.tmux_session_name);
        compare_spawn_nonce(self.spawn_nonce.as_deref(), current.as_deref())
    }
}

#[derive(Debug)]
struct IncarnationObservationCounters {
    matched: AtomicU64,
    mismatched: AtomicU64,
    unknown: AtomicU64,
}

impl Default for IncarnationObservationCounters {
    fn default() -> Self {
        Self {
            matched: AtomicU64::new(0),
            mismatched: AtomicU64::new(0),
            unknown: AtomicU64::new(0),
        }
    }
}

static OBSERVATION_COUNTERS: OnceLock<IncarnationObservationCounters> = OnceLock::new();

fn counters() -> &'static IncarnationObservationCounters {
    OBSERVATION_COUNTERS.get_or_init(IncarnationObservationCounters::default)
}

/// Count and log one captured-vs-current comparison.
///
/// `Legacy` records nothing, matching the switch semantics where the nonce is
/// not consulted at all. `Observe` and `Enforce` both count; neither decides
/// anything here, because this module has no destructive branch to gate.
#[allow(dead_code)] // #5071 T3-A0: read model lands before its T3-A1 call sites.
pub(in crate::services::discord) fn record_incarnation_observation(
    mode: ExecutionIdentityMode,
    site: &'static str,
    session_ref: &SessionIncarnationRef,
    observed: IncarnationObservation,
) {
    if !mode.records_identity_observations() {
        return;
    }
    let session_key = session_ref.tmux_session_name();
    match observed {
        IncarnationObservation::Match => {
            counters().matched.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                counter = "execution_identity_nonce_match",
                site,
                session_key,
                "captured spawn nonce still matches the live marker"
            );
        }
        IncarnationObservation::Mismatch => {
            counters().mismatched.fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                counter = "execution_identity_nonce_mismatch",
                site,
                session_key,
                "spawn nonce changed under the same tmux session name"
            );
        }
        IncarnationObservation::Unknown => {
            counters().unknown.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                counter = "execution_identity_nonce_unknown",
                site,
                session_key,
                "no readable spawn nonce on one side; identity is unproven"
            );
        }
    }
}

#[cfg(test)]
fn observation_counts() -> (u64, u64, u64) {
    (
        counters().matched.load(Ordering::Relaxed),
        counters().mismatched.load(Ordering::Relaxed),
        counters().unknown.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::tmux::write_spawn_nonce;

    fn isolated_runtime_root() -> (tempfile::TempDir, crate::config::TestEnvVarGuard) {
        let root = tempfile::tempdir().expect("isolated runtime root");
        let env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        (root, env)
    }

    fn unique_session(label: &str) -> String {
        format!("AgentDesk-5071-{label}-{}", uuid::Uuid::new_v4().simple())
    }

    /// Fixed mutation gate (a): rewriting the absent-nonce arm of
    /// `compare_spawn_nonce` to `IncarnationObservation::Match` must fail here.
    /// Absence of evidence is not evidence of the same incarnation, so every
    /// shape that lacks a readable nonce on either side stays `Unknown`.
    #[test]
    fn absent_spawn_nonce_is_never_observed_as_a_match() {
        assert_eq!(
            compare_spawn_nonce(None, None),
            IncarnationObservation::Unknown
        );
        assert_eq!(
            compare_spawn_nonce(Some("a1b2"), None),
            IncarnationObservation::Unknown
        );
        assert_eq!(
            compare_spawn_nonce(None, Some("a1b2")),
            IncarnationObservation::Unknown
        );

        let (_root, _env) = isolated_runtime_root();
        let session = unique_session("absent-nonce");
        let captured = SessionIncarnationRef::capture(&session);
        assert_eq!(captured.spawn_nonce(), None);
        assert_eq!(captured.tmux_session_name(), session);
        assert_eq!(captured.observe_current(), IncarnationObservation::Unknown);

        // A nonce appearing only AFTER the capture is still not a match: the
        // captured side has nothing to compare against.
        write_spawn_nonce(&session).expect("write nonce after capture");
        assert_eq!(captured.observe_current(), IncarnationObservation::Unknown);
    }

    /// Fixed mutation gate (b): deleting the `captured == current` comparison in
    /// `compare_spawn_nonce` (so any two readable nonces report `Match`) must
    /// fail here. A respawn reuses the tmux session NAME, so the nonce is the
    /// only thing that separates the two incarnations.
    #[test]
    fn spawn_nonce_mismatch_under_the_same_session_name_is_observed_as_mismatch() {
        assert_eq!(
            compare_spawn_nonce(Some("a1b2"), Some("a1b2")),
            IncarnationObservation::Match
        );
        assert_eq!(
            compare_spawn_nonce(Some("a1b2"), Some("c3d4")),
            IncarnationObservation::Mismatch
        );

        let (_root, _env) = isolated_runtime_root();
        let session = unique_session("respawn-nonce");
        let first = write_spawn_nonce(&session).expect("first spawn nonce");
        let captured = SessionIncarnationRef::capture(&session);
        assert_eq!(captured.spawn_nonce(), Some(first.as_str()));
        assert_eq!(captured.observe_current(), IncarnationObservation::Match);

        let second = write_spawn_nonce(&session).expect("respawn nonce");
        assert_ne!(first, second, "each spawn mints a fresh nonce");
        assert_eq!(captured.observe_current(), IncarnationObservation::Mismatch);
    }

    /// `Legacy` does not consult the nonce, so it must not move any counter;
    /// `Observe` and `Enforce` both record. Counters are process-global, so this
    /// single test owns all three of them and asserts deltas.
    #[test]
    fn observation_counters_record_only_outside_legacy_mode() {
        let session_ref = SessionIncarnationRef::from_parts("counter-probe", Some("a1b2".into()));
        let (matched_before, mismatched_before, unknown_before) = observation_counts();

        for observed in [
            IncarnationObservation::Match,
            IncarnationObservation::Mismatch,
            IncarnationObservation::Unknown,
        ] {
            record_incarnation_observation(
                ExecutionIdentityMode::Legacy,
                "t3a0_test",
                &session_ref,
                observed,
            );
        }
        assert_eq!(
            observation_counts(),
            (matched_before, mismatched_before, unknown_before),
            "Legacy must leave every observation counter untouched"
        );

        for mode in [
            ExecutionIdentityMode::Observe,
            ExecutionIdentityMode::Enforce,
        ] {
            for observed in [
                IncarnationObservation::Match,
                IncarnationObservation::Mismatch,
                IncarnationObservation::Unknown,
            ] {
                record_incarnation_observation(mode, "t3a0_test", &session_ref, observed);
            }
        }
        assert_eq!(
            observation_counts(),
            (
                matched_before + 2,
                mismatched_before + 2,
                unknown_before + 2
            ),
            "Observe and Enforce must each record one of every outcome"
        );
    }

    /// T3-A0 lands the read model with no destructive consumer. The compiled-in
    /// default stays `Legacy`, so an untouched config observes nothing, and
    /// `Enforce` is the only mode that may ever deny — which it cannot do in
    /// this slice because no call site reads the predicate yet.
    #[test]
    fn execution_identity_modes_keep_a0_non_destructive() {
        assert_eq!(
            ExecutionIdentityMode::default(),
            ExecutionIdentityMode::Legacy
        );
        assert!(!ExecutionIdentityMode::Legacy.records_identity_observations());
        assert!(ExecutionIdentityMode::Observe.records_identity_observations());
        assert!(ExecutionIdentityMode::Enforce.records_identity_observations());
        assert!(!ExecutionIdentityMode::Legacy.denies_on_incarnation_mismatch());
        assert!(!ExecutionIdentityMode::Observe.denies_on_incarnation_mismatch());
        assert!(ExecutionIdentityMode::Enforce.denies_on_incarnation_mismatch());
    }
}
