//! Operand assembly for the composed relay verdict — #5942.
//!
//! Split out of `super` (`health/snapshot.rs`) by #5942 r4. That file is a
//! registered giant (`scripts/giant_file_registry.toml`, `decision = "shrink"`,
//! #5447, deadline 2027-02-28) and r3 grew its production LoC by 94, which the
//! `giant_file_progress.py` no-regression gate rejects. Nothing here changed in
//! the move: the four items below are the ones `super` already called, with the
//! same bodies, and the `#[cfg(unix)]` gate they each carried is now on the
//! `mod` declaration instead.
//!
//! # Where `super` publishes what these operands decide
//!
//! The expiry these operands can produce surfaces on
//! `DiscordHealthSnapshot::expired_relay_ledgers`, beside `degraded_reasons`
//! rather than inside it, and `server::routes::health_api::public_health_json`
//! re-projects that vector onto the unauthenticated `/api/health` body. Expiry
//! must not be silent — the failure the field exists for is a set that GROWS —
//! but it must also not be counted into the degraded axis, because counting it
//! is the saturation #5942 reported.
//!
//! An empty vector is NOT the normal steady state. On the node that reported
//! #5942 three routine channels are stamped once a day and expire about ten
//! minutes after each run, so the vector is non-empty for roughly 23 of every
//! 24 hours. A reader must therefore look at the entry COUNT and at
//! `unobserved_for_secs` (an age past a day means the routine did not run at
//! all), never at emptiness. There is no such reader in this repo yet — #5947
//! tracks adding one or removing the field.

use crate::services::discord::relay_health::{RelayActiveTurn, RelayHealthSnapshot};
use crate::services::provider::ProviderKind;

use super::super::reachability::ledger::{ledger_file_exists, ledger_path};
use super::super::session_enrichment::ExecutorWitness;

pub(super) struct RelayVerdictProbeOperands {
    pub(super) pane_idle_confirmed: bool,
    pub(super) rowless_active_turn: bool,
    pub(super) placeholder_present: bool,
    /// #5942: three-valued execution-owner witness for the ledger TTL. Derived
    /// from the SAME observation `pane_idle_confirmed` is, so the two cannot
    /// disagree about whether a session was seen.
    pub(super) executor: ExecutorWitness,
    pub(super) now_epoch_ms: u64,
    pub(super) process_started_at_epoch_ms: u64,
}

/// Preserve the reachability probe's three-valued tail evidence. A missing
/// `unread_bytes` is not a proven drained tail for a row-backed turn; only the
/// separately witnessed rowless case may use pane-idle as positive evidence.
/// Do not collapse this to `unwrap_or(0) == 0`: the unmeasured-tail mutation is
/// pinned by `call_site_withholds_the_pane_idle_witness_for_an_unmeasured_tail`.
pub(super) fn reachability_ledger_operand_exists(provider: &ProviderKind, channel_id: u64) -> bool {
    ledger_path(provider, channel_id)
        .as_deref()
        .is_some_and(ledger_file_exists)
}

pub(super) fn relay_verdict_probe_operands(
    executor: ExecutorWitness,
    relay_health: &RelayHealthSnapshot,
    rowless_active_turn: bool,
    process_started_at_unix: i64,
) -> RelayVerdictProbeOperands {
    // #5942 r3 (P1-B): `Present` ONLY. `pane_idle_confirmed` is 4987 §-1.4's
    // second incarnation-alive witness, so widening this to "not Absent" would
    // let `Unwitnessed` — "we could not check" — satisfy the alive gate and
    // compose to `Reachable`. §7.2 forbids exactly that reading, and §1.5 names
    // it the root defect of the predecessor design. Pinned by
    // `an_unwitnessed_executor_never_confirms_the_pane_idle_witness`.
    let pane_alive = matches!(executor, ExecutorWitness::Present);
    RelayVerdictProbeOperands {
        pane_idle_confirmed: pane_alive
            && matches!(relay_health.active_turn, RelayActiveTurn::None)
            && relay_health.idle_witness_tail_is_not_waiting(),
        rowless_active_turn,
        executor,
        placeholder_present: relay_health.pending_discord_callback_msg_id.is_some(),
        now_epoch_ms: chrono::Utc::now().timestamp_millis().max(0) as u64,
        process_started_at_epoch_ms: process_started_at_unix.max(0).saturating_mul(1_000) as u64,
    }
}

/// The detail path's execution-owner witness (#5942 r2, P1-4).
///
/// **The two health paths do NOT share a probe, and this comment used to claim
/// they did.** They ask different questions on purpose and r2 left it that way:
///
/// * the AGGREGATE path (`build_health_snapshot_with_options`) probes
///   `tmux::session_presence` — "does the session exist" — because the bool it
///   also derives (`tmux_present`) has consumers all over `super` and in stall
///   recovery, and changing what THEY mean is not #5942's to do;
/// * the DETAIL path probes `tmux::pane_liveness` — "does the session have a
///   live pane" — which is what `tmux_session_alive` has always published.
///
/// The one property that matters for the ledger TTL is held on BOTH: a probe
/// that could not answer yields [`ExecutorWitness::Unwitnessed`] and therefore
/// cannot expire anything. `PaneLiveness::ProbeError` arrives here as `None`,
/// and `SessionPresence::ProbeFailed` becomes `Unwitnessed` in
/// `witness_tmux_session_within`.
///
/// They diverge in THREE places, each pinned by
/// `the_two_health_paths_agree_inside_the_probe_and_diverge_on_the_wedge_the_budget_and_the_blank_name`:
///
/// * the dead-pane wedge — a session that still exists with only dead panes.
///   The aggregate reads `Present` (blocks expiry), the detail reads `Absent`
///   (would expire);
/// * the shared probe budget. Only the aggregate path is charged against it, so
///   an exhausted budget withholds the aggregate witness while the detail one
///   answers normally;
/// * a BLANK session name (r4, P2-2). `tmux::session_presence` rejects it as
///   `ProbeFailed` → `Unwitnessed`, while `tmux::pane_liveness` rejects it as
///   `DeadOrAbsent` → `Some(false)` → `Absent` here. r3's doc claimed a third
///   divergence would fail the test; the test enumerated only two, so it did
///   not. It enumerates all three now.
///
/// Both err in the safe direction where it counts on the first two: the
/// aggregate is the path that decides `/api/health`'s `ok`, and there it is the
/// one that refuses to expire. The blank name is the exception and is harmless
/// for a different reason — the detail path publishes no expiry, it only
/// abstains from the warrant, so its stricter reading costs a warrant operand
/// rather than a ledger.
///
/// `(None, None)` is not a probe fault — `probe_tmux_session_alive` returns
/// `None` without probing when there is no session name — so it is a positive
/// absence, matching the aggregate path's rule for the same situation.
pub(super) fn detail_executor_witness(
    tmux_session_alive: Option<bool>,
    tmux_session: Option<&str>,
) -> ExecutorWitness {
    match (tmux_session_alive, tmux_session) {
        (Some(true), _) => ExecutorWitness::Present,
        (Some(false), _) | (None, None) => ExecutorWitness::Absent,
        (None, Some(_)) => ExecutorWitness::Unwitnessed,
    }
}
